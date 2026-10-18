// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! atalaya-collector: one per node. Scrapes the Prometheus endpoints of what
//! runs on its node (discovered through Nomad) plus the static ones from the
//! config, applies the metric rules, tails the allocation logs and writes it
//! all to ClickHouse in atalaya's format: each series is registered once and
//! after that only points are sent.
//!
//! Usage: atalaya-collector <config.yaml>   (password in CLICKHOUSE_PASSWORD)

mod collector;
mod config;
mod docker_logs;
mod host;
mod logs;
mod nomad;
mod prom;
mod rules;
mod sink;
mod smart;
mod syslog;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::collector::{Collector, Registry};
use crate::config::{Config, parse_duration};
use crate::nomad::Nomad;
use crate::rules::Rules;
use crate::sink::Sink;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,atalaya_collector=info".into()))
        // No colours outside a terminal: under Nomad the output ends up in atalaya's logs
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let path = std::env::args().nth(1).context("usage: atalaya-collector <config.yaml>")?;
    let cfg: Config = serde_saphyr::from_str(&std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?)?;
    cfg.validate()?;
    std::fs::create_dir_all(&cfg.state_dir)?;
    let password = std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_default();
    let interval = parse_duration(&cfg.scrape.interval)?;
    // A Nomad template that resolves the URL with nomadService can render it
    // empty ("http:///") while the allocation starts: better to exit and let
    // Nomad restart us when the template re-renders than to spool batches that
    // can never be sent
    let url = reqwest::Url::parse(&cfg.clickhouse.url).with_context(|| format!("invalid clickhouse.url: {}", cfg.clickhouse.url))?;
    if url.host_str().is_none_or(str::is_empty) {
        anyhow::bail!(
            "clickhouse.url has no host ({}): is the clickhouse-http service not resolved yet?",
            cfg.clickhouse.url
        );
    }

    let sink = Arc::new(Sink::new(
        cfg.clickhouse.url.clone(),
        cfg.clickhouse.user.clone(),
        password,
        cfg.clickhouse.database.clone(),
        cfg.state_dir.join("spool"),
        cfg.clickhouse.max_spool_mb << 20,
    )?);
    let ca = cfg.nomad.as_ref().and_then(|n| n.ca_file.as_deref()).map(nomad::ca_cert).transpose()?;
    let token = match cfg.nomad.as_ref().and_then(|n| n.token_file.as_deref()) {
        Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?),
        None => None,
    };
    let collector = Arc::new(Collector {
        nomad: cfg
            .nomad
            .as_ref()
            .map(|n| Nomad::new(n.addr.clone(), n.node_id.clone(), ca.clone(), token.as_deref()))
            .transpose()?,
        http: collector::scrape_client(ca.as_ref())?,
        rules: Mutex::new(Rules::new(&cfg.rules)?),
        host: if cfg.host.enabled {
            let h = host::Host::new(&cfg.host.root);
            if h.is_none() {
                tracing::warn!("no host sysfs at {}: host metrics disabled", cfg.host.root.display());
            }
            h.map(|h| Arc::new(std::sync::Mutex::new(h)))
        } else {
            None
        },
        registry: Mutex::new(Registry::new()),
        stats: Mutex::new(BTreeMap::new()),
        sink: sink.clone(),
        cfg,
    });
    tracing::info!("atalaya-collector on {}: scraping every {:?}", collector.cfg.node, interval);

    // Producers: scraping, SMART, logs and self metrics. On shutdown they are
    // cancelled first, then the in-flight send is awaited and the rest is flushed.
    let mut producers = Vec::new();
    // A stop signal that is not lost if the task is busy when it arrives
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    {
        let c = collector.clone();
        producers.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                c.scrape_all().await;
            }
        }));
    }
    if collector.host.is_some() && collector.cfg.host.smart.enabled {
        let c = collector.clone();
        let every = parse_duration(&c.cfg.host.smart.interval)?;
        producers.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                c.smart_all().await;
            }
        }));
    }
    // Logs (Nomad allocations and/or Docker containers): offsets are only saved
    // once the lines read up to them are sent or in the disk spool (otherwise a
    // crash would skip them)
    let logs_task = collector.cfg.logs.enabled.then(|| {
        let c = collector.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            let node = c.cfg.node.clone();
            let mut alloc = c
                .nomad
                .is_some()
                .then(|| logs::Tailer::new(c.cfg.logs.alloc_dir.clone(), &c.cfg.state_dir, node.clone()));
            let mut docker = c
                .cfg
                .logs
                .docker_dir
                .clone()
                .map(|d| docker_logs::DockerTailer::new(d, &c.cfg.state_dir, node.clone()));
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            type Offsets = (Option<logs::State>, Option<docker_logs::State>);
            let mut waiting: std::collections::VecDeque<(u64, Offsets)> = Default::default();
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stop.changed() => return (alloc, docker),
                }
                let mut new_lines = 0;
                let mut cut = 0;
                if let (Some(t), Some(nomad)) = (alloc.as_mut(), c.nomad.as_ref()) {
                    let (l, tr) = (t.lines, t.truncated);
                    t.poll(nomad, &c.sink).await;
                    new_lines += t.lines - l;
                    cut += t.truncated - tr;
                }
                if let Some(t) = docker.as_mut() {
                    let l = t.lines;
                    t.poll(&c.sink).await;
                    new_lines += t.lines - l;
                }
                {
                    let mut st = c.stats.lock().await;
                    *st.entry("log_lines".into()).or_default() += new_lines as f64;
                    *st.entry("log_lines_truncated".into()).or_default() += cut as f64;
                }
                waiting.push_back((c.sink.mark(), (alloc.as_ref().map(|t| t.offsets()), docker.as_ref().map(|t| t.offsets()))));
                let mut durable = None;
                while waiting.front().is_some_and(|(m, _)| c.sink.is_durable(*m)) {
                    durable = waiting.pop_front().map(|(_, s)| s);
                }
                if let Some((a, d)) = durable {
                    if let (Some(t), Some(s)) = (alloc.as_ref(), a) {
                        t.save_offsets(&s);
                    }
                    if let (Some(t), Some(s)) = (docker.as_ref(), d) {
                        t.save_offsets(&s);
                    }
                }
            }
        })
    });
    {
        let c = collector.clone();
        producers.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                c.report().await;
            }
        }));
    }
    // Syslog receiver (optional): counted in atalaya_collector_syslog_lines_total
    let syslog_lines = Arc::new(std::sync::atomic::AtomicU64::new(0));
    if let Some(cfg) = collector.cfg.syslog.clone() {
        syslog::run(cfg, sink.clone(), syslog_lines.clone()).await?;
        let (c, n) = (collector.clone(), syslog_lines.clone());
        producers.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                let v = n.load(std::sync::atomic::Ordering::Relaxed) as f64;
                c.stats.lock().await.insert("syslog_lines".into(), v);
            }
        }));
    }
    // Sending: an in-flight send is never cancelled halfway
    let flush_every = parse_duration(&collector.cfg.clickhouse.flush_interval)?;
    let flusher = {
        let s = sink.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(flush_every);
            loop {
                tokio::select! {
                    _ = tick.tick() => s.flush().await,
                    _ = stop.changed() => break,
                }
            }
        })
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down: stop reading, finish the in-flight send and flush the rest");
    for p in &producers {
        p.abort();
    }
    let _ = stop_tx.send(true);
    let tailer = match logs_task {
        Some(t) => t.await.ok(),
        None => None,
    };
    let _ = flusher.await;
    sink.flush().await;
    // Everything read is sent or on disk: offsets can be saved now
    if let Some((alloc, docker)) = tailer {
        alloc.iter().for_each(|t| t.save());
        docker.iter().for_each(|t| t.save());
    }
    Ok(())
}
