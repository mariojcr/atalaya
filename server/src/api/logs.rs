// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Log explorer: search, fields, values, context and saved searches.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use serde_json::{Value, json};

use atalaya::ch::Param;

use super::{ApiError, ApiResult, unix_now};
use crate::app::Shared;
use crate::{logsearch, notebooks, query};

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/logs", get(logs))
        .route("/api/v1/logs/fields", get(log_fields))
        .route("/api/v1/logs/counts", get(log_counts))
        .route("/api/v1/logs/values", get(log_values))
        .route("/api/v1/logs/context", get(log_context))
        .route("/api/v1/searches", get(sv_list))
        .route("/api/v1/searches/{id}", get(sv_get).put(sv_put).delete(sv_delete))
}

#[derive(Deserialize)]
struct LogParams {
    #[serde(default)]
    range: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    #[serde(default)]
    service: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    level: String,
    /// Text to search for in the message (case-insensitive)
    #[serde(default)]
    q: String,
    limit: Option<u32>,
    /// Break the histogram down by this field (service, level, url.path…)
    #[serde(default)]
    breakdown: String,
    /// Only the lines, without the histogram and the field counts (dashboards)
    #[serde(default)]
    docs_only: bool,
}

/// Every value of a repeatable URL parameter
fn query_all(q: &Option<String>, key: &str) -> Vec<String> {
    form_urlencoded::parse(q.as_deref().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .collect()
}

/// Structured `f=` filters from the URL (repeatable)
fn log_filters(q: &Option<String>) -> Vec<String> {
    query_all(q, "f")
}

async fn logs(State(app): State<Shared>, Query(p): Query<LogParams>, RawQuery(rq): RawQuery) -> ApiResult<Value> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let end = p.end.unwrap_or(now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("1h"))?,
    };
    let mut params: Vec<Param> = vec![("start".into(), start.to_string()), ("end".into(), end.to_string())];
    let mut conds = vec!["Time >= fromUnixTimestamp64Milli({start:Int64} * 1000) AND Time < fromUnixTimestamp64Milli({end:Int64} * 1000)".to_owned()];
    if !p.service.is_empty() {
        conds.push("Service = {service:String}".into());
        params.push(("service".into(), p.service));
    }
    if !p.host.is_empty() {
        conds.push("Host = {host:String}".into());
        params.push(("host".into(), p.host));
    }
    if !p.level.is_empty() {
        conds.push("has(splitByChar(',', {level:String}), Level)".into());
        params.push(("level".into(), p.level));
    }
    if !p.q.is_empty() {
        conds.push("positionCaseInsensitiveUTF8(Body, {q:String}) > 0".into());
        params.push(("q".into(), p.q));
    }
    conds.extend(logsearch::filters_sql(&log_filters(&rq), &mut params)?);
    let limit = p.limit.unwrap_or(500).min(5000);
    let table = format!("{}.logs", app.db);
    let filter = conds.join(" AND ");
    let rows = app
        .ch
        .rows(
            &format!(
                "SELECT toUnixTimestamp64Milli(Time) AS ts, Service AS service, Host AS host, Level AS level, Stream AS stream,
                        Body AS body, Resource AS resource, Attributes AS attributes
                 FROM {table} WHERE {filter} ORDER BY Time DESC LIMIT {limit}"
            ),
            &params,
        )
        .await?;
    if p.docs_only {
        return Ok(Json(json!({ "start": start, "end": end, "logs": rows })));
    }
    // Counts per interval for the histogram
    let step = ((end - start) / 120).max(1);
    let mut hp = params.clone();
    hp.push(("step".into(), step.to_string()));
    let histogram = app
        .ch
        .rows(
            &format!(
                "SELECT toUnixTimestamp(toStartOfInterval(Time, INTERVAL {{step:Int64}} SECOND)) AS ts, Level AS level, count() AS n
                 FROM {table} WHERE {filter} GROUP BY ts, level ORDER BY ts"
            ),
            &hp,
        )
        .await?;
    // Counts per field in the range (ignoring the other filters), for the fields panel
    let fields = app
        .ch
        .rows(
            &format!(
                "SELECT * FROM (
                   SELECT 'service' AS field, Service AS value, count() AS n FROM {table} WHERE {range} GROUP BY value
                   UNION ALL SELECT 'host', Host, count() FROM {table} WHERE {range} GROUP BY Host
                   UNION ALL SELECT 'level', Level, count() FROM {table} WHERE {range} GROUP BY Level
                 ) ORDER BY field, n DESC",
                range = conds[0]
            ),
            &params[..2],
        )
        .await?;
    // Histogram breakdown by one field
    let breakdown = if p.breakdown.is_empty() {
        Value::Null
    } else {
        let mut bp = params.clone();
        bp.push(("step".into(), step.to_string()));
        // Validates the field name; the generated condition is not needed
        logsearch::filters_sql(&[format!("{}:*", p.breakdown)], &mut bp)?;
        let col_sql = match p.breakdown.as_str() {
            "service" => "Service".to_owned(),
            "host" => "Host".to_owned(),
            "level" => "Level".to_owned(),
            "stream" => "Stream".to_owned(),
            _ => {
                bp.push(("bk".into(), p.breakdown.clone()));
                "Attributes[{bk:String}]".to_owned()
            }
        };
        json!(app.ch.rows(&logsearch::breakdown_sql(&table, &filter, &col_sql), &bp).await?)
    };
    Ok(Json(
        json!({ "start": start, "end": end, "step": step, "logs": rows, "histogram": histogram, "fields": fields, "breakdown": breakdown }),
    ))
}

