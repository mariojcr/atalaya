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
}

/// Structured `f=` filters from the URL (repeatable)
fn log_filters(q: &Option<String>) -> Vec<String> {
    form_urlencoded::parse(q.as_deref().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "f")
        .map(|(_, v)| v.into_owned())
        .collect()
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
