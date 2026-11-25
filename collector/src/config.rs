// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Collector configuration (the YAML file given as argument).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::rules::RulesConfig;

pub(crate) fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("invalid duration: {s:?}"))?;
    let d = Duration::from_secs(match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => anyhow::bail!("invalid unit: {s}"),
    });
    // A zero interval would make tokio::time::interval panic
    anyhow::ensure!(!d.is_zero(), "duration must be greater than 0: {s}");
    Ok(d)
}

pub(crate) fn default_interval() -> String {
    "15s".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Node name (host.name of everything this collector sends)
    pub(crate) node: String,
    /// Where it sends: ClickHouse, Victoria (VictoriaMetrics and VictoriaLogs) or both at once
    #[serde(default)]
    pub(crate) clickhouse: Option<ClickHouseConfig>,
    #[serde(default)]
    pub(crate) victoria: Option<VictoriaConfig>,
    /// Optional: with Nomad the node's services are discovered and their
    /// allocation logs are tailed; without it, static targets
    /// (scrape.static_targets), host metrics and Docker logs
    #[serde(default)]
    pub(crate) nomad: Option<NomadConfig>,
    #[serde(default)]
    pub(crate) scrape: ScrapeConfig,
    #[serde(default)]
    pub(crate) rules: RulesConfig,
    #[serde(default)]
    pub(crate) logs: LogsConfig,
    #[serde(default)]
    pub(crate) host: HostConfig,
    /// Optional syslog receiver (network devices sending their logs)
    #[serde(default)]
    pub(crate) syslog: Option<crate::syslog::SyslogConfig>,
    /// Log offsets and the disk spool
    pub(crate) state_dir: PathBuf,
}

impl Config {
    /// Everything that can be checked at startup, so it fails then and not
    /// when the task that uses it gets there
    pub(crate) fn validate(&self) -> Result<()> {
        parse_duration(&self.scrape.interval).context("scrape.interval")?;
        anyhow::ensure!(
            self.clickhouse.is_some() || self.victoria.is_some(),
            "nowhere to send: configure clickhouse, victoria or both"
        );
        if let Some(c) = &self.clickhouse {
            parse_duration(&c.flush_interval).context("clickhouse.flush_interval")?;
            check_url("clickhouse.url", &c.url)?;
            anyhow::ensure!(c.metrics || c.logs, "clickhouse sends neither metrics nor logs: remove the section instead");
        }
        // Taking metrics or logs away from ClickHouse without sending them to Victoria would drop
        // them quietly: a configuration mistake
        if let Some(c) = &self.clickhouse {
            let vm = self.victoria.as_ref().is_some_and(|v| v.metrics_url.is_some());
            let vl = self.victoria.as_ref().is_some_and(|v| v.logs_url.is_some());
            anyhow::ensure!(
                c.metrics || vm,
                "clickhouse.metrics is false and there is no victoria.metrics_url: metrics would go nowhere"
            );
            anyhow::ensure!(
                c.logs || vl,
                "clickhouse.logs is false and there is no victoria.logs_url: logs would go nowhere"
            );
        }
        if let Some(v) = &self.victoria {
            parse_duration(&v.flush_interval).context("victoria.flush_interval")?;
            anyhow::ensure!(v.metrics_url.is_some() || v.logs_url.is_some(), "victoria needs metrics_url, logs_url or both");
            anyhow::ensure!(v.user.as_deref().is_none_or(|u| !u.is_empty()), "victoria.user is empty");
            for (name, url) in [("victoria.metrics_url", &v.metrics_url), ("victoria.logs_url", &v.logs_url)] {
                if let Some(u) = url {
                    check_url(name, u)?;
                }
            }
        }
        parse_duration(&self.host.smart.interval).context("host.smart.interval")?;
        anyhow::ensure!(!self.node.trim().is_empty(), "node is empty");
        if let Some(s) = &self.syslog {
            s.listen
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("syslog.listen: {}", s.listen))?;
        }
        Ok(())
    }

    /// How often buffered rows are sent: ClickHouse's interval when it is configured (it decides
    /// how many parts are created), else Victoria's
    pub(crate) fn flush_interval(&self) -> &str {
        match (&self.clickhouse, &self.victoria) {
            (Some(c), _) => &c.flush_interval,
            (None, Some(v)) => &v.flush_interval,
            (None, None) => "15s",
        }
    }
}

