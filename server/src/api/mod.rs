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
