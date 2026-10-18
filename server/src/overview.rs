// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The home page (Board / Table): the state of every host and service in a
//! single response. What each service shows (status line and sparkline) is
//! configured in overview.yaml; the rest comes from node-exporter, Nomad alloc
//! metrics, the series table and the logs.
//!
//! Queries end ATALAYA_NOW_DELAY before now, like the panels: the time for the
//! collector's data to arrive (a half-filled window skews rates).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::App;
use atalaya::promql::{Labels, Output};

/// Points per sparkline
const SPARK_POINTS: i64 = 48;
/// Window for errors per minute and for considering a service alive
const RECENT: i64 = 300;

#[derive(Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    defaults: ServiceCfg,
    #[serde(default)]
    services: BTreeMap<String, ServiceCfg>,
    /// node-exporter job, source of each host's CPU, memory and network
    #[serde(default = "default_node_job")]
    node_job: String,
    /// Nomad tasks whose logs are ignored (e.g. sidecars)
    #[serde(default)]
    ignore_tasks: Vec<String>,
}

fn default_node_job() -> String {
    "node-exporter".into()
}

#[derive(Deserialize, Default, Clone)]
pub struct ServiceCfg {
    /// Sparkline PromQL; `$job` and `$host` are substituted
    spark: Option<String>,
    spark_unit: Option<String>,
    /// Status line parts, joined with " · "
    #[serde(default)]
    status: Vec<StatusPart>,
    /// PromQL: if it returns anything > 0, the service is marked as warning
    warn: Option<String>,
    /// Services without `up` (e.g. the collector itself): PromQL whose series
    /// give the hosts through their instance label
    hosts_from: Option<String>,
}

/// A text in one language or a language → text map ({en: …, es: …})
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Text {
    One(String),
    ByLang(BTreeMap<String, String>),
}

impl Text {
    pub fn get(&self, lang: &str) -> &str {
        match self {
            Text::One(s) => s,
            Text::ByLang(m) => m
                .get(lang)
                .or_else(|| m.get("en"))
                .or_else(|| m.values().next())
                .map(String::as_str)
                .unwrap_or(""),
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct StatusPart {
    /// Text with `{{v}}` where the value goes (single or per language)
    text: Text,
    query: String,
    decimals: Option<usize>,
    /// bytes → GiB/MiB, secs → duration; no unit means the plain number
    unit: Option<String>,
}

pub fn load_config(path: &std::path::Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_saphyr::from_str(&s).map_err(|e| tracing::warn!("{}: {e}", path.display())).ok())
        .unwrap_or_default()
}

fn subst(q: &str, job: &str, host: &str) -> String {
    q.replace("$job", job).replace("$host", host)
}

/// Number with few decimals, using the language's separators
pub fn fmt_num(v: f64, decimals: Option<usize>, lang: &str) -> String {
    let (thousands, decimal) = if lang == "es" { ('.', ',') } else { (',', '.') };
    let d = decimals.unwrap_or(if v.abs() >= 100.0 {
        0
    } else if v.abs() >= 10.0 {
        1
    } else {
        2
    });
    let s = format!("{v:.d$}");
    let (int, frac) = s.split_once('.').map(|(a, b)| (a.to_owned(), Some(b.to_owned()))).unwrap_or((s.clone(), None));
    let neg = int.starts_with('-');
    let digits: Vec<char> = int.trim_start_matches('-').chars().collect();
    let mut out = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(thousands);
        }
        out.push(*c);
    }
    let mut r = if neg { format!("-{out}") } else { out };
    if let Some(f) = frac {
        r.push(decimal);
        r.push_str(&f);
    }
    r
}

pub fn fmt_value(v: f64, unit: Option<&str>, decimals: Option<usize>, lang: &str) -> String {
    match unit {
        Some("bytes") => {
            let units = ["B", "KiB", "MiB", "GiB", "TiB"];
            let mut x = v;
            let mut i = 0;
            while x.abs() >= 1024.0 && i < units.len() - 1 {
                x /= 1024.0;
                i += 1;
            }
            format!("{} {}", fmt_num(x, decimals.or(Some(if i >= 3 { 2 } else { 0 })), lang), units[i])
        }
        Some("secs") => {
            let s = v.round() as i64;
            if s >= 86400 {
                format!("{} d", s / 86400)
            } else if s >= 3600 {
                format!("{} h", s / 3600)
            } else if s >= 60 {
                format!("{} min", s / 60)
            } else {
                format!("{s} s")
            }
        }
        _ => fmt_num(v, decimals, lang),
    }
}

