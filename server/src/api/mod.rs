// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

mod alerts;
mod assets;
mod dashboards;
mod home;
mod logs;
mod metrics;
mod notebooks;
mod prom;

use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::app::Shared;

pub(crate) struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // ClickHouse down or failing: 502 (not the request's fault); anything
        // else is invalid input
        let status = if self.0.chain().any(prom::unavailable) {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({ "error": format!("{:#}", self.0) }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

pub(crate) fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Language of server-generated texts: en (default) or es
pub(crate) fn lang_of(p: &Option<String>) -> &'static str {
    match p.as_deref() {
        Some(l) if l.starts_with("es") => "es",
        _ => "en",
    }
}

pub fn router(app: Shared) -> Router {
    Router::new()
        .merge(assets::routes())
        .merge(metrics::routes())
        .merge(logs::routes())
        .merge(prom::routes())
        .merge(home::routes())
        .merge(alerts::routes())
        .merge(notebooks::routes())
        .merge(dashboards::routes())
        // Liveness (for restarts) and readiness (ClickHouse and alerts)
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        // gzip/brotli when the client accepts it: series and log JSON
        // compresses 5-10x
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(app)
}

/// Whether the server can serve: every store it reads answers (VictoriaMetrics or ClickHouse for
/// metrics, VictoriaLogs or ClickHouse for logs) and alerts are evaluated. 503 if one does not
/// answer; the age of the last alert evaluation is in the body for whoever monitors it. The route
/// needs no session, so the body says only ok or down per store: why one failed (its address,
/// version or "authentication failed") goes to the log.
async fn readyz(axum::extract::State(app): axum::extract::State<Shared>) -> Response {
    let victoria = matches!(app.promql, atalaya::promql::Backend::Victoria(_));
    let uses_ch = !victoria || app.vlogs.is_none();
    let now = super::app::unix_now();
    let metrics = async {
        if victoria {
            Some(app.promql.instant("vector(1)", now).await.map(|_| ()))
        } else {
            None
        }
    };
    let logs = async {
        match &app.vlogs {
            Some(vl) => Some(vl.count("_time:1m").await.map(|_| ())),
            None => None,
        }
    };
    let ch = async {
        if uses_ch {
            Some(app.ch.text("SELECT 1", &[], &[]).await.map(|_| ()))
        } else {
            None
        }
    };
    let (metrics, logs, ch) = tokio::join!(metrics, logs, ch);
    let mut body = serde_json::Map::new();
    let mut ok = true;
    for (name, r) in [("victoriametrics", metrics), ("victorialogs", logs), ("clickhouse", ch)] {
        let Some(r) = r else { continue };
        if let Err(e) = &r {
            tracing::warn!("readyz: {name}: {e:#}");
        }
        ok &= r.is_ok();
        body.insert(name.into(), json!(if r.is_ok() { "ok" } else { "down" }));
    }
    let last_eval = app.alerts.last_tick();
    let age = last_eval.map(|t| super::app::unix_now() - t);
    body.insert("alerts_last_eval".into(), json!(last_eval));
    body.insert("alerts_age_s".into(), json!(age));
    let body = Value::Object(body);
    let status = if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{body::Bytes, http::Uri, routing::post};

    use super::*;
    use crate::app::App;

    type Seen = Arc<Mutex<Vec<String>>>;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A ClickHouse without rows that keeps the SQL of every query
    async fn recording_clickhouse() -> (String, Seen) {
        let seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().fallback(post(move |body: Bytes| {
            let log = log.clone();
            async move {
                let sql = String::from_utf8_lossy(&body).into_owned();
                // The engine reads samples as RowBinary, where no bytes is no rows; the rest is JSON
                let binary = sql.contains("FORMAT RowBinary");
                log.lock().unwrap().push(sql);
                if binary { Vec::new() } else { br#"{"data":[]}"#.to_vec() }
            }
        }));
        (format!("{}/", serve(app).await), seen)
    }

    /// A VictoriaMetrics without series that keeps the path of every request
    async fn recording_victoria() -> (String, Seen) {
        let seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().fallback(move |uri: Uri| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(uri.path().to_owned());
                let data = match uri.path() {
                    "/api/v1/query" => json!({ "resultType": "vector", "result": [] }),
                    "/api/v1/query_range" => json!({ "resultType": "matrix", "result": [] }),
                    _ => json!([]),
                };
                Json(json!({ "status": "success", "data": data }))
            }
        });
        (serve(app).await, seen)
    }

    const RULES: &str = r#"
channels: { default: { type: log } }
rules:
  - { id: target-down, title: Target down, expr: 'up == 0', summary: s }
  - { id: app-errors, title: Errors, kind: logs, filters: ["level:ERROR"], threshold: 20, summary: s }
"#;

    fn app(promql: atalaya::promql::Backend, ch: &str, name: &str) -> Shared {
        let dir = std::env::temp_dir().join(format!("atalaya-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alerts.yaml"), RULES).unwrap();
        Arc::new(App {
            promql,
            vlogs: None,
            ch: atalaya::ch::ClickHouse::new(ch.to_owned(), "u".into(), String::new()),
            db: "atalaya".into(),
            dashboards: dir.join("dashboards"),
            overview: dir.join("overview.yaml"),
            timeline: dir.join("timeline.yaml"),
            data: dir.clone(),
            alerts: crate::alerts::Alerts::new(dir.join("alerts.yaml"), &dir),
            overview_cache: Default::default(),
        })
    }

    /// Every API that reads metrics or logs (the UI's and Grafana's), readiness, and one evaluation
    /// of an alert rule of each kind. Returns the requests that did not answer 200
    async fn exercise(app: &Shared) -> Vec<String> {
        app.alerts.tick(app).await;
        let base = serve(router(app.clone())).await;
        let http = reqwest::Client::new();
        let end = crate::app::unix_now() - 120;
        let (s, e) = ((end - 3600).to_string(), end.to_string());
        let gets: Vec<(&str, Vec<(&str, String)>)> = vec![
            ("/api/v1/metricDescriptors", vec![("q", "up".into())]),
            ("/api/v1/metricDescriptors", vec![("brief", "1".into())]),
            ("/api/v1/labels", vec![("metric", "up".into())]),
            ("/api/v1/query", vec![("query", "up".into()), ("time", e.clone())]),
            (
                "/api/v1/query_range",
                vec![
                    ("query", "rate(up[5m])".into()),
                    ("start", s.clone()),
                    ("end", e.clone()),
                    ("step", "60".into()),
                ],
            ),
            ("/api/v1/series", vec![("match[]", "up".into())]),
            ("/api/v1/label/job/values", vec![]),
            ("/api/v1/overview", vec![("range", "1h".into())]),
            ("/api/v1/events", vec![("range", "1h".into())]),
            ("/api/v1/logs", vec![("range", "1h".into())]),
            ("/api/v1/logs/fields", vec![("range", "1h".into())]),
            ("/api/v1/logs/counts", vec![("range", "1h".into()), ("by", "service".into())]),
            ("/api/v1/logs/values", vec![("range", "1h".into()), ("field", "service".into())]),
            ("/api/v1/logs/context", vec![("service", "x".into()), ("ts", (end * 1000).to_string())]),
            ("/api/v1/alerts", vec![]),
            ("/readyz", vec![]),
        ];
        let mut failed = Vec::new();
        for (path, q) in gets {
            let r = http.get(format!("{base}{path}")).query(&q).send().await.unwrap();
            if !r.status().is_success() {
                failed.push(format!("GET {path}: {} {}", r.status(), r.text().await.unwrap_or_default()));
            }
        }
        let posts = [
            ("/api/v1/timeSeries:query", json!({ "metric": "up", "interval": { "range": "1h" } })),
            (
                "/api/v1/query_range:batch",
                json!({ "queries": [{ "query": "up", "start": end - 3600, "end": end, "step": 60 }] }),
            ),
            ("/api/v1/alerts/target-down/test", json!({})),
            ("/api/v1/alerts/app-errors/test", json!({})),
        ];
        for (path, body) in posts {
            let r = http.post(format!("{base}{path}")).json(&body).send().await.unwrap();
            if !r.status().is_success() {
                failed.push(format!("POST {path}: {} {}", r.status(), r.text().await.unwrap_or_default()));
            }
        }
        failed
    }

    /// Tables of the atalaya database the queries name, other than logs
    fn metric_tables(sql: &[String]) -> std::collections::BTreeSet<String> {
        let re = regex::Regex::new(r"\batalaya\.(\w+)").unwrap();
        sql.iter()
            .flat_map(|s| re.captures_iter(s).map(|c| c[1].to_owned()).collect::<Vec<_>>())
            .filter(|t| t != "logs")
            .collect()
    }

    /// With metrics on VictoriaMetrics, ClickHouse holds only the logs: no API, page or alert rule
    /// may read a metric table there, or dropping them after the move would break it
    #[tokio::test]
    async fn metrics_on_victoria_never_read_clickhouse_metric_tables() {
        let (ch_url, ch_seen) = recording_clickhouse().await;
        let (vm_url, vm_seen) = recording_victoria().await;
        let vm = atalaya::promql::victoria::VictoriaMetrics::new(&vm_url).unwrap();
        let app = app(atalaya::promql::Backend::Victoria(vm), &ch_url, "victoria");
        let failed = exercise(&app).await;
        assert!(failed.is_empty(), "every request is served: {failed:#?}");
        let sql = ch_seen.lock().unwrap().clone();
        assert_eq!(metric_tables(&sql), Default::default(), "{sql:#?}");
        // ClickHouse was asked, for the logs and readiness, and the metrics went to VictoriaMetrics
        assert!(sql.iter().any(|s| s.contains("atalaya.logs")));
        assert!(sql.iter().any(|s| s.trim() == "SELECT 1"));
        let paths = vm_seen.lock().unwrap().clone();
        for p in ["/api/v1/query", "/api/v1/query_range", "/api/v1/series", "/api/v1/label/job/values"] {
            assert!(paths.iter().any(|x| x == p), "{p} not asked: {paths:?}");
        }
    }

    /// The same requests on atalaya's engine do read the metric tables: the check above would see them
    #[tokio::test]
    async fn metrics_on_clickhouse_read_its_metric_tables() {
        let (ch_url, ch_seen) = recording_clickhouse().await;
        let ch = atalaya::ch::ClickHouse::new(ch_url.clone(), "u".into(), String::new());
        let engine = atalaya::promql::Engine::new(atalaya::promql::store::Store::new(ch, "atalaya".into()));
        let app = app(atalaya::promql::Backend::Native(Box::new(engine)), &ch_url, "native");
        exercise(&app).await;
        let tables = metric_tables(&ch_seen.lock().unwrap());
        for t in ["series", "metric_descriptors"] {
            assert!(tables.contains(t), "{t} not read: {tables:?}");
        }
    }
}