/// A Nomad template that resolves a URL with nomadService can render it empty ("http:///") while
/// the allocation starts: better to exit and let Nomad restart the collector when the template
/// re-renders than to spool batches that can never be sent
///
/// Credentials never go in a URL: errors and logs print URLs, so they go in their own fields
fn check_url(name: &str, url: &str) -> Result<()> {
    // Not even an invalid URL is printed if it may carry a password
    let shown = if url.contains('@') { "<hidden: it has an @>" } else { url };
    let u = reqwest::Url::parse(url).with_context(|| format!("invalid {name}: {shown}"))?;
    if !u.username().is_empty() || u.password().is_some() {
        anyhow::bail!("{name} carries credentials: put the user in victoria.user and the password in VICTORIA_PASSWORD");
    }
    if u.host_str().is_none_or(str::is_empty) {
        anyhow::bail!("{name} has no host ({url}): is its Nomad service not resolved yet?");
    }
    Ok(())
}

/// VictoriaMetrics (points, to /api/v1/import) and VictoriaLogs (log lines, to /insert/jsonline),
/// with its own disk spool (state_dir/spool-victoria)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VictoriaConfig {
    /// VictoriaMetrics' base URL (http://host:8428)
    #[serde(default)]
    pub(crate) metrics_url: Option<String>,
    /// VictoriaLogs' base URL (http://host:9428)
    #[serde(default)]
    pub(crate) logs_url: Option<String>,
    /// Basic auth user for both URLs (-httpAuth.username of vminsert / VictoriaLogs); the password
    /// comes from VICTORIA_PASSWORD, like ClickHouse's, so the config file holds no secret
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default = "default_spool")]
    pub(crate) max_spool_mb: u64,
    /// Used when there is no ClickHouse: Victoria takes small batches well
    #[serde(default = "default_interval")]
    pub(crate) flush_interval: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClickHouseConfig {
    pub(crate) url: String,
    pub(crate) user: String,
    #[serde(default = "default_db")]
    pub(crate) database: String,
    #[serde(default = "default_spool")]
    pub(crate) max_spool_mb: u64,
    /// How often buffered rows are sent. Large batches = few parts in
    /// ClickHouse: each send creates one per table and level, and merging them
    /// is what writes the most to disk.
    #[serde(default = "default_flush")]
    pub(crate) flush_interval: String,
    /// What goes to ClickHouse: metrics, logs or both (both by default). With metrics on
    /// VictoriaMetrics, logs alone keep ClickHouse's logs table without its metrics tables
    #[serde(default = "yes")]
    pub(crate) metrics: bool,
    #[serde(default = "yes")]
    pub(crate) logs: bool,
}
pub(crate) fn default_flush() -> String {
    "30s".into()
}
pub(crate) fn default_db() -> String {
    "atalaya".into()
}
pub(crate) fn default_spool() -> u64 {
    1024
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NomadConfig {
    pub(crate) addr: String,
    pub(crate) node_id: String,
    /// CA of the cluster's TLS (an https addr): also trusted by the scrapes,
    /// since the agent's own metrics come from the same API
    #[serde(default)]
    pub(crate) ca_file: Option<PathBuf>,
    /// ACL token for the Nomad API, read once at start
    #[serde(default)]
    pub(crate) token_file: Option<PathBuf>,
    /// Bearer tokens for scrapes: a service tagged `metrics_token=<name>` is
    /// scraped with the one in `<dir>/<namespace>/<job>/<name>`, its
    /// allocation's namespace and job (the parent of a dispatched or periodic
    /// one), read on every scrape
    #[serde(default)]
    pub(crate) scrape_tokens: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrapeConfig {
    #[serde(default = "default_interval")]
    pub(crate) interval: String,
    #[serde(default)]
    pub(crate) static_targets: Vec<StaticTarget>,
}

// Written by hand: with derive(Default), omitting the whole `scrape` section
// would leave the interval empty (serde's field default only applies when the
// section is present)
impl Default for ScrapeConfig {
    fn default() -> Self {
        Self {
            interval: default_interval(),
            static_targets: Vec::new(),
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct StaticTarget {
    pub(crate) service: String,
    pub(crate) url: String,
    /// Bearer token its scrape sends, read on every scrape like the Nomad
    /// ones (nomad.scrape_tokens): a target that asks for one that cannot be
    /// read fails, unsent
    #[serde(default)]
    pub(crate) token_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogsConfig {
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    /// Nomad allocation logs (only with the nomad section)
    #[serde(default = "default_alloc_dir")]
    pub(crate) alloc_dir: PathBuf,
    /// Docker container logs (json-file driver), e.g.
    /// /var/lib/docker/containers mounted read-only
    #[serde(default)]
    pub(crate) docker_dir: Option<PathBuf>,
}
impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alloc_dir: default_alloc_dir(),
            docker_dir: None,
        }
    }
}
/// Metrics of the host itself read from sysfs (service `host`)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostConfig {
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    /// Where the host's root filesystem is mounted
    #[serde(default = "default_host_root")]
    pub(crate) root: PathBuf,
    #[serde(default)]
    pub(crate) smart: SmartConfig,
    #[serde(default)]
    pub(crate) chrony: ChronyConfig,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: default_host_root(),
            smart: SmartConfig::default(),
            chrony: ChronyConfig::default(),
        }
    }
}

