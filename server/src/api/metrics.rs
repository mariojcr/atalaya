// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Cloud Monitoring-style metrics: descriptors, labels and timeSeries:query.

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiResult;
use crate::app::Shared;
use crate::query;
use crate::query::TimeSeriesQuery;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/metricDescriptors", get(metric_descriptors))
        .route("/api/v1/labels", get(labels))
        .route("/api/v1/timeSeries:query", post(time_series))
}

#[derive(Deserialize)]
struct DescriptorParams {
    #[serde(default)]
    q: String,
    /// Omit descriptions (for the picker list; they are 40 % of the JSON).
    /// Any value except 0/false/empty
    #[serde(default)]
    brief: Option<String>,
}

/// Metric descriptors with their number of active series
async fn metric_descriptors(State(app): State<Shared>, Query(p): Query<DescriptorParams>) -> ApiResult<Value> {
    let db = &app.db;
    let brief = p.brief.as_deref().is_some_and(|v| !matches!(v, "" | "0" | "false"));
    let desc = if brief { "''" } else { "d.Description" };
    // The UI picker requests the whole list (brief) and filters client-side; a
    // single ClickHouse server alone exports more than 2000 metric names
    let limit = if brief { 50_000 } else { 2_000 };
    let rows = app
        .ch
        .rows(
            &format!(
                "SELECT d.Metric AS name, toString(d.Kind) AS kind, d.Unit AS unit, {desc} AS description, s.n AS series, s.services AS services
                 FROM (SELECT * FROM {db}.metric_descriptors FINAL WHERE Metric ILIKE {{q:String}}) AS d
                 INNER JOIN (SELECT Metric, count() AS n, groupUniqArray(Service) AS services FROM {db}.series FINAL
                             WHERE LastSeen > now() - INTERVAL 1 DAY GROUP BY Metric) AS s USING (Metric)
                 ORDER BY name LIMIT {limit}"
            ),
            &[("q".into(), format!("%{}%", p.q))],
        )
        .await?;
    Ok(Json(json!({ "metricDescriptors": rows })))
}

#[derive(Deserialize)]
struct LabelParams {
    metric: String,
}

/// Label keys of a metric with their most frequent values
async fn labels(State(app): State<Shared>, Query(p): Query<LabelParams>) -> ApiResult<Value> {
    let db = &app.db;
    let rows = app
        .ch
        .rows(
            &format!(
                "SELECT field, topK(100)(value) AS values, uniqExact(value) AS distinct FROM (
                   SELECT 'service' AS field, Service AS value FROM {db}.series FINAL WHERE Metric = {{m:String}}
                   UNION ALL
                   SELECT concat('resource.', k) AS field, v AS value FROM {db}.series FINAL
                     ARRAY JOIN mapKeys(Resource) AS k, mapValues(Resource) AS v WHERE Metric = {{m:String}}
                   UNION ALL
                   SELECT concat('metric.', k) AS field, v AS value FROM {db}.series FINAL
                     ARRAY JOIN mapKeys(Labels) AS k, mapValues(Labels) AS v WHERE Metric = {{m:String}}
                 ) GROUP BY field ORDER BY field"
            ),
            &[("m".into(), p.metric)],
        )
        .await?;
    Ok(Json(json!({ "labels": rows })))
}

async fn time_series(State(app): State<Shared>, Json(q): Json<TimeSeriesQuery>) -> ApiResult<query::QueryResult> {
    Ok(Json(query::run(&app.ch, &app.db, &q).await?))
}
