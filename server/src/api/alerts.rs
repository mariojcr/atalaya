// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Alerts: rules and their state, history, tests and silences.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, lang_of, unix_now};
use crate::app::Shared;
use crate::{alerts, query};

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/alerts", get(alerts_list))
        .route("/api/v1/alerts/history", get(alerts_history))
        .route("/api/v1/alerts/silences", get(alerts_silences).post(alerts_silence_add))
        .route("/api/v1/alerts/silences/{id}", delete(alerts_silence_delete))
        .route("/api/v1/alerts/{id}/test", post(alerts_test))
}

#[derive(Deserialize)]
struct LangParams {
    #[serde(default)]
    lang: Option<String>,
}

/// Rules with their state, firing instances, channels and silences
async fn alerts_list(State(app): State<Shared>, Query(p): Query<LangParams>) -> ApiResult<Value> {
    Ok(Json(app.alerts.snapshot(lang_of(&p.lang)).await))
}

#[derive(Deserialize)]
struct HistoryParams {
    range: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    #[serde(default)]
    lang: Option<String>,
}

/// Transitions (firing / resolved) within the range
async fn alerts_history(State(app): State<Shared>, Query(p): Query<HistoryParams>) -> ApiResult<Value> {
    let end = p.end.unwrap_or_else(unix_now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("7d"))?,
    };
    Ok(Json(
        json!({ "start": start, "end": end, "history": app.alerts.history_json(start, end, lang_of(&p.lang)).await }),
    ))
}

/// Evaluates the rule now and returns what matches, without touching its state
async fn alerts_test(State(app): State<Shared>, Path(id): Path<String>, Query(p): Query<LangParams>) -> ApiResult<Value> {
    let cfg = app.alerts.config().await;
    let rule = cfg.rules.iter().find(|r| r.id == id).ok_or_else(|| anyhow::anyhow!("unknown rule: {id}"))?;
    let lang = lang_of(&p.lang);
    let t0 = std::time::Instant::now();
    let matches = app.alerts.evaluate(&app, rule).await?;
    Ok(Json(json!({
        "rule": id, "elapsedMs": t0.elapsed().as_millis(),
        "matches": matches.iter().map(|(l, v)| json!({ "labels": l, "value": v, "summary": rule.render_summary(lang, l, *v) })).collect::<Vec<_>>(),
    })))
}

async fn alerts_silences(State(app): State<Shared>) -> ApiResult<Value> {
    Ok(Json(json!({ "silences": app.alerts.silences().await })))
}

#[derive(Deserialize)]
struct SilenceBody {
    #[serde(default)]
    rule: Option<String>,
    #[serde(default)]
    labels: alerts::Labels,
    /// Duration ("2h") or RFC 3339 date
    until: String,
    #[serde(default)]
    comment: String,
}

async fn alerts_silence_add(State(app): State<Shared>, Json(b): Json<SilenceBody>) -> ApiResult<Value> {
    let until = match query::parse_duration(&b.until) {
        Ok(d) => unix_now() + d,
        Err(_) => chrono::DateTime::parse_from_rfc3339(&b.until)
            .map_err(|_| anyhow::anyhow!("until: a duration (2h) or an RFC 3339 date"))?
            .timestamp(),
    };
    let s = app.alerts.add_silence(b.rule, b.labels, until, b.comment).await?;
    Ok(Json(serde_json::to_value(s)?))
}

async fn alerts_silence_delete(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(if app.alerts.delete_silence(&id).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    })
}
