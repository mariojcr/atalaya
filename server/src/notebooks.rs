// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Notebooks: saved investigations (text, PromQL and log cells whose results
//! the UI renders). One JSON file per notebook in the data directory; the UI
//! defines the content, the server only stores and lists it.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

const MAX_BYTES: usize = 2 << 20;

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("invalid notebook id");
    }
    Ok(dir.join(format!("{id}.json")))
}

/// Lists id, title, update time and cell count of every notebook
pub fn list(dir: &Path) -> Result<Value> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            out.push(json!({
                "id": p.file_stem().unwrap_or_default().to_string_lossy(),
                "title": v["title"].as_str().unwrap_or("Untitled"),
                "updated": v["updated"],
                "cells": v["cells"].as_array().map(|c| c.len()).unwrap_or(0),
            }));
        }
    }
    out.sort_by(|a, b| b["updated"].as_i64().cmp(&a["updated"].as_i64()));
    Ok(json!({ "notebooks": out }))
}

pub fn get(dir: &Path, id: &str) -> Result<Option<Value>> {
    let p = path(dir, id)?;
    match std::fs::read_to_string(&p) {
        Ok(t) => Ok(Some(serde_json::from_str(&t)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Cell types the UI renders
const CELL_TYPES: &[&str] = &["text", "note", "promql", "logs"];
const MAX_CELLS: usize = 500;

/// Validates a notebook's shape before saving: the UI puts each cell's type and
/// id into the HTML, so only known values are accepted.
pub fn validate(body: &Value) -> Result<()> {
    if !body.is_object() {
        bail!("the notebook must be a JSON object");
    }
    if body.get("title").is_some_and(|t| !t.is_string()) {
        bail!("title must be a string");
    }
    let Some(cells) = body.get("cells") else { return Ok(()) };
    let Some(cells) = cells.as_array() else { bail!("cells must be an array") };
    if cells.len() > MAX_CELLS {
        bail!("too many cells (maximum {MAX_CELLS})");
    }
    for (i, c) in cells.iter().enumerate() {
        let ty = c.get("type").and_then(Value::as_str).unwrap_or_default();
        if !CELL_TYPES.contains(&ty) {
            bail!("cell {}: invalid type", i + 1);
        }
        let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
        if !valid_id(id) {
            bail!("cell {}: invalid id", i + 1);
        }
    }
    Ok(())
}

/// Saves the notebook (stamps `updated`); atomic write via rename
pub fn save(dir: &Path, id: &str, mut body: Value) -> Result<Value> {
    let p = path(dir, id)?;
    if !body.is_object() {
        bail!("the notebook must be a JSON object");
    }
    body["id"] = json!(id);
    body["updated"] = json!(chrono::Utc::now().timestamp());
    let text = serde_json::to_vec_pretty(&body)?;
    if text.len() > MAX_BYTES {
        bail!("notebook too large (maximum 2 MB)");
    }
    std::fs::create_dir_all(dir)?;
    // One temp file per write so concurrent saves never share a file
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = p.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, &p)?;
    Ok(body)
}

pub fn delete(dir: &Path, id: &str) -> Result<bool> {
    let p = path(dir, id)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_cell_types_and_ids() {
        assert!(validate(&json!({"title": "x", "cells": [{"id": "c-1", "type": "promql"}]})).is_ok());
        assert!(validate(&json!({"cells": [{"id": "c-1", "type": "x\" onerror=alert(1) \""}]})).is_err());
        assert!(validate(&json!({"cells": [{"id": "c 1\"><img>", "type": "text"}]})).is_err());
        assert!(validate(&json!({"cells": "no"})).is_err());
        assert!(validate(&json!([1])).is_err());
    }

    #[test]
    fn concurrent_saves_use_distinct_temp_files() {
        let dir = std::env::temp_dir().join(format!("atalaya-nb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let d = dir.clone();
                std::thread::spawn(move || save(&d, "same", json!({"title": format!("v{i}"), "cells": []})).unwrap())
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(get(&dir, "same").unwrap().is_some());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp files left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