/// Fields present in the logs matching the filters, with counts and their most
/// frequent values (the Discover-style fields panel)
async fn log_fields(State(app): State<Shared>, Query(p): Query<LogParams>, RawQuery(rq): RawQuery) -> ApiResult<Value> {
    let now = unix_now();
    let end = p.end.unwrap_or(now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("1h"))?,
    };
    let mut params: Vec<Param> = vec![("start".into(), start.to_string()), ("end".into(), end.to_string())];
    let mut conds = vec!["Time >= fromUnixTimestamp64Milli({start:Int64} * 1000) AND Time < fromUnixTimestamp64Milli({end:Int64} * 1000)".to_owned()];
    if !p.service.is_empty() {
        conds.push("Service = {service:String}".into());
        params.push(("service".into(), p.service));
    }
    if !p.q.is_empty() {
        conds.push("positionCaseInsensitiveUTF8(Body, {q:String}) > 0".into());
        params.push(("q".into(), p.q));
    }
    conds.extend(logsearch::filters_sql(&log_filters(&rq), &mut params)?);
    let table = format!("{}.logs", app.db);
    let rows = app.ch.rows(&logsearch::fields_sql(&table, &conds.join(" AND ")), &params).await?;
    Ok(Json(json!({ "start": start, "end": end, "fields": rows })))
}

#[derive(Deserialize)]
struct CountsParams {
    #[serde(default)]
    range: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    /// Seconds per point; without it, one count for the whole range
    step: Option<i64>,
    /// How many groups to keep, the most frequent (10 by default, up to 100)
    top: Option<u32>,
}

/// At most this many points per series, as in the PromQL API
const MAX_POINTS: i64 = 11_000;
/// At most this many `by` fields
const MAX_BY: usize = 3;

