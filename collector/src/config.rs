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
    pub(crate) clickhouse: ClickHouseConfig,
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
        parse_duration(&self.clickhouse.flush_interval).context("clickhouse.flush_interval")?;
        parse_duration(&self.host.smart.interval).context("host.smart.interval")?;
        anyhow::ensure!(!self.node.trim().is_empty(), "node is empty");
        if let Some(s) = &self.syslog {
            s.listen
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("syslog.listen: {}", s.listen))?;
        }
        Ok(())
    }
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
}
