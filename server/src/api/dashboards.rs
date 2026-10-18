// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! YAML dashboards.

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::{Value, json};

use super::ApiResult;
use crate::app::Shared;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/dashboards", get(list_dashboards))
        .route("/api/v1/dashboards/{name}", get(get_dashboard))
}

async fn list_dashboards(State(app): State<Shared>) -> ApiResult<Value> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&app.dashboards) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("yaml") {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let title = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_saphyr::from_str::<BTreeMap<String, serde_json::Value>>(&s).ok())
                    .and_then(|m| m.get("title").and_then(|t| t.as_str().map(str::to_owned)))
                    .unwrap_or_else(|| name.clone());
                out.push(json!({ "name": name, "title": title }));
            }
        }
    }
    out.sort_by(|a, b| a["title"].as_str().cmp(&b["title"].as_str()));
    Ok(Json(json!({ "dashboards": out })))
}

/// A dashboard's file: its name is the file stem, letters, digits, - and _
fn dashboard_path(dir: &std::path::Path, name: &str) -> anyhow::Result<std::path::PathBuf> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!("invalid dashboard name");
    }
    Ok(dir.join(format!("{name}.yaml")))
}

async fn get_dashboard(State(app): State<Shared>, Path(name): Path<String>) -> ApiResult<Value> {
    let text = std::fs::read_to_string(dashboard_path(&app.dashboards, &name)?)?;
    let value: Value = serde_saphyr::from_str(&text)?;
    Ok(Json(value))
}