tokio::task_local! {
    /// Queries that failed while building the overview: returned in the
    /// response so the UI can say data is missing instead of silently showing gaps
    static ERRORS: std::cell::RefCell<Vec<String>>;
}

fn note_error(what: &str, e: &anyhow::Error) {
    tracing::warn!("overview: {what}: {e:#}");
    let _ = ERRORS.try_with(|v| v.borrow_mut().push(format!("{what}: {e:#}")));
}

/// `up` by (job, host). With several replicas of a job on one host (series that
/// differ in `nomad_alloc_index`), the worst one: a replica that is down shows.
fn up_by_job_host(up: &HashMap<Labels, f64>) -> BTreeMap<(String, String), Option<f64>> {
    let mut out: BTreeMap<(String, String), Option<f64>> = BTreeMap::new();
    for (l, v) in up {
        if let (Some(j), Some(h)) = (l.get("job"), l.get("instance")) {
            let e = out.entry((j.clone(), h.clone())).or_insert(Some(*v));
            *e = e.map(|w| w.min(*v));
        }
    }
    out
}

async fn instant(app: &App, q: &str, t: i64) -> HashMap<Labels, f64> {
    match app.promql.instant(q, t).await {
        Ok(r) => match r.output {
            Output::Matrix(m) => m.into_iter().filter_map(|(l, pts)| pts.last().map(|p| (l, p.1))).collect(),
            Output::Scalar(s) => s.last().map(|p| [(Labels::new(), p.1)].into()).unwrap_or_default(),
            Output::Str(_) => HashMap::new(),
        },
        Err(e) => {
            note_error(q, &e);
            HashMap::new()
        }
    }
}

async fn range(app: &App, q: &str, start: i64, end: i64, step: i64) -> Vec<(Labels, Vec<(i64, f64)>)> {
    match app.promql.range(q, start, end, step).await {
        Ok(r) => match r.output {
            Output::Matrix(m) => m,
            Output::Scalar(s) => vec![(Labels::new(), s)],
            Output::Str(_) => vec![],
        },
        Err(e) => {
            note_error(q, &e);
            vec![]
        }
    }
}

fn by(m: &HashMap<Labels, f64>, key: &str) -> HashMap<String, f64> {
    m.iter().filter_map(|(l, v)| l.get(key).map(|k| (k.clone(), *v))).collect()
}

fn spark_json(pts: &[(i64, f64)]) -> Value {
    Value::Array(pts.iter().map(|(t, v)| json!([t, (v * 1000.0).round() / 1000.0])).collect())
}

fn log_json(r: &Value) -> Value {
    json!({ "time": r["ts"].as_f64().map(|x| (x / 1000.0).floor() as i64), "service": r["service"], "level": r["level"], "body": r["body"] })
}

pub async fn overview(app: &App, range_secs: i64, lang: &str) -> Result<Value> {
    ERRORS
        .scope(Default::default(), async {
            let mut v = build(app, range_secs, lang).await?;
            let errors = ERRORS.with(|e| e.take());
            v["errors"] = json!(errors);
            Ok(v)
        })
        .await
}

