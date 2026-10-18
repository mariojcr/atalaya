// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Prometheus HTTP API (PromQL) for dashboards.

use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use atalaya::promql::{self, Output};

use super::unix_now;
use crate::app::Shared;
use crate::query;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/query_range", get(prom_query_range).post(prom_query_range))
        .route("/api/v1/query_range:batch", post(prom_query_range_batch))
        .route("/api/v1/query", get(prom_query).post(prom_query))
        .route("/api/v1/label/{name}/values", get(prom_label_values))
        .route("/api/v1/series", get(prom_series).post(prom_series))
}

/// Error in Prometheus format (Grafana shows it as is)
struct PromError(anyhow::Error);

impl IntoResponse for PromError {
    fn into_response(self) -> Response {
        // Like Prometheus: bad_data (400) for the query, unavailable when
        // ClickHouse fails (502 rather than 503: the storage is another service)
        let down = self.0.chain().any(|e| e.is::<atalaya::ch::Unavailable>());
        let (status, kind) = if down {
            (StatusCode::BAD_GATEWAY, "unavailable")
        } else {
            (StatusCode::BAD_REQUEST, "bad_data")
        };
        let body = json!({ "status": "error", "errorType": kind, "error": format!("{:#}", self.0) });
        (status, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for PromError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

/// URL parameters plus, on a POST, form parameters; match[] repeats
struct PromParams(Vec<(String, String)>);

impl PromParams {
    fn new(query: Option<String>, body: &str) -> Self {
        let mut v: Vec<(String, String)> = Vec::new();
        for src in [query.as_deref().unwrap_or(""), body] {
            v.extend(form_urlencoded::parse(src.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())));
        }
        Self(v)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(n, _)| n == k).map(|(_, v)| v.as_str()).collect()
    }
    fn query(&self) -> anyhow::Result<&str> {
        self.get("query")
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing query"))
    }
    /// Unix seconds (with decimals) or RFC 3339
    fn time(&self, k: &str, default: i64) -> anyhow::Result<i64> {
        match self.get(k) {
            None | Some("") => Ok(default),
            Some(v) => {
                if let Ok(f) = v.parse::<f64>() {
                    Ok(f.floor() as i64)
                } else {
                    Ok(chrono::DateTime::parse_from_rfc3339(v)
                        .map_err(|_| anyhow::anyhow!("invalid {k}: {v}"))?
                        .timestamp())
                }
            }
        }
    }
    fn selectors(&self) -> anyhow::Result<Vec<(Option<String>, promql_parser::label::Matchers)>> {
        self.all("match[]")
            .into_iter()
            .map(|m| match promql_parser::parser::parse(m) {
                Ok(promql_parser::parser::Expr::VectorSelector(vs)) => Ok((vs.name, vs.matchers)),
                Ok(_) => anyhow::bail!("match[] must be a selector: {m}"),
                Err(e) => anyhow::bail!("{e}"),
            })
            .collect()
    }
}

fn prom_success(data: Value, extra: Value) -> Response {
    Json(json!({ "status": "success", "data": data, "atalaya": extra })).into_response()
}

fn matrix_json(m: Vec<(promql::Labels, Vec<(i64, f64)>)>) -> Value {
    Value::Array(
        m.into_iter()
            .map(|(l, pts)| json!({ "metric": l, "values": pts.into_iter().map(|(t, v)| json!([t, promql::fmt_value(v)])).collect::<Vec<_>>() }))
            .collect(),
    )
}

async fn prom_query_range(State(app): State<Shared>, RawQuery(q): RawQuery, body: String) -> Result<Response, PromError> {
    let p = PromParams::new(q, &body);
    let now = unix_now();
    let end = p.time("end", now)?;
    let start = p.time("start", end - 3600)?;
    let step = match p.get("step") {
        Some(s) => s.parse::<f64>().map(|f| f.ceil() as i64).or_else(|_| query::parse_duration(s))?,
        None => 15,
    };
    let t0 = std::time::Instant::now();
    let r = app.promql.range(p.query()?, start, end, step).await?;
    let extra = json!({ "tier": r.tier.name(), "elapsedMs": t0.elapsed().as_millis(), "timing": r.timing, "samplesRead": r.samples_read });
    Ok(match r.output {
        Output::Matrix(m) => prom_success(json!({ "resultType": "matrix", "result": matrix_json(m) }), extra),
        Output::Scalar(s) => prom_success(json!({ "resultType": "matrix", "result": matrix_json(vec![(Default::default(), s)]) }), extra),
        Output::Str(_) => return Err(anyhow::anyhow!("a range query cannot return a string").into()),
    })
}

#[derive(Deserialize)]
struct BatchQuery {
    query: String,
    start: i64,
    end: i64,
    step: i64,
}

#[derive(Deserialize)]
struct BatchParams {
    queries: Vec<BatchQuery>,
}

/// Several range queries in one request (dashboards). Each result is what
/// /api/v1/query_range would return, or its error, in the same order.
async fn prom_query_range_batch(State(app): State<Shared>, Json(p): Json<BatchParams>) -> Result<Response, PromError> {
    if p.queries.len() > 64 {
        return Err(anyhow::anyhow!("at most 64 queries per batch").into());
    }
    let runs = p.queries.iter().map(|q| async {
        let t0 = std::time::Instant::now();
        match app.promql.range(&q.query, q.start, q.end, q.step).await {
            Ok(r) => {
                let extra = json!({ "tier": r.tier.name(), "elapsedMs": t0.elapsed().as_millis(), "timing": r.timing, "samplesRead": r.samples_read });
                match r.output {
                    Output::Matrix(m) => json!({ "status": "success", "data": { "resultType": "matrix", "result": matrix_json(m) }, "atalaya": extra }),
                    Output::Scalar(s) => json!({ "status": "success", "data": { "resultType": "matrix", "result": matrix_json(vec![(Default::default(), s)]) }, "atalaya": extra }),
                    Output::Str(_) => json!({ "status": "error", "errorType": "bad_data", "error": "a range query cannot return a string" }),
                }
            }
            Err(e) => {
                let kind = if e.chain().any(|x| x.is::<atalaya::ch::Unavailable>()) { "unavailable" } else { "bad_data" };
                json!({ "status": "error", "errorType": kind, "error": format!("{e:#}") })
            }
        }
    });
    let results: Vec<Value> = futures::future::join_all(runs).await;
    Ok(Json(json!({ "status": "success", "results": results })).into_response())
}

async fn prom_query(State(app): State<Shared>, RawQuery(q): RawQuery, body: String) -> Result<Response, PromError> {
    let p = PromParams::new(q, &body);
    let time = p.time("time", unix_now())?;
    let t0 = std::time::Instant::now();
    let r = app.promql.instant(p.query()?, time).await?;
    let extra = json!({ "tier": r.tier.name(), "elapsedMs": t0.elapsed().as_millis(), "timing": r.timing, "samplesRead": r.samples_read });
    Ok(match r.output {
        Output::Matrix(m) => {
            let result: Vec<Value> = m
                .into_iter()
                .filter_map(|(l, pts)| pts.last().map(|(t, v)| json!({ "metric": l, "value": [t, promql::fmt_value(*v)] })))
                .collect();
            prom_success(json!({ "resultType": "vector", "result": result }), extra)
        }
        Output::Scalar(s) => {
            let (t, v) = s.last().copied().unwrap_or((time, f64::NAN));
            prom_success(json!({ "resultType": "scalar", "result": [t, promql::fmt_value(v)] }), extra)
        }
        Output::Str(s) => prom_success(json!({ "resultType": "string", "result": [time, s] }), extra),
    })
}

async fn prom_label_values(State(app): State<Shared>, Path(name): Path<String>, RawQuery(q): RawQuery) -> Result<Response, PromError> {
    let p = PromParams::new(q, "");
    let start = p.time("start", unix_now() - 3600)?;
    let vals = app.promql.store.label_values(&name, &p.selectors()?, start).await?;
    Ok(prom_success(json!(vals), json!({})))
}

async fn prom_series(State(app): State<Shared>, RawQuery(q): RawQuery, body: String) -> Result<Response, PromError> {
    let p = PromParams::new(q, &body);
    let start = p.time("start", unix_now() - 3600)?;
    let sels = p.selectors()?;
    if sels.is_empty() {
        return Err(anyhow::anyhow!("missing match[]").into());
    }
    let series = app.promql.store.series(&sels, start).await?;
    Ok(prom_success(json!(series), json!({})))
}
