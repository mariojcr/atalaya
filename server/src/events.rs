// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The timeline: what happened in a time range, ordered by time.
//!
//! Events (no rules to configure, they are derived from stored data):
//!   - Nomad allocs that start (deployments) or stop reporting, from the first
//!     and last sample of their metrics;
//!   - restarts, from increments of nomad_client_allocs_restart;
//!   - log errors and line bursts, per service and bucket;
//!   - real alerts (alerts.yaml) that fire or resolve.
//!
//! Lanes: log lines per service and bucket (total and errors) and the metrics
//! from timeline.yaml.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::App;
use atalaya::ch::Param;
use atalaya::promql::Output;

const MAX_EVENTS: usize = 400;

#[derive(Deserialize, Default)]
pub struct Config {
    /// Metric lanes: title, PromQL and unit
    #[serde(default)]
    lanes: Vec<Lane>,
    /// Services with a log lane; if empty, the ones with most lines in the range
    #[serde(default)]
    log_lanes: Vec<String>,
    /// Nomad tasks to ignore (e.g. sidecars)
    #[serde(default)]
    ignore_tasks: Vec<String>,
}

#[derive(Deserialize, Clone)]
pub struct Lane {
    title: crate::overview::Text,
    query: String,
    #[serde(default)]
    unit: String,
}

pub fn load_config(path: &std::path::Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_saphyr::from_str(&s).map_err(|e| tracing::warn!("{}: {e}", path.display())).ok())
        .unwrap_or_default()
}

/// Points table and resolution. The age of the start decides first (a short
/// range from two months ago only exists in the 10 min tier), then the span,
/// using the same retentions as the PromQL engine.
fn tier(start: i64, end: i64, now: i64) -> (&'static str, &'static str, i64) {
    use atalaya::promql::store::{ONE_MINUTE_DAYS, RAW_DAYS};
    let (range, age) = (end - start, now - start);
    if range <= 86400 && age <= RAW_DAYS * 86400 {
        ("points", "Value", 15)
    } else if range <= ONE_MINUTE_DAYS * 86400 && age <= ONE_MINUTE_DAYS * 86400 {
        ("points_1m", "Max", 60)
    } else {
        ("points_10m", "Max", 600)
    }
}

fn short(alloc: &str) -> String {
    alloc.chars().take(8).collect()
}

/// Event texts per language
fn tr(lang: &str, key: &str) -> &'static str {
    let es = lang == "es";
    match key {
        "new_alloc" => {
            if es {
                "alloc nueva"
            } else {
                "new alloc"
            }
        }
        "new_alloc_detail" => {
            if es {
                "Empieza a reportar en {host} (despliegue o recolocación)"
            } else {
                "Starts reporting on {host} (deployment or reschedule)"
            }
        }
        "stopped" => {
            if es {
                "parada"
            } else {
                "stopped"
            }
        }
        "stopped_detail" => {
            if es {
                "Última muestra en {host}"
            } else {
                "Last sample on {host}"
            }
        }
        "restart" => {
            if es {
                "reinicio de alloc"
            } else {
                "alloc restart"
            }
        }
        "restart_detail" => {
            if es {
                "Nomad reinició la tarea"
            } else {
                "Nomad restarted the task"
            }
        }
        "restarts_in_bucket" => {
            if es {
                "{n} reinicios en el tramo"
            } else {
                "{n} restarts in the bucket"
            }
        }
        "error" => "error",
        "errors" => {
            if es {
                "errores"
            } else {
                "errors"
            }
        }
        "burst" => {
            if es {
                "ráfaga de logs"
            } else {
                "log burst"
            }
        }
        "burst_detail" => {
            if es {
                "{n} líneas en {m} min (lo normal son {median})"
            } else {
                "{n} lines in {m} min (normally {median})"
            }
        }
        "new_allocs" => {
            if es {
                "allocs nuevas"
            } else {
                "new allocs"
            }
        }
        "stopped_allocs" => {
            if es {
                "allocs paradas"
            } else {
                "allocs stopped"
            }
        }
        "restarts" => {
            if es {
                "reinicios"
            } else {
                "restarts"
            }
        }
        "restarts_group" => {
            if es {
                "Nomad reinició las tareas"
            } else {
                "Nomad restarted the tasks"
            }
        }
        "deploy_group" => {
            if es {
                "Despliegue o recolocación"
            } else {
                "Deployment or reschedule"
            }
        }
        _ => "",
    }
}

