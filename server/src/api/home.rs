// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Summary views: overview (home) and timeline.

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::Value;

use super::{ApiResult, lang_of, unix_now};
use crate::app::Shared;
use crate::{events, overview, query};

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/overview", get(overview_handler))
        .route("/api/v1/events", get(events_handler))
}

// ── Overview ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OverviewParams {
    #[serde(default)]
    range: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

/// Host and service status in one response (Board / Table views)
async fn overview_handler(State(app): State<Shared>, Query(p): Query<OverviewParams>) -> ApiResult<Value> {
    let range = query::parse_duration(p.range.as_deref().unwrap_or("24h"))?.clamp(900, 90 * 86400);
    let lang = lang_of(&p.lang);
    // The cache key keeps languages apart (one bit for the language)
    let key = range * 2 + if lang == "es" { 1 } else { 0 };
    if let Some((t, v)) = app.overview_cache.lock().await.get(&key)
        && t.elapsed() < std::time::Duration::from_secs(10)
    {
        return Ok(Json(v.clone()));
    }
    let v = overview::overview(&app, range, lang).await?;
    let mut cache = app.overview_cache.lock().await;
    // Drop expired entries, and keep at most 32 distinct ranges
    cache.retain(|_, (t, _)| t.elapsed() < std::time::Duration::from_secs(10));
    if cache.len() < 32 {
        cache.insert(key, (std::time::Instant::now(), v.clone()));
    }
    drop(cache);
    Ok(Json(v))
}

// ── Timeline ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EventsParams {
    range: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    #[serde(default)]
    service: String,
    #[serde(default)]
    lang: Option<String>,
}

/// Events and lanes for a range
async fn events_handler(State(app): State<Shared>, Query(p): Query<EventsParams>) -> ApiResult<Value> {
    let now = unix_now() - crate::app::now_delay();
    let end = p.end.unwrap_or(now).min(now);
    let start = match p.start {
        Some(s) => s,
        None => end - query::parse_duration(p.range.as_deref().unwrap_or("24h"))?,
    };
    if end - start < 300 || end - start > 400 * 86400 {
        return Err(anyhow::anyhow!("range must be between 5 minutes and 400 days").into());
    }
    let service = if p.service.is_empty() { None } else { Some(p.service.as_str()) };
    Ok(Json(events::events(&app, start, end, service, lang_of(&p.lang)).await?))
}
