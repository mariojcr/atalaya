// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap, ser::SerializeSeq};
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

/// The storage (ClickHouse or VictoriaMetrics) did not answer: 502, not a bad query
pub(super) fn unavailable(e: &(dyn std::error::Error + 'static)) -> bool {
    e.is::<atalaya::ch::Unavailable>() || e.is::<atalaya::promql::victoria::Unavailable>() || e.is::<crate::vlogs::Unavailable>()
}

/// Error in Prometheus format (Grafana shows it as is)
struct PromError(anyhow::Error);

impl IntoResponse for PromError {
    fn into_response(self) -> Response {
        // Like Prometheus: bad_data (400) for the query, unavailable when
        // ClickHouse fails (502 rather than 503: the storage is another service)
        let down = self.0.chain().any(unavailable);
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

/// A matrix result as the Prometheus API writes it, serialized straight from the evaluated series:
/// a serde_json::Value tree costs ~400 bytes per point, against 16 for the point itself
struct MatrixJson(Vec<(promql::Labels, Vec<(i64, f64)>)>);

impl Serialize for MatrixJson {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (labels, points) in &self.0 {
            seq.serialize_element(&SeriesJson(labels, points))?;
        }
        seq.end()
    }
}

struct SeriesJson<'a>(&'a promql::Labels, &'a [(i64, f64)]);

impl Serialize for SeriesJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("metric", self.0)?;
        m.serialize_entry("values", &PointsJson(self.1))?;
        m.end()
    }
}

struct PointsJson<'a>(&'a [(i64, f64)]);

impl Serialize for PointsJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (t, v) in self.0 {
            seq.serialize_element(&(t, promql::fmt_value(*v)))?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct MatrixData {
    #[serde(rename = "resultType")]
    result_type: &'static str,
    result: MatrixJson,
}

/// A successful range query: what /api/v1/query_range returns, and each entry of a batch
#[derive(Serialize)]
struct RangeSuccess {
    status: &'static str,
    data: MatrixData,
    atalaya: Value,
}

impl RangeSuccess {
    fn new(m: Vec<(promql::Labels, Vec<(i64, f64)>)>, extra: Value) -> Self {
        Self {
            status: "success",
            data: MatrixData {
                result_type: "matrix",
                result: MatrixJson(m),
            },
            atalaya: extra,
        }
    }
}

/// One result of a batch: the range query's answer, or its error
#[derive(Serialize)]
#[serde(untagged)]
enum BatchResult {
    Ok(RangeSuccess),
    Err(Value),
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
        Output::Matrix(m) => Json(RangeSuccess::new(m, extra)).into_response(),
        Output::Scalar(s) => Json(RangeSuccess::new(vec![(Default::default(), s)], extra)).into_response(),
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
                    Output::Matrix(m) => BatchResult::Ok(RangeSuccess::new(m, extra)),
                    Output::Scalar(s) => BatchResult::Ok(RangeSuccess::new(vec![(Default::default(), s)], extra)),
                    Output::Str(_) => BatchResult::Err(json!({ "status": "error", "errorType": "bad_data", "error": "a range query cannot return a string" })),
                }
            }
            Err(e) => {
                let kind = if e.chain().any(unavailable) { "unavailable" } else { "bad_data" };
                BatchResult::Err(json!({ "status": "error", "errorType": kind, "error": format!("{e:#}") }))
            }
        }
    });
    #[derive(Serialize)]
    struct Batch {
        status: &'static str,
        results: Vec<BatchResult>,
    }
    let results = futures::future::join_all(runs).await;
    Ok(Json(Batch { status: "success", results }).into_response())
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
    let vals = app.promql.label_values(&name, &p.all("match[]"), &p.selectors()?, start).await?;
    Ok(prom_success(json!(vals), json!({})))
}

async fn prom_series(State(app): State<Shared>, RawQuery(q): RawQuery, body: String) -> Result<Response, PromError> {
    let p = PromParams::new(q, &body);
    let start = p.time("start", unix_now() - 3600)?;
    let sels = p.selectors()?;
    if sels.is_empty() {
        return Err(anyhow::anyhow!("missing match[]").into());
    }
    let series = app.promql.series(&p.all("match[]"), &sels, start).await?;
    Ok(prom_success(json!(series), json!({})))
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn a_streamed_matrix_is_the_prometheus_json() {
        let labels: promql::Labels = [("__name__".to_owned(), "up".to_owned()), ("job".to_owned(), "a\"b".to_owned())].into();
        let m = vec![
            (labels.clone(), vec![(1, 2.5), (2, f64::NAN), (3, f64::INFINITY)]),
            (Default::default(), vec![]),
        ];
        let extra = json!({ "tier": "raw" });
        let got = serde_json::to_value(RangeSuccess::new(m, extra.clone())).unwrap();
        let want = json!({
            "status": "success",
            "data": { "resultType": "matrix", "result": [
                { "metric": labels, "values": [[1, "2.5"], [2, "NaN"], [3, "+Inf"]] },
                { "metric": {}, "values": [] },
            ] },
            "atalaya": extra,
        });
        assert_eq!(got, want);
        let batch = serde_json::to_value(vec![BatchResult::Err(json!({ "status": "error" }))]).unwrap();
        assert_eq!(batch, json!([{ "status": "error" }]));
    }
}