/// Time synchronisation from chronyd's command port, read on every scrape
/// (the collector needs the host's network to reach it on localhost)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChronyConfig {
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    #[serde(default = "default_chrony_address")]
    pub(crate) address: std::net::SocketAddr,
}
impl Default for ChronyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            address: default_chrony_address(),
        }
    }
}
pub(crate) fn default_chrony_address() -> std::net::SocketAddr {
    ([127, 0, 0, 1], 323).into()
}

/// Disk health via smartctl (needs a privileged container)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SmartConfig {
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    #[serde(default = "default_smart_interval")]
    pub(crate) interval: String,
    #[serde(default = "default_smartctl")]
    pub(crate) smartctl: String,
}
impl Default for SmartConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: default_smart_interval(),
            smartctl: default_smartctl(),
        }
    }
}
/// SMART changes very slowly: one reading per hour is enough and does not load
/// the disks (panels read the latest value with last_over_time). NVMe
/// temperature arrives on every scrape through hwmon.
pub(crate) fn default_smart_interval() -> String {
    "1h".into()
}
pub(crate) fn default_smartctl() -> String {
    "smartctl".into()
}
pub(crate) fn default_host_root() -> PathBuf {
    PathBuf::from("/host")
}

pub(crate) fn yes() -> bool {
    true
}
pub(crate) fn default_alloc_dir() -> PathBuf {
    PathBuf::from("/var/lib/nomad/alloc")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Compose example's collector.yaml (no Nomad) loads and validates
    #[test]
    fn example_config_is_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/compose/collector.yaml");
        let cfg: Config = serde_saphyr::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.nomad.is_none());
        assert!(cfg.logs.docker_dir.is_some());
        assert_eq!(cfg.scrape.static_targets.len(), 1);
    }

    #[test]
    fn a_static_target_may_name_its_token_file() {
        let yaml = "node: h\nstate_dir: /s\nclickhouse: { url: 'http://c:8123/', user: u }\nscrape:\n  static_targets:\n    - { service: plane, url: 'http://127.0.0.1:8080/metrics', token_file: /run/atalaya/plane-token }\n    - { service: web, url: 'http://127.0.0.1:9000/metrics' }\n";
        let cfg: Config = serde_saphyr::from_str(yaml).unwrap();
        let t = &cfg.scrape.static_targets;
        assert_eq!(t[0].token_file.as_deref(), Some(std::path::Path::new("/run/atalaya/plane-token")));
        assert!(t[1].token_file.is_none());
    }

    #[test]
    fn a_misspelt_key_is_an_error_that_names_it() {
        // Ignored, it silently left the setting at its default
        for (yaml, key) in [
            (
                "node: h\nstate_dir: /s\nclickhouse: { url: 'http://c:8123/', user: u, flush_intervl: 10s }\n",
                "flush_intervl",
            ),
            (
                "node: h\nstate_dir: /s\nclickhouse: { url: 'http://c:8123/', user: u }\nrules: { dorp: ['x'] }\n",
                "dorp",
            ),
            (
                "node: h\nstate_dir: /s\nclickhouse: { url: 'http://c:8123/', user: u }\nsyslog: { port: 514 }\n",
                "port",
            ),
        ] {
            let err = match serde_saphyr::from_str::<Config>(yaml) {
                Ok(_) => panic!("{key} was accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn scrape_section_is_optional() {
        let cfg: Config = serde_saphyr::from_str("node: h\nstate_dir: /s\nclickhouse: { url: 'http://c:8123/', user: u }\n").unwrap();
        cfg.validate().unwrap();
        assert_eq!(parse_duration(&cfg.scrape.interval).unwrap(), Duration::from_secs(15));
    }

    #[test]
    fn victoria_alone_or_with_clickhouse_is_valid_and_nothing_is_an_error() {
        let base = "node: h\nstate_dir: /s\n";
        let ok = [
            "victoria: { metrics_url: 'http://vm:8428', logs_url: 'http://vl:9428' }\n",
            "victoria: { logs_url: 'http://vl:9428' }\n",
            "clickhouse: { url: 'http://c:8123/', user: u }\nvictoria: { metrics_url: 'http://vm:8428' }\n",
            // Metrics only to VictoriaMetrics, logs only to ClickHouse
            "clickhouse: { url: 'http://c:8123/', user: u, metrics: false }\nvictoria: { metrics_url: 'http://vm:8428' }\n",
        ];
        for y in ok {
            let cfg: Config = serde_saphyr::from_str(&format!("{base}{y}")).unwrap();
            cfg.validate().unwrap_or_else(|e| panic!("{y}: {e:#}"));
        }
        let only_v: Config = serde_saphyr::from_str(&format!("{base}{}", ok[0])).unwrap();
        assert_eq!(only_v.flush_interval(), "15s", "Victoria's own interval without ClickHouse");
        let both: Config = serde_saphyr::from_str(&format!(
            "{base}clickhouse: {{ url: 'http://c:8123/', user: u, flush_interval: 30s }}\nvictoria: {{ metrics_url: 'http://vm:8428' }}\n"
        ))
        .unwrap();
        assert_eq!(both.flush_interval(), "30s", "ClickHouse's when it is there");
        for (y, msg) in [
            ("", "nowhere to send"),
            ("victoria: {}\n", "metrics_url, logs_url or both"),
            ("victoria: { metrics_url: 'http:///' }\n", "victoria.metrics_url"),
            (
                "victoria: { metrics_url: 'http://w:secret@vm:8480/insert/0/prometheus' }\n",
                "carries credentials",
            ),
            ("victoria: { metrics_url: 'http://vm:8428', user: '' }\n", "victoria.user is empty"),
            ("clickhouse: { url: 'http://c:8123/', user: u, metrics: false }\n", "metrics would go nowhere"),
            (
                "clickhouse: { url: 'http://c:8123/', user: u, logs: false }\nvictoria: { metrics_url: 'http://vm:8428' }\n",
                "logs would go nowhere",
            ),
            (
                "clickhouse: { url: 'http://c:8123/', user: u, metrics: false, logs: false }\n",
                "neither metrics nor logs",
            ),
        ] {
            let cfg: Config = serde_saphyr::from_str(&format!("{base}{y}")).unwrap();
            let e = format!("{:#}", cfg.validate().unwrap_err());
            assert!(e.contains(msg), "{y}: {e}");
        }
    }
}