/// Line counts for dashboards, as series in the PromQL API's shape: grouped by
/// the `by=` fields (repeatable), the `top` most frequent groups of the range,
/// with one count each or one per `step` (0 where there were no lines)
async fn log_counts(State(app): State<Shared>, Query(p): Query<CountsParams>, RawQuery(rq): RawQuery) -> ApiResult<Value> {
    let now = unix_now();
    let end = p.end.unwrap_or(now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("1h"))?,
    };
    // Within the epoch and a day ahead: `end - start` and the milliseconds
    // ClickHouse computes cannot overflow
    if start < 0 || end < start || end > now + 86400 {
        return Err(anyhow::anyhow!("invalid range").into());
    }
    let mut params: Vec<Param> = vec![("start".into(), start.to_string()), ("end".into(), end.to_string())];
    let mut conds = vec!["Time >= fromUnixTimestamp64Milli({start:Int64} * 1000) AND Time < fromUnixTimestamp64Milli({end:Int64} * 1000)".to_owned()];
    let filters = log_filters(&rq);
    conds.extend(logsearch::filters_sql(&filters, &mut params)?);
    let by = query_all(&rq, "by");
    if by.len() > MAX_BY {
        return Err(anyhow::anyhow!("at most {MAX_BY} by fields").into());
    }
    // Their parameters are numbered after the filters'
    let keys = by
        .iter()
        .enumerate()
        .map(|(i, f)| logsearch::field_sql(f, &mut params, filters.len() + i))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Some(step) = p.step {
        if step <= 0 {
            return Err(anyhow::anyhow!("step must be positive").into());
        }
        if (end - start) / step + 1 > MAX_POINTS {
            return Err(anyhow::anyhow!("too many points per series ({}): increase the step", (end - start) / step + 1).into());
        }
        params.push(("step".into(), step.to_string()));
    }
    let top = p.top.unwrap_or(10).clamp(1, 100);
    let table = format!("{}.logs", app.db);
    let sql = logsearch::counts_sql(&table, &conds.join(" AND "), &keys, top, p.step.is_some());
    let rows = app.ch.rows(&sql, &params).await?;
    Ok(Json(json!({ "series": count_series(&by, &rows, start, end, p.step) })))
}

/// The rows of `counts_sql` as series: labels named after the `by` fields and
/// points every `step` from the bucket holding `start` (0 where a group had no
/// lines), or a single point at `end`
fn count_series(by: &[String], rows: &[Value], start: i64, end: i64, step: Option<i64>) -> Vec<Value> {
    let mut groups: Vec<(serde_json::Map<String, Value>, BTreeMap<i64, u64>)> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for r in rows {
        let key: Vec<String> = (0..by.len()).map(|i| r[format!("k{i}")].as_str().unwrap_or("").to_owned()).collect();
        let i = *index.entry(key.clone()).or_insert_with(|| {
            groups.push((by.iter().cloned().zip(key.into_iter().map(Value::String)).collect(), BTreeMap::new()));
            groups.len() - 1
        });
        let ts = step.map_or(end, |_| r["ts"].as_i64().unwrap_or(0));
        groups[i].1.insert(ts, r["n"].as_u64().unwrap_or(0));
    }
    groups
        .into_iter()
        .map(|(labels, counts)| {
            let points: Vec<Value> = match step {
                Some(step) => {
                    let first = start - start.rem_euclid(step);
                    (0..=MAX_POINTS)
                        .map(|k| first + k * step)
                        .take_while(|t| *t < end)
                        .map(|t| json!([t, counts.get(&t).copied().unwrap_or(0)]))
                        .collect()
                }
                None => counts.into_iter().map(|(t, n)| json!([t, n])).collect(),
            };
            json!({ "labels": labels, "points": points })
        })
        .collect()
}

#[derive(Deserialize)]
struct ValuesParams {
    #[serde(default)]
    range: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    field: String,
    #[serde(default)]
    prefix: String,
}

/// Most frequent values of a field starting with the prefix, within the range
/// and filters (query bar autocompletion)
async fn log_values(State(app): State<Shared>, Query(p): Query<ValuesParams>, RawQuery(rq): RawQuery) -> ApiResult<Value> {
    let now = unix_now();
    let end = p.end.unwrap_or(now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("1h"))?,
    };
    let mut params: Vec<Param> = vec![
        ("start".into(), start.to_string()),
        ("end".into(), end.to_string()),
        ("prefix".into(), p.prefix),
    ];
    let mut conds = vec!["Time >= fromUnixTimestamp64Milli({start:Int64} * 1000) AND Time < fromUnixTimestamp64Milli({end:Int64} * 1000)".to_owned()];
    // The query bar filters except the one being typed (the client strips it)
    conds.extend(logsearch::filters_sql(&log_filters(&rq), &mut params)?);
    let col = logsearch::field_sql(&p.field, &mut params, 999)?;
    let table = format!("{}.logs", app.db);
    let rows = app.ch.rows(&logsearch::values_sql(&table, &conds.join(" AND "), &col), &params).await?;
    Ok(Json(json!({ "field": p.field, "values": rows })))
}