async fn build(app: &App, range_secs: i64, lang: &str) -> Result<Value> {
    let lang = lang.to_owned();
    let cfg = load_config(&app.overview);
    let now = chrono::Utc::now().timestamp();
    let end = now - crate::app::now_delay();
    let start = end - range_secs;
    let step = (range_secs / SPARK_POINTS).max(60);

    // ── PromQL, all at once ─────────────────────────────────────────────────
    // The job name goes quoted into the selector: strip quotes and backslashes,
    // which would break the query
    let node_job: String = cfg.node_job.chars().filter(|c| !matches!(c, '"' | '\\')).collect();
    let ne = format!("job=\"{node_job}\"");
    let q_cpu = format!("100 * (1 - avg by (instance) (rate(node_cpu_seconds_total{{{ne},mode=\"idle\"}}[5m])))");
    let q_cores = format!("count by (instance) (node_cpu_seconds_total{{{ne},mode=\"idle\"}})");
    let q_load = format!("node_load1{{{ne}}}");
    let q_memt = format!("node_memory_MemTotal_bytes{{{ne}}}");
    let q_mema = format!("node_memory_MemAvailable_bytes{{{ne}}}");
    let q_rx = format!("sum by (instance) (rate(node_network_receive_bytes_total{{{ne},device!=\"lo\"}}[5m])) * 8");
    let q_tx = format!("sum by (instance) (rate(node_network_transmit_bytes_total{{{ne},device!=\"lo\"}}[5m])) * 8");
    let (cpu_range, cores, load1, mem_total, mem_avail, net_rx, net_tx, up, svc_cpu, svc_mem, svc_alloc, svc_spark) = tokio::join!(
        range(app, &q_cpu, start, end, step),
        instant(app, &q_cores, end),
        instant(app, &q_load, end),
        instant(app, &q_memt, end),
        instant(app, &q_mema, end),
        instant(app, &q_rx, end),
        instant(app, &q_tx, end),
        instant(app, "up", end),
        instant(app, "sum by (exported_job, instance) (nomad_client_allocs_cpu_total_percent)", end),
        instant(app, "sum by (exported_job, instance) (nomad_client_allocs_memory_rss)", end),
        instant(app, "max by (exported_job, instance, alloc_id) (nomad_client_allocs_cpu_total_percent)", end),
        range(app, "sum by (exported_job, instance) (nomad_client_allocs_cpu_total_percent)", start, end, step),
    );

    // ── ClickHouse: series, errors and last log per host and per service ───
    let db = &app.db;
    let params = vec![
        ("from".into(), (end - RECENT).to_string()),
        ("end".into(), end.to_string()),
        ("ignore".into(), atalaya::ch::array_param(&cfg.ignore_tasks)),
    ];
    let sql_series = format!(
        "SELECT Resource['host.name'] AS host, Service AS service, count() AS n FROM {db}.series FINAL
         WHERE LastSeen >= toDateTime({{from:Int64}}) - 7200 GROUP BY host, service"
    );
    let sql_err = format!(
        "SELECT Host AS host, Service AS service, count() AS n FROM {db}.logs
         WHERE Level IN ('ERROR', 'FATAL') AND Time >= fromUnixTimestamp64Milli({{from:Int64}} * 1000) AND Time < fromUnixTimestamp64Milli({{end:Int64}} * 1000)
         GROUP BY host, service"
    );
    // The last day, subtracted before converting: an integer subtracted from a
    // DateTime64 counts seconds, whatever its precision
    let sql_last = format!(
        "SELECT Host AS host, Service AS service, toUnixTimestamp64Milli(Time) AS ts, Level AS level, substring(Body, 1, 300) AS body
         FROM {db}.logs WHERE Time >= fromUnixTimestamp64Milli(({{end:Int64}} - 86400) * 1000)
           AND NOT has({{ignore:Array(String)}}, Resource['nomad.task'])
         ORDER BY Time DESC LIMIT 1 BY host, service"
    );
    let (series_rows, err_rows, last_rows, points_rate) = tokio::join!(
        app.ch.rows(&sql_series, &params),
        app.ch.rows(&sql_err, &params),
        app.ch.rows(&sql_last, &params),
        instant(app, "sum(rate(atalaya_collector_points_total[5m]))", end),
    );
    let series_rows = series_rows.context("series")?;
    let err_rows = err_rows.context("errors")?;
    let last_rows = last_rows.context("last log")?;

    // Per host and per service@host (node-exporter and nomad run on every host)
    let sk = |r: &Value| format!("{}@{}", r["service"].as_str().unwrap_or_default(), r["host"].as_str().unwrap_or_default());
    let mut series_host: HashMap<String, u64> = HashMap::new();
    let mut series_svc: HashMap<String, u64> = HashMap::new();
    for r in &series_rows {
        let n = r["n"].as_u64().unwrap_or(0);
        *series_host.entry(r["host"].as_str().unwrap_or_default().to_owned()).or_default() += n;
        *series_svc.entry(sk(r)).or_default() += n;
    }
    let mut err_host: HashMap<String, f64> = HashMap::new();
    let mut err_svc: HashMap<String, f64> = HashMap::new();
    for r in &err_rows {
        let per_min = r["n"].as_f64().unwrap_or(0.0) / (RECENT as f64 / 60.0);
        *err_host.entry(r["host"].as_str().unwrap_or_default().to_owned()).or_default() += per_min;
        *err_svc.entry(sk(r)).or_default() += per_min;
    }
    // Last log per host (the most recent of its services) and per service@host
    let mut last_host: HashMap<String, &Value> = HashMap::new();
    let mut last_svc: HashMap<String, &Value> = HashMap::new();
    for r in &last_rows {
        let h = r["host"].as_str().unwrap_or_default();
        let ts = r["ts"].as_f64().unwrap_or(0.0);
        if last_host.get(h).is_none_or(|p| p["ts"].as_f64().unwrap_or(0.0) < ts) {
            last_host.insert(h.to_owned(), r);
        }
        last_svc.insert(sk(r), r);
    }

    // ── Hosts ───────────────────────────────────────────────────────────────
    let cores = by(&cores, "instance");
    let load1 = by(&load1, "instance");
    let mem_total = by(&mem_total, "instance");
    let mem_avail = by(&mem_avail, "instance");
    let net_rx = by(&net_rx, "instance");
    let net_tx = by(&net_tx, "instance");
    let mut hosts: Vec<Value> = Vec::new();
    let mut host_names: Vec<String> = cores.keys().cloned().collect();
    host_names.sort();
    for h in &host_names {
        let spark = cpu_range
            .iter()
            .find(|(l, _)| l.get("instance") == Some(h))
            .map(|(_, p)| p.as_slice())
            .unwrap_or_default();
        let cpu_pct = spark.last().map(|p| p.1);
        let total = mem_total.get(h).copied();
        let used = total.zip(mem_avail.get(h).copied()).map(|(t, a)| t - a);
        hosts.push(json!({
            "name": h, "cores": cores.get(h), "load1": load1.get(h), "cpu_pct": cpu_pct,
            "mem_used_bytes": used, "mem_total_bytes": total,
            "net_rx_bps": net_rx.get(h), "net_tx_bps": net_tx.get(h),
            "series": series_host.get(h).copied().unwrap_or(0), "err_per_min": err_host.get(h).copied().unwrap_or(0.0),
            "last_log": last_host.get(h).map(|r| log_json(r)),
            "cpu_spark": spark_json(spark),
        }));
    }

    // ── Services: those with `up` plus the configured ones ─────────────────
    let key = |l: &Labels, job: &str| format!("{}@{}", l.get(job).cloned().unwrap_or_default(), l.get("instance").cloned().unwrap_or_default());
    let svc_cpu: HashMap<String, f64> = svc_cpu.iter().map(|(l, v)| (key(l, "exported_job"), *v)).collect();
    let svc_mem: HashMap<String, f64> = svc_mem.iter().map(|(l, v)| (key(l, "exported_job"), *v)).collect();
    let mut svc_alloc_id: HashMap<String, String> = HashMap::new();
    for l in svc_alloc.keys() {
        if let Some(a) = l.get("alloc_id") {
            svc_alloc_id.entry(key(l, "exported_job")).or_insert_with(|| a.chars().take(8).collect());
        }
    }
    let default_spark: HashMap<String, &[(i64, f64)]> = svc_spark.iter().map(|(l, p)| (key(l, "exported_job"), p.as_slice())).collect();

    let mut entries = up_by_job_host(&up);
    // Configured services without `up` (e.g. the collector itself): their hosts
    // come from the configured hosts_from query
    for (job, c) in &cfg.services {
        if entries.keys().any(|(j, _)| j == job) {
            continue;
        }
        if let Some(q) = &c.hosts_from {
            for (l, _) in instant(app, &subst(q, job, ""), end).await {
                if let Some(h) = l.get("instance") {
                    entries.insert((job.clone(), h.clone()), None);
                }
            }
        }
    }

    // Configured sparklines and status lines, in parallel
    let mut futs = Vec::new();
    for (job, host) in entries.keys() {
        let c = cfg.services.get(job).cloned().unwrap_or_default();
        let d = &cfg.defaults;
        let spark_q = c.spark.clone().or_else(|| d.spark.clone());
        let spark_unit = c.spark_unit.clone().or_else(|| d.spark_unit.clone()).unwrap_or_else(|| "percent".into());
        let status_parts = if c.status.is_empty() { d.status.clone() } else { c.status.clone() };
        let warn_q = c.warn.clone().or_else(|| d.warn.clone());
        let (job, host) = (job.clone(), host.clone());
        let lang = lang.clone();
        futs.push(async move {
            // Without a query of its own or in `defaults`, the shared allocation-CPU one
            let spark = match &spark_q {
                None => Vec::new(),
                Some(q) => range(app, &subst(q, &job, &host), start, end, step)
                    .await
                    .into_iter()
                    .find(|(l, _)| l.get("instance").is_none_or(|i| i == &host))
                    .map(|(_, p)| p)
                    .unwrap_or_default(),
            };
            let mut parts = Vec::new();
            for p in &status_parts {
                let r = instant(app, &subst(&p.query, &job, &host), end).await;
                let v = r.iter().find(|(l, _)| l.get("instance").is_none_or(|i| i == &host)).map(|(_, v)| *v);
                if let Some(v) = v {
                    parts.push(p.text.get(&lang).replace("{{v}}", &fmt_value(v, p.unit.as_deref(), p.decimals, &lang)));
                }
            }
            let warn = match warn_q {
                Some(q) => instant(app, &subst(&q, &job, &host), end).await.values().any(|v| *v > 0.0),
                None => false,
            };
            (job, host, spark, spark_unit, parts.join(" · "), warn)
        });
    }
    let results = futures::future::join_all(futs).await;

    let mut services = Vec::new();
    for (job, host, spark, spark_unit, status, warn) in results {
        let k = format!("{job}@{host}");
        let up_v = entries.get(&(job.clone(), host.clone())).copied().flatten();
        let spark: Vec<(i64, f64)> = if spark.is_empty() {
            default_spark.get(&k).map(|p| p.to_vec()).unwrap_or_default()
        } else {
            spark
        };
        let err = err_svc.get(&k).copied().unwrap_or(0.0);
        let health = match up_v {
            Some(v) if v < 1.0 => "bad",
            None if spark.last().is_none_or(|p| p.0 < end - RECENT * 2) => "unknown",
            _ if warn || err > 0.0 => "warn",
            _ => "ok",
        };
        services.push(json!({
            "job": job, "host": host, "alloc": svc_alloc_id.get(&k), "up": up_v.map(|v| v as i64),
            "health": health, "series": series_svc.get(&k).copied().unwrap_or(0),
            "cpu_pct": svc_cpu.get(&k), "mem_bytes": svc_mem.get(&k), "err_per_min": err,
            "status": status, "last_log": last_svc.get(&k).map(|r| log_json(r)),
            "spark_unit": spark_unit, "spark": spark_json(&spark),
        }));
    }
    let order = |h: &str| match h {
        "bad" => 0,
        "warn" => 1,
        "ok" => 2,
        _ => 3,
    };
    services.sort_by(|a, b| {
        order(a["health"].as_str().unwrap_or(""))
            .cmp(&order(b["health"].as_str().unwrap_or("")))
            .then_with(|| a["host"].as_str().cmp(&b["host"].as_str()))
            .then_with(|| a["job"].as_str().cmp(&b["job"].as_str()))
    });

    let targets_total = services.iter().filter(|s| !s["up"].is_null()).count();
    let targets_up = services.iter().filter(|s| s["up"].as_i64() == Some(1)).count();
    let warnings = services.iter().filter(|s| matches!(s["health"].as_str(), Some("warn") | Some("bad"))).count();
    Ok(json!({
        "time": end, "range": range_secs,
        "summary": {
            "targets_up": targets_up, "targets_total": targets_total,
            "series": series_svc.values().sum::<u64>(),
            "points_per_s": points_rate.values().next().copied(),
            "warnings": warnings,
        },
        "hosts": hosts, "services": services,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(replica: &str, v: f64) -> (Labels, f64) {
        let l: Labels = [("job", "web"), ("instance", "node-1"), ("nomad_alloc_index", replica)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (l, v)
    }

    #[test]
    fn a_replica_that_is_down_shows_on_its_service() {
        // Every HashMap gets its own hash seed, so the iteration order changes: a
        // "last one wins" would get it right only by chance
        for _ in 0..32 {
            let ups: HashMap<Labels, f64> = [up("0", 1.0), up("1", 0.0)].into_iter().collect();
            let got = up_by_job_host(&ups);
            assert_eq!(got.len(), 1);
            assert_eq!(got[&("web".to_string(), "node-1".to_string())], Some(0.0));
        }
    }
}
