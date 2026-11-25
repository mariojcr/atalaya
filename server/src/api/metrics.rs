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
    if matches!(app.promql, atalaya::promql::Backend::Victoria(_)) {
        return Ok(Json(json!({ "metricDescriptors": victoria_descriptors(&app, &p.q).await? })));
    }
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
    if matches!(app.promql, atalaya::promql::Backend::Victoria(_)) {
        return Ok(Json(json!({ "labels": victoria_labels(&app, &p.metric).await? })));
    }
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
    if matches!(app.promql, atalaya::promql::Backend::Victoria(_)) {
        return Ok(Json(query::run_victoria(&app.promql, &q).await?));
    }
    Ok(Json(query::run(&app.ch, &app.db, &q).await?))
}

/// Metric names on VictoriaMetrics with their live series and services (no descriptors there: the
/// kind is guessed from the name, as the engine does without one)
async fn victoria_descriptors(app: &crate::app::App, q: &str) -> anyhow::Result<Vec<Value>> {
    let re = if q.is_empty() {
        ".+".to_owned()
    } else {
        format!("(?i).*{}.*", regex_escape(q))
    };
    let r = app
        .promql
        .instant(
            &format!("count by (__name__, job) ({{__name__=~{}}})", serde_json::to_string(&re)?),
            crate::app::unix_now(),
        )
        .await?;
    let atalaya::promql::Output::Matrix(m) = r.output else { return Ok(vec![]) };
    let mut by_name: std::collections::BTreeMap<String, (u64, Vec<String>)> = Default::default();
    for (l, pts) in m {
        let (Some(name), Some((_, n))) = (l.get("__name__"), pts.last()) else {
            continue;
        };
        let e = by_name.entry(name.clone()).or_default();
        e.0 += *n as u64;
        if let Some(j) = l.get("job") {
            e.1.push(j.clone());
        }
    }
    Ok(by_name
        .into_iter()
        .take(50_000)
        .map(|(name, (series, services))| {
            let kind = if ["_total", "_count", "_sum", "_bucket"].iter().any(|s| name.ends_with(s)) {
                "cumulative"
            } else {
                "gauge"
            };
            json!({ "name": name, "kind": kind, "unit": "", "description": "", "series": series, "services": services })
        })
        .collect())
}

/// Label fields of a metric (service, resource.*, metric.*) with their most frequent values
async fn victoria_labels(app: &crate::app::App, metric: &str) -> anyhow::Result<Vec<Value>> {
    if metric.is_empty() || !metric.chars().all(|c| c.is_ascii_alphanumeric() || "_:".contains(c)) {
        anyhow::bail!("invalid metric: {metric}");
    }
    let series = app.promql.series(&[metric], &[], crate::app::unix_now() - 86400).await?;
    let mut fields: std::collections::BTreeMap<String, std::collections::HashMap<String, u64>> = Default::default();
    for l in &series {
        for (k, v) in l {
            if let Some(f) = query::field_of_label(k) {
                *fields.entry(f).or_default().entry(v.clone()).or_default() += 1;
            }
        }
    }
    Ok(fields
        .into_iter()
        .map(|(field, vals)| {
            let mut v: Vec<(String, u64)> = vals.into_iter().collect();
            let distinct = v.len();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let values: Vec<String> = v.into_iter().take(100).map(|(s, _)| s).collect();
            json!({ "field": field, "values": values, "distinct": distinct })
        })
        .collect())
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| if "\\.+*?()|[]{}^$".contains(c) { format!("\\{c}") } else { c.to_string() })
        .collect()
}