pub async fn events(app: &App, start: i64, end: i64, service: Option<&str>, lang: &str, with_lanes: bool) -> Result<Value> {
    let cfg = load_config(&app.timeline);
    let range = end - start;
    let bucket = (range / 120).clamp(60, 3600);
    let (table, col, res) = tier(start, end, chrono::Utc::now().timestamp());
    let db = &app.db;
    let mut params: Vec<Param> = vec![
        ("start".into(), start.to_string()),
        ("end".into(), end.to_string()),
        ("bucket".into(), bucket.to_string()),
        ("ignore".into(), atalaya::ch::array_param(&cfg.ignore_tasks)),
    ];
    let svc_cond = match service {
        Some(s) => {
            params.push(("svc".into(), s.to_owned()));
            " AND Service = {svc:String}"
        }
        None => "",
    };
    let alloc_cond = match service {
        Some(_) => " AND s.Labels['job'] = {svc:String}",
        None => "",
    };

    // ── Allocs: first and last sample in the range ─────────────────────────
    let sql_allocs = format!(
        "SELECT s.Labels['job'] AS job, s.Labels['alloc_id'] AS alloc, s.Resource['host.name'] AS host,
                toUnixTimestamp(min(p.Time)) AS first, toUnixTimestamp(max(p.Time)) AS last
         FROM {db}.{table} AS p
         INNER JOIN (SELECT SeriesId, Labels, Resource FROM {db}.series FINAL
                     WHERE Metric = 'nomad_client_allocs_cpu_total_percent' AND NOT has({{ignore:Array(String)}}, Labels['task'])) AS s USING SeriesId
         WHERE p.Metric = 'nomad_client_allocs_cpu_total_percent'
           AND p.Time >= toDateTime({{start:Int64}}) - {res} AND p.Time <= toDateTime({{end:Int64}}){alloc_cond}
         GROUP BY job, alloc, host"
    );
    // ── Restarts: counter increments per alloc ─────────────────────────────
    let sql_restarts = format!(
        "SELECT job, alloc, host, toUnixTimestamp(Time) AS t, v - prev AS n FROM (
           SELECT s.Labels['job'] AS job, s.Labels['alloc_id'] AS alloc, s.Resource['host.name'] AS host, p.Time AS Time, p.v AS v,
                  lagInFrame(p.v) OVER (PARTITION BY p.SeriesId ORDER BY p.Time) AS prev
           -- One row per series and bucket: aggregated tiers may hold unmerged
           -- parts with pieces of the same bucket
           FROM (SELECT SeriesId, Time, max({col}) AS v FROM {db}.{table}
                 WHERE Metric = 'nomad_client_allocs_restart'
                   AND Time >= toDateTime({{start:Int64}}) - {res} - 600 AND Time <= toDateTime({{end:Int64}})
                 GROUP BY SeriesId, Time) AS p
           INNER JOIN (SELECT SeriesId, Labels, Resource FROM {db}.series FINAL
                       WHERE Metric = 'nomad_client_allocs_restart' AND NOT has({{ignore:Array(String)}}, Labels['task'])) AS s USING SeriesId
           WHERE 1{alloc_cond})
         WHERE prev IS NOT NULL AND v > prev AND Time >= toDateTime({{start:Int64}})"
    );
    // ── Logs per service and bucket: total and errors ──────────────────────
    let sql_logs = format!(
        "SELECT Service AS service, toUnixTimestamp(toStartOfInterval(Time, INTERVAL {{bucket:Int64}} SECOND)) AS t,
                count() AS n, countIf(Level IN ('ERROR', 'FATAL')) AS errors,
                anyIf(substring(Body, 1, 200), Level IN ('ERROR', 'FATAL')) AS sample, any(Host) AS host
         FROM {db}.logs
         WHERE Time >= fromUnixTimestamp64Milli({{start:Int64}} * 1000) AND Time < fromUnixTimestamp64Milli({{end:Int64}} * 1000)
           AND NOT has({{ignore:Array(String)}}, Resource['nomad.task']){svc_cond}
         GROUP BY service, t ORDER BY service, t"
    );
    let (allocs, restarts, logs) = tokio::join!(
        app.ch.rows(&sql_allocs, &params),
        app.ch.rows(&sql_restarts, &params),
        app.ch.rows(&sql_logs, &params)
    );
    let (allocs, restarts, logs) = (allocs.context("allocs")?, restarts.context("restarts")?, logs.context("logs")?);

    let mut events: Vec<Value> = Vec::new();
    // Deployments and stops. A first sample right at the start of the range is
    // not a start: the alloc was already running.
    for r in &allocs {
        let (job, alloc, host) = (
            r["job"].as_str().unwrap_or(""),
            r["alloc"].as_str().unwrap_or(""),
            r["host"].as_str().unwrap_or(""),
        );
        let first = r["first"].as_i64().unwrap_or(0);
        let last = r["last"].as_i64().unwrap_or(0);
        if first > start + 2 * res {
            events.push(
                json!({ "time": first, "type": "alloc_started", "service": job, "host": host, "alloc": short(alloc),
                "title": format!("{job} · {} {}", tr(lang, "new_alloc"), short(alloc)), "detail": tr(lang, "new_alloc_detail").replace("{host}", host) }),
            );
        }
        if last < end - 3 * res.max(60) {
            events.push(
                json!({ "time": last, "type": "alloc_stopped", "service": job, "host": host, "alloc": short(alloc),
                "title": format!("{job} · alloc {} {}", short(alloc), tr(lang, "stopped")), "detail": tr(lang, "stopped_detail").replace("{host}", host) }),
            );
        }
    }
    for r in &restarts {
        let (job, alloc, host) = (
            r["job"].as_str().unwrap_or(""),
            r["alloc"].as_str().unwrap_or(""),
            r["host"].as_str().unwrap_or(""),
        );
        let n = r["n"].as_f64().unwrap_or(1.0).round() as i64;
        events.push(json!({ "time": r["t"].as_i64(), "type": "restart", "service": job, "host": host, "alloc": short(alloc),
            "title": format!("{job} · {} {}", tr(lang, "restart"), short(alloc)), "detail": if n > 1 { tr(lang, "restarts_in_bucket").replace("{n}", &n.to_string()) } else { tr(lang, "restart_detail").to_owned() } }));
    }

    // Log lanes, plus error and burst events
    let mut lanes: BTreeMap<String, Vec<(i64, i64, i64)>> = BTreeMap::new();
    let mut hosts: HashMap<String, String> = HashMap::new();
    for r in &logs {
        let svc = r["service"].as_str().unwrap_or("").to_owned();
        hosts.entry(svc.clone()).or_insert_with(|| r["host"].as_str().unwrap_or("").to_owned());
        lanes
            .entry(svc)
            .or_default()
            .push((r["t"].as_i64().unwrap_or(0), r["n"].as_i64().unwrap_or(0), r["errors"].as_i64().unwrap_or(0)));
    }
    let samples: HashMap<(String, i64), String> = logs
        .iter()
        .filter(|r| r["errors"].as_i64().unwrap_or(0) > 0)
        .map(|r| {
            (
                (r["service"].as_str().unwrap_or("").to_owned(), r["t"].as_i64().unwrap_or(0)),
                r["sample"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    for (svc, pts) in &lanes {
        let host = hosts.get(svc).cloned().unwrap_or_default();
        // Errors: consecutive buckets with errors form one event
        let mut run: Option<(i64, i64, i64)> = None; // (start, last bucket, total)
        let flush = |run: &mut Option<(i64, i64, i64)>, events: &mut Vec<Value>| {
            if let Some((t0, _, total)) = run.take() {
                let sample = samples.get(&(svc.clone(), t0)).cloned().unwrap_or_default();
                events.push(json!({ "time": t0, "type": "errors", "service": svc, "host": host, "count": total,
                    "title": format!("{svc} · {total} {}", tr(lang, if total == 1 { "error" } else { "errors" })), "detail": sample }));
            }
        };
        for &(t, _, e) in pts {
            match (&mut run, e > 0) {
                (Some(r), true) if t - r.1 <= bucket => {
                    r.1 = t;
                    r.2 += e;
                }
                (Some(_), true) => {
                    flush(&mut run, &mut events);
                    run = Some((t, t, e));
                }
                (Some(_), false) => flush(&mut run, &mut events),
                (None, true) => run = Some((t, t, e)),
                (None, false) => {}
            }
        }
        flush(&mut run, &mut events);
        // Bursts: a bucket above 4x the service median (and at least 50 lines)
        let mut counts: Vec<i64> = pts.iter().map(|p| p.1).collect();
        counts.sort_unstable();
        let median = counts[counts.len() / 2];
        for &(t, n, _) in pts {
            if n >= 50 && n > 4 * median.max(1) {
                events.push(json!({ "time": t, "type": "log_burst", "service": svc, "host": host, "count": n,
                    "title": format!("{svc} · {}", tr(lang, "burst")), "detail": tr(lang, "burst_detail").replace("{n}", &n.to_string()).replace("{m}", &(bucket / 60).to_string()).replace("{median}", &median.to_string()) }));
            }
        }
    }

    // Real alerts in the range (from alerts.yaml), with their severity
    events.extend(app.alerts.events(start, end, lang).await);

    // Deploying a system job yields one new alloc, one stop and one restart per
    // node almost at once: events of the same service and type within three
    // minutes are merged into one event with a count
    events.sort_by_key(|e| e["time"].as_i64().unwrap_or(0));
    let mut grouped: Vec<Value> = Vec::new();
    for e in events {
        // Events from other services may sit between two allocs of the same
        // deployment: look back further than just the last one
        let t = e["time"].as_i64().unwrap_or(0);
        let same = grouped
            .iter_mut()
            .rev()
            .take(24)
            .find(|g| g["service"] == e["service"] && g["type"] == e["type"] && t - g["end"].as_i64().or(g["time"].as_i64()).unwrap_or(0) <= 180);
        match same {
            Some(g) if matches!(e["type"].as_str(), Some("alloc_started") | Some("alloc_stopped") | Some("restart")) => {
                let n = g["count"].as_i64().unwrap_or(1) + 1;
                g["count"] = json!(n);
                g["end"] = e["time"].clone();
                let kind = e["type"].as_str().unwrap_or("");
                let what = tr(
                    lang,
                    match kind {
                        "alloc_started" => "new_allocs",
                        "alloc_stopped" => "stopped_allocs",
                        _ => "restarts",
                    },
                );
                g["title"] = json!(format!("{} · {n} {what}", e["service"].as_str().unwrap_or("")));
                let mut allocs: Vec<String> = g["allocs"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                if let Some(a) = e["alloc"].as_str() {
                    allocs.push(a.to_owned());
                }
                g["detail"] = json!(format!(
                    "{} ({})",
                    tr(lang, if kind == "restart" { "restarts_group" } else { "deploy_group" }),
                    allocs.join(", ")
                ));
                g["allocs"] = json!(allocs);
            }
            _ => {
                let mut e = e;
                if let Some(a) = e["alloc"].as_str().map(str::to_owned) {
                    e["allocs"] = json!([a]);
                }
                grouped.push(e);
            }
        }
    }
    let mut events = grouped;
    events.sort_by_key(|e| std::cmp::Reverse(e["time"].as_i64().unwrap_or(0)));
    events.truncate(MAX_EVENTS);

    // Log lanes to return: the configured ones or those with most lines
    let lane_names: Vec<String> = if cfg.log_lanes.is_empty() {
        let mut v: Vec<(&String, i64)> = lanes.iter().map(|(s, p)| (s, p.iter().map(|x| x.1).sum())).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v.into_iter().take(6).map(|x| x.0.clone()).collect()
    } else {
        cfg.log_lanes.clone()
    };
    let log_lanes: Vec<Value> = lane_names
        .iter()
        .filter_map(|s| {
            lanes
                .get(s)
                .map(|pts| json!({ "service": s, "host": hosts.get(s), "points": pts.iter().map(|(t, n, e)| json!([t, n, e])).collect::<Vec<_>>() }))
        })
        .collect();

    // Metric lanes
    let lanes_cfg: &[Lane] = if with_lanes { &cfg.lanes } else { &[] };
    let metric_lanes = futures::future::join_all(lanes_cfg.iter().map(|l| async move {
        let series = match app.promql.range(&l.query, start, end, bucket).await {
            Ok(r) => match r.output {
                Output::Matrix(m) => m.into_iter().next().map(|(_, p)| p).unwrap_or_default(),
                Output::Scalar(s) => s,
                Output::Str(_) => vec![],
            },
            Err(e) => {
                tracing::debug!("lane {}: {e:#}", l.title.get(lang));
                vec![]
            }
        };
        json!({ "title": l.title.get(lang), "unit": l.unit, "query": l.query, "points": series.iter().map(|(t, v)| json!([t, (v * 1000.0).round() / 1000.0])).collect::<Vec<_>>() })
    }))
    .await;

    Ok(json!({ "start": start, "end": end, "bucket": bucket, "events": events, "log_lanes": log_lanes, "metric_lanes": metric_lanes }))
}

#[cfg(test)]
mod tests {
    use super::tier;

    #[test]
    fn old_short_ranges_use_a_tier_that_still_has_the_data() {
        let now = 100 * 86400;
        // An hour ending now: raw
        assert_eq!(tier(now - 3600, now, now).0, "points");
        // An hour sixty days ago: raw (7 d) and 1 min (42 d) no longer have it
        assert_eq!(tier(now - 60 * 86400, now - 60 * 86400 + 3600, now).0, "points_10m");
        // An hour ten days ago: 1 min
        assert_eq!(tier(now - 10 * 86400, now - 10 * 86400 + 3600, now).0, "points_1m");
        // Long recent ranges are chosen by span
        assert_eq!(tier(now - 7 * 86400, now, now).0, "points_1m");
        assert_eq!(tier(now - 90 * 86400, now, now).0, "points_10m");
    }
}
