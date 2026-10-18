// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Notebooks (saved investigations).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use super::{ApiError, ApiResult};
use crate::app::Shared;
use crate::notebooks;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/notebooks", get(nb_list))
        .route("/api/v1/notebooks/{id}", get(nb_get).put(nb_put).delete(nb_delete))
}

async fn nb_list(State(app): State<Shared>) -> ApiResult<Value> {
    Ok(Json(notebooks::list(&app.data)?))
}

async fn nb_get(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    match notebooks::get(&app.data, &id)? {
        Some(v) => Ok(Json(v).into_response()),
        None => Ok((StatusCode::NOT_FOUND, Json(json!({ "error": "notebook not found" }))).into_response()),
    }
}

async fn nb_put(State(app): State<Shared>, Path(id): Path<String>, body: Bytes) -> ApiResult<Value> {
    if body.len() > 2 << 20 {
        return Err(anyhow::anyhow!("notebook too large (maximum 2 MB)").into());
    }
    let v: Value = serde_json::from_slice(&body)?;
    notebooks::validate(&v)?;
    Ok(Json(notebooks::save(&app.data, &id, v)?))
}

async fn nb_delete(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(if notebooks::delete(&app.data, &id)? {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    })
}
