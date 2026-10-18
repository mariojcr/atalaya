// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Shared server state and environment configuration.

use std::{path::PathBuf, sync::Arc};

use serde_json::Value;

use atalaya::{ch::ClickHouse, promql};

use crate::alerts;

pub struct App {
    pub ch: ClickHouse,
    pub db: String,
    pub dashboards: PathBuf,
    /// overview.yaml: what the home page shows for each service
    pub overview: PathBuf,
    /// timeline.yaml: timeline lanes
    pub timeline: PathBuf,
    /// Data written by the server (notebooks, saved searches, alert state): a volume in production
    pub data: PathBuf,
    pub promql: promql::Engine,
    /// Alert rules, their state and notifications
    pub alerts: alerts::Alerts,
    /// The home page is the same for everyone: cached for 10 s per range
    pub overview_cache: tokio::sync::Mutex<std::collections::HashMap<i64, (std::time::Instant, Value)>>,
}

pub type Shared = Arc<App>;

pub fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// Seconds that panels, alerts and the home page stay behind "now"
/// (ATALAYA_NOW_DELAY, 45): the time for data to arrive, so the last rate
/// point never sees a half-filled window. 45 fits the collector's default
/// 30 s flush; with a 15 s flush, 30 is enough.
pub fn now_delay() -> i64 {
    static D: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *D.get_or_init(|| parse_secs(&env("ATALAYA_NOW_DELAY", "")).unwrap_or(45))
}

/// Seconds between automatic refreshes of the pages that are not dashboards
/// (ATALAYA_REFRESH, 60); each dashboard says its own
pub fn page_refresh() -> i64 {
    static D: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *D.get_or_init(|| parse_secs(&env("ATALAYA_REFRESH", "")).filter(|s| *s > 0).unwrap_or(60))
}

/// "30" or "30s"; nothing if empty, negative or not a number
fn parse_secs(v: &str) -> Option<i64> {
    v.trim().trim_end_matches('s').parse().ok().filter(|s| *s >= 0)
}

/// Config file path: the environment variable or, if unset, `name` next to the
/// dashboards directory
pub fn config_path(var: &str, name: &str) -> PathBuf {
    let v = env(var, "");
    if !v.is_empty() {
        return PathBuf::from(v);
    }
    let d = PathBuf::from(env("ATALAYA_DASHBOARDS", "dashboards"));
    d.parent().map(|p| p.join(name)).unwrap_or_else(|| PathBuf::from(name))
}

/// Database or table name (unquoted in SQL): only letters, digits, _ and .
pub fn ident(s: &str) -> anyhow::Result<String> {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
        Ok(s.to_owned())
    } else {
        anyhow::bail!("invalid identifier: {s}")
    }
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::parse_secs;

    #[test]
    fn seconds_from_the_environment() {
        assert_eq!(parse_secs("30"), Some(30));
        assert_eq!(parse_secs(" 15s "), Some(15));
        assert_eq!(parse_secs("0"), Some(0));
        assert_eq!(parse_secs(""), None);
        assert_eq!(parse_secs("-5"), None);
        assert_eq!(parse_secs("1m"), None);
    }
}
