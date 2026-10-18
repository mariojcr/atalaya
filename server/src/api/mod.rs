// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Server HTTP routes, grouped by area.

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
use serde_json::json;

use crate::app::Shared;

pub(crate) struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // ClickHouse down or failing: 502 (not the request's fault); anything
        // else is invalid input
        let status = if self.0.chain().any(|e| e.is::<atalaya::ch::Unavailable>()) {
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

/// All API and UI routes
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

/// Whether the server can serve: ClickHouse answers and alerts are evaluated.
/// 503 if ClickHouse does not answer; the age of the last alert evaluation is
/// in the body for whoever monitors it. The route needs no session, so the
/// body says only ok or down: why ClickHouse failed (its address, version or
/// "authentication failed") goes to the log.
async fn readyz(axum::extract::State(app): axum::extract::State<Shared>) -> Response {
    let ch = app.ch.text("SELECT 1", &[], &[]).await;
    if let Err(e) = &ch {
        tracing::warn!("readyz: {e:#}");
    }
    let last_eval = app.alerts.last_tick();
    let age = last_eval.map(|t| super::app::unix_now() - t);
    let body = json!({
        "clickhouse": if ch.is_ok() { "ok" } else { "down" },
        "alerts_last_eval": last_eval, "alerts_age_s": age,
    });
    let status = if ch.is_ok() { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(body)).into_response()
}
