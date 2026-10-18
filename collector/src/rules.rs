// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Metric rules applied at the source, before anything is sent:
//!   allow       per service, only the listed metrics are kept (the rest is dropped)
//!   drop        metrics that are not stored
//!   drop_series specific series (metric + label + value)
//!   aggregate   sum while removing labels (e.g. per-core CPU → per host)
//!   slow        metrics sent less often

use std::collections::HashMap;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

use crate::prom::{Labels, Scalar, Scrape};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesConfig {
    #[serde(default)]
    pub allow: Vec<Allow>,
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default)]
    pub drop_series: Vec<DropSeries>,
    #[serde(default)]
    pub aggregate: Vec<Aggregate>,
    #[serde(default)]
    pub slow: Option<Slow>,
}

/// Allowlist for a service: only metrics matching some `keep` regex pass from
/// its scrapes (histograms by their base name)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    pub service: String,
    pub keep: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropSeries {
    pub metric: String,
    pub label: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregate {
    pub metric: String,
    pub without: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slow {
    #[serde(with = "humantime_secs")]
    pub interval: Duration,
    pub metrics: Vec<String>,
}

mod humantime_secs {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        crate::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

fn anchored(re: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!("^(?:{re})$"))
}

pub struct Rules {
    allow: Vec<(Regex, Vec<Regex>)>,
    drop: Vec<Regex>,
    drop_series: Vec<(Regex, String, Regex)>,
    aggregate: Vec<(Regex, Vec<String>)>,
    slow: Option<(Duration, Vec<Regex>)>,
    last_slow: HashMap<String, Instant>,
    slow_pruned: Instant,
}

impl Rules {
    pub fn new(cfg: &RulesConfig) -> anyhow::Result<Self> {
        Ok(Self {
            allow: cfg
                .allow
                .iter()
                .map(|a| Ok((anchored(&a.service)?, a.keep.iter().map(|r| anchored(r)).collect::<Result<_, _>>()?)))
                .collect::<Result<_, regex::Error>>()?,
            drop: cfg.drop.iter().map(|r| anchored(r)).collect::<Result<_, _>>()?,
            drop_series: cfg
                .drop_series
                .iter()
                .map(|d| Ok((anchored(&d.metric)?, d.label.clone(), anchored(&d.value)?)))
                .collect::<Result<_, regex::Error>>()?,
            aggregate: cfg
                .aggregate
                .iter()
                .map(|a| Ok((anchored(&a.metric)?, a.without.clone())))
                .collect::<Result<_, regex::Error>>()?,
            slow: match &cfg.slow {
                Some(s) => Some((s.interval, s.metrics.iter().map(|r| anchored(r)).collect::<Result<_, _>>()?)),
                None => None,
            },
            last_slow: HashMap::new(),
            slow_pruned: Instant::now(),
        })
    }

    /// Forgets slow metrics not let through for two intervals: a live one passes
    /// every interval, so only sources and metrics that are gone are dropped
    fn prune_slow(&mut self, now: Instant) {
        let Some((interval, _)) = &self.slow else { return };
        if now.duration_since(self.slow_pruned) < *interval {
            return;
        }
        let keep = *interval * 2;
        self.slow_pruned = now;
        self.last_slow.retain(|_, t| now.duration_since(*t) <= keep);
    }

    /// Applies the rules to a scrape of `service`. `target` identifies the
    /// source for tracking slow metrics.
    pub fn apply(&mut self, service: &str, target: &str, mut s: Scrape) -> Scrape {
        let now = Instant::now();
        self.prune_slow(now);
        // The service's allowlist (if any) is evaluated before everything else
        let allowed: Vec<&Vec<Regex>> = self.allow.iter().filter(|(svc, _)| svc.is_match(service)).map(|(_, k)| k).collect();
        if !allowed.is_empty() {
            let pass = |metric: &str| allowed.iter().any(|keep| keep.iter().any(|r| r.is_match(metric)));
            s.scalars.retain(|x| pass(&x.metric));
            s.distributions.retain(|x| pass(&x.metric));
        }
        // Slow metrics not yet due for this source
        let mut skip_slow = |metric: &str| -> bool {
            let Some((interval, res)) = &self.slow else { return false };
            if !res.iter().any(|r| r.is_match(metric)) {
                return false;
            }
            let key = format!("{target}\u{1f}{metric}");
            match self.last_slow.get(&key) {
                Some(t) if now.duration_since(*t) < *interval => true,
                _ => {
                    self.last_slow.insert(key, now);
                    false
                }
            }
        };
        let mut slow_decision: HashMap<String, bool> = HashMap::new();
        let mut keep = |metric: &str, labels: &Labels| -> bool {
            if self.drop.iter().any(|r| r.is_match(metric)) {
                return false;
            }
            if self
                .drop_series
                .iter()
                .any(|(m, l, v)| m.is_match(metric) && labels.get(l).is_some_and(|x| v.is_match(x)))
            {
                return false;
            }
            !*slow_decision.entry(metric.to_owned()).or_insert_with(|| skip_slow(metric))
        };

        s.scalars.retain(|x| keep(&x.metric, &x.labels));
        s.distributions.retain(|x| keep(&x.metric, &x.labels));

        // Aggregation: sum the series that differ only in the removed labels
        if !self.aggregate.is_empty() {
            let mut merged: Vec<Scalar> = Vec::with_capacity(s.scalars.len());
            let mut index: HashMap<(String, Labels), usize> = HashMap::new();
            for mut x in s.scalars.drain(..) {
                if let Some((_, without)) = self.aggregate.iter().find(|(r, _)| r.is_match(&x.metric)) {
                    for l in without {
                        x.labels.remove(l);
                    }
                    let key = (x.metric.clone(), x.labels.clone());
                    if let Some(&i) = index.get(&key) {
                        merged[i].value += x.value;
                        continue;
                    }
                    index.insert(key, merged.len());
                }
                merged.push(x);
            }
            s.scalars = merged;
        }
        s.descriptors.retain(|m, _| !self.drop.iter().any(|r| r.is_match(m)));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prom::Scalar;

    fn scrape(names: &[&str]) -> Scrape {
        Scrape {
            scalars: names
                .iter()
                .map(|n| Scalar {
                    metric: n.to_string(),
                    labels: Labels::default(),
                    value: 1.0,
                    counter: false,
                })
                .collect(),
            distributions: vec![],
            descriptors: HashMap::new(),
            ..Scrape::default()
        }
    }

    #[test]
    fn slow_metrics_of_sources_that_are_gone_are_forgotten() {
        let cfg: RulesConfig = serde_saphyr::from_str("slow:\n  interval: 5m\n  metrics: ['big_.*']\n").unwrap();
        let mut r = Rules::new(&cfg).unwrap();
        let start = Instant::now();
        r.slow_pruned = start;
        r.last_slow.insert("old-target\u{1f}big_x".into(), start);
        r.last_slow.insert("live-target\u{1f}big_x".into(), start + Duration::from_secs(540));
        // Not due before an interval has passed
        r.prune_slow(start + Duration::from_secs(120));
        assert_eq!(r.last_slow.len(), 2);
        r.prune_slow(start + Duration::from_secs(660));
        assert_eq!(r.last_slow.keys().map(String::as_str).collect::<Vec<_>>(), ["live-target\u{1f}big_x"]);
    }

    #[test]
    fn allow_keeps_only_listed_metrics_of_that_service() {
        let cfg: RulesConfig =
            serde_saphyr::from_str("allow:\n  - service: clickhouse\n    keep: ['ClickHouseMetrics_Query', 'ClickHouseAsyncMetrics_Disk.*', 'up']\n").unwrap();
        let mut rules = Rules::new(&cfg).unwrap();
        let out = rules.apply(
            "clickhouse",
            "t",
            scrape(&[
                "ClickHouseMetrics_Query",
                "ClickHouseMetrics_Other",
                "ClickHouseAsyncMetrics_DiskUsed_default",
                "up",
            ]),
        );
        let names: Vec<&str> = out.scalars.iter().map(|x| x.metric.as_str()).collect();
        assert_eq!(names, vec!["ClickHouseMetrics_Query", "ClickHouseAsyncMetrics_DiskUsed_default", "up"]);
        // Another service has no allowlist: everything passes
        let out = rules.apply("nomad", "t", scrape(&["anything_goes"]));
        assert_eq!(out.scalars.len(), 1);
    }
}