#[derive(Deserialize)]
struct ContextParams {
    service: String,
    #[serde(default)]
    host: String,
    /// Unix milliseconds of the document
    ts: i64,
    #[serde(default)]
    n: Option<u32>,
}

/// Documents around one: the n before and after it from the same service
/// (and host)
async fn log_context(State(app): State<Shared>, Query(p): Query<ContextParams>) -> ApiResult<Value> {
    let n = p.n.unwrap_or(20).min(200);
    let table = format!("{}.logs", app.db);
    let mut params: Vec<Param> = vec![("service".into(), p.service), ("ts".into(), p.ts.to_string())];
    let mut cond = "Service = {service:String}".to_owned();
    if !p.host.is_empty() {
        cond.push_str(" AND Host = {host:String}");
        params.push(("host".into(), p.host));
    }
    let select = "toUnixTimestamp64Milli(Time) AS ts, Service AS service, Host AS host, Level AS level, Stream AS stream, Body AS body, Resource AS resource, Attributes AS attributes";
    let before = app
        .ch
        .rows(
            &format!("SELECT {select} FROM {table} WHERE {cond} AND Time < fromUnixTimestamp64Milli({{ts:Int64}}) ORDER BY Time DESC LIMIT {n}"),
            &params,
        )
        .await?;
    let after = app
        .ch
        .rows(
            &format!(
                "SELECT {select} FROM {table} WHERE {cond} AND Time >= fromUnixTimestamp64Milli({{ts:Int64}}) ORDER BY Time ASC LIMIT {}",
                n + 1
            ),
            &params,
        )
        .await?;
    Ok(Json(json!({ "before": before.into_iter().rev().collect::<Vec<_>>(), "after": after })))
}

// ── Saved searches (log explorer) ────────────────────────────────────────────

async fn sv_list(State(app): State<Shared>) -> ApiResult<Value> {
    Ok(Json(json!({ "searches": notebooks::list(&app.data.join("searches"))?["notebooks"] })))
}

async fn sv_get(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    match notebooks::get(&app.data.join("searches"), &id)? {
        Some(v) => Ok(Json(v).into_response()),
        None => Ok((StatusCode::NOT_FOUND, Json(json!({ "error": "saved search not found" }))).into_response()),
    }
}

async fn sv_put(State(app): State<Shared>, Path(id): Path<String>, body: Bytes) -> ApiResult<Value> {
    if body.len() > 256 << 10 {
        return Err(anyhow::anyhow!("saved search too large").into());
    }
    let v: Value = serde_json::from_slice(&body)?;
    Ok(Json(notebooks::save(&app.data.join("searches"), &id, v)?))
}

async fn sv_delete(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(if notebooks::delete(&app.data.join("searches"), &id)? {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_series_fill_the_buckets_without_lines() {
        let by = ["host".to_owned()];
        let rows = [
            json!({"ts": 1200, "k0": "Thor", "n": 3}),
            json!({"ts": 1800, "k0": "Thor", "n": 1}),
            json!({"ts": 1800, "k0": "Odin", "n": 2}),
        ];
        // Buckets start at the one holding `start` and stop before `end`
        let got = count_series(&by, &rows, 1250, 2400, Some(600));
        assert_eq!(
            got,
            [
                json!({"labels": {"host": "Thor"}, "points": [[1200, 3], [1800, 1]]}),
                json!({"labels": {"host": "Odin"}, "points": [[1200, 0], [1800, 2]]}),
            ]
        );
        // Never more than MAX_POINTS
        let got = count_series(&by, &rows[..1], 0, i64::MAX, Some(1));
        assert_eq!(
            got[0]["points"].as_array().unwrap().len(),
            MAX_POINTS as usize + 1,
            "the bucket holding start, then MAX_POINTS more"
        );
        // Without a step: one count per group, at the end
        let got = count_series(&by, &[json!({"k0": "Thor", "n": 4})], 0, 3600, None);
        assert_eq!(got, [json!({"labels": {"host": "Thor"}, "points": [[3600, 4]]})]);
    }
}
