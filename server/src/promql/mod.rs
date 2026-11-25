// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! PromQL over the atalaya store, like Cloud Monitoring's PromQL support: the
//! same API as Prometheus (query_range, query, label values and series), so
//! Grafana dashboards migrate with their queries.
//!
//! Evaluation has two phases: first the expression's selectors are collected
//! and fetched from ClickHouse in parallel (from the tier that fits the step
//! and age), then the expression is evaluated in memory, step by step.
//!
//! Besides PromQL, queries accept the Grafana macros `$__rate_interval`,
//! `$__interval` and `$__range`, which are expanded here because they depend
//! on the chosen tier.

mod eval;
pub mod store;
pub mod victoria;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use promql_parser::parser::{self, AtModifier, Expr, Offset, VectorSelector};

use store::{Samples, Store, Tier};

pub type Labels = BTreeMap<String, String>;

/// Prometheus does not return more points per series
const MAX_POINTS: i64 = 11_000;
/// Default window to look back for the latest sample of a series
const LOOKBACK: i64 = 300;

/// Value as text, as in the Prometheus API
pub fn fmt_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        format!("{v}")
    }
}

pub enum Output {
    /// Series with their points (Unix seconds, value)
    Matrix(Vec<(Labels, Vec<(i64, f64)>)>),
    Scalar(Vec<(i64, f64)>),
    Str(String),
}

pub struct Evaluated {
    pub output: Output,
    pub tier: Tier,
    /// Where the time goes (ms): series = finding the series in ClickHouse,
    /// samples = reading their samples, eval = evaluating in memory. The first
    /// two run in parallel per selector, so they add up to more than wall time.
    pub timing: Timing,
    /// Samples read from ClickHouse (in the aggregated tiers a counter bucket
    /// gives two: its start and its end)
    pub samples_read: usize,
}

#[derive(Default, Clone, Copy, serde::Serialize)]
pub struct Timing {
    pub series_ms: u128,
    pub samples_ms: u128,
    pub eval_ms: u128,
}

pub struct Engine {
    pub store: Store,
    /// Most samples one query may read (ATALAYA_MAX_SAMPLES), like Prometheus' query.max-samples
    max_samples: u64,
    /// Samples being read or evaluated right now, across queries: a query reserves its estimate
    /// before reading, and waits while others hold the budget (max_samples in all, so the memory
    /// of every query at once stays about that of the largest one allowed)
    in_flight: std::sync::Arc<tokio::sync::Semaphore>,
    /// The same budget for alert evaluation, apart: queries from the UI and the API cannot hold
    /// alerts back by keeping theirs busy
    alerts_in_flight: std::sync::Arc<tokio::sync::Semaphore>,
}

/// Default for ATALAYA_MAX_SAMPLES. Measured on a release build, a query takes 30 to 100 bytes per
/// sample read (the ClickHouse answer, the series, a join, the JSON): 5 million is up to ~500 MiB
const MAX_SAMPLES: u64 = 5_000_000;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Expands the Grafana macros that depend on the step and the tier
/// Checks that a query parses and that the engine supports it, without running it (for alert rules,
/// when their file is loaded). The parser panics on some input (`rate(x[1e30])`): that is an error too
pub fn check(query: &str) -> Result<()> {
    let expr = parse(&expand_macros(query, 60, 60, 3600))?;
    collect(&expr, &mut Vec::new())
}

/// The parser panics on some input (`rate(x[1e30])` overflows a Duration): that is a bad query, not
/// a reason to take down the request or the alert loop
fn parse(text: &str) -> Result<Expr> {
    std::panic::catch_unwind(|| parser::parse(text))
        .map_err(|_| anyhow::anyhow!("invalid expression (the PromQL parser panicked on it)"))?
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn expand_macros(q: &str, step: i64, res: i64, range: i64) -> String {
    let rate = (4 * res).max(step + res);
    q.replace("${__rate_interval}", &format!("{rate}s"))
        .replace("$__rate_interval", &format!("{rate}s"))
        .replace("${__interval_ms}", &(step * 1000).to_string())
        .replace("$__interval_ms", &(step * 1000).to_string())
        .replace("${__interval}", &format!("{step}s"))
        .replace("$__interval", &format!("{step}s"))
        .replace("${__range_s}", &range.to_string())
        .replace("$__range_s", &range.to_string())
        .replace("${__range_ms}", &(range * 1000).to_string())
        .replace("$__range_ms", &(range * 1000).to_string())
        .replace("${__range}", &format!("{range}s"))
        .replace("$__range", &format!("{range}s"))
}

fn offset_secs(o: &Option<Offset>) -> i64 {
    match o {
        Some(Offset::Pos(d)) => d.as_secs() as i64,
        Some(Offset::Neg(d)) => -(d.as_secs() as i64),
        None => 0,
    }
}

fn at_secs(at: &Option<AtModifier>, start: i64, end: i64) -> Option<i64> {
    match at {
        Some(AtModifier::Start) => Some(start),
        Some(AtModifier::End) => Some(end),
        Some(AtModifier::At(t)) => Some(
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_else(|e| -(e.duration().as_secs() as i64)),
        ),
        None => None,
    }
}

/// Key of a selector in the data map
pub(crate) fn sel_key(vs: &VectorSelector) -> String {
    vs.to_string()
}

/// What to fetch for each selector: from when to when
struct Need<'a> {
    vs: &'a VectorSelector,
    from: i64,
    to: i64,
}

/// Walks the expression collecting selectors and their range windows
fn collect<'a>(e: &'a Expr, out: &mut Vec<(&'a VectorSelector, i64)>) -> Result<()> {
    match e {
        Expr::VectorSelector(vs) => out.push((vs, 0)),
        Expr::MatrixSelector(ms) => out.push((&ms.vs, ms.range.as_secs() as i64)),
        Expr::Aggregate(a) => {
            collect(&a.expr, out)?;
            if let Some(p) = &a.param {
                collect(p, out)?;
            }
        }
        Expr::Unary(u) => collect(&u.expr, out)?,
        Expr::Binary(b) => {
            collect(&b.lhs, out)?;
            collect(&b.rhs, out)?;
        }
        Expr::Paren(p) => collect(&p.expr, out)?,
        Expr::Call(c) => {
            for a in &c.args.args {
                collect(a, out)?;
            }
        }
        Expr::Subquery(_) => bail!("subqueries ([range:step]) are not supported"),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) => {}
        Expr::Extension(_) => bail!("unsupported expression"),
    }
    Ok(())
}

/// Where PromQL runs: atalaya's engine over ClickHouse, or VictoriaMetrics. Both answer with the same
/// series and labels, so the API, the home page and the alert rules do not tell them apart.
pub enum Backend {
    /// Boxed: the engine's semaphores and store are far larger than a VictoriaMetrics client
    Native(Box<Engine>),
    Victoria(victoria::VictoriaMetrics),
}

impl Backend {
    pub async fn range(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        match self {
            Self::Native(e) => e.range(query, start, end, step).await,
            Self::Victoria(v) => v.range(query, start, end, step).await,
        }
    }

    pub async fn instant(&self, query: &str, time: i64) -> Result<Evaluated> {
        match self {
            Self::Native(e) => e.instant(query, time).await,
            Self::Victoria(v) => v.instant(query, time).await,
        }
    }

    /// For alerts: the engine keeps a separate budget; VictoriaMetrics has its own limits
    pub async fn range_for_alerts(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        match self {
            Self::Native(e) => e.range_for_alerts(query, start, end, step).await,
            Self::Victoria(v) => v.range(query, start, end, step).await,
        }
    }

    pub async fn instant_for_alerts(&self, query: &str, time: i64) -> Result<Evaluated> {
        match self {
            Self::Native(e) => e.instant_for_alerts(query, time).await,
            Self::Victoria(v) => v.instant(query, time).await,
        }
    }

    /// Values of a label in the series matching the `match[]` selectors (as written, and parsed for
    /// the engine) since `from`
    pub async fn label_values(&self, label: &str, raw: &[&str], parsed: &[(Option<String>, promql_parser::label::Matchers)], from: i64) -> Result<Vec<String>> {
        match self {
            Self::Native(e) => e.store.label_values(label, parsed, from).await,
            Self::Victoria(v) => v.label_values(label, raw, from).await,
        }
    }

    pub async fn series(&self, raw: &[&str], parsed: &[(Option<String>, promql_parser::label::Matchers)], from: i64) -> Result<Vec<Labels>> {
        match self {
            Self::Native(e) => e.store.series(parsed, from).await,
            Self::Victoria(v) => v.series(raw, from).await,
        }
    }
}

impl Engine {
    pub fn new(store: Store) -> Self {
        let max_samples = std::env::var("ATALAYA_MAX_SAMPLES")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(MAX_SAMPLES);
        Self::with_max_samples(store, max_samples)
    }

    pub fn with_max_samples(store: Store, max_samples: u64) -> Self {
        let max_samples = max_samples.min(u64::from(u32::MAX));
        Self {
            store,
            max_samples,
            in_flight: std::sync::Arc::new(tokio::sync::Semaphore::new(max_samples as usize)),
            alerts_in_flight: std::sync::Arc::new(tokio::sync::Semaphore::new(max_samples as usize)),
        }
    }

    /// Range query: steps start, start+step, ..., end
    pub async fn range(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        self.run(query, start, end, step, &self.in_flight).await
    }

    /// A range query for alerts (rules, sparklines), with a budget of their own
    pub async fn range_for_alerts(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        self.run(query, start, end, step, &self.alerts_in_flight).await
    }

    /// An instant query for alert rules, with a budget of their own
    pub async fn instant_for_alerts(&self, query: &str, time: i64) -> Result<Evaluated> {
        self.range_for_alerts(query, time, time, 15).await
    }

    async fn run(&self, query: &str, start: i64, end: i64, step: i64, budget: &std::sync::Arc<tokio::sync::Semaphore>) -> Result<Evaluated> {
        if step <= 0 {
            bail!("step must be positive");
        }
        if end < start {
            bail!("end is before start");
        }
        let now = now();
        // Within the epoch and a day ahead: start and end come from the request,
        // and neither `end - start` nor the arithmetic on them below may overflow
        if start < 0 || end > now + 86400 {
            bail!("invalid range");
        }
        if (end - start) / step + 1 > MAX_POINTS {
            bail!("too many points per series ({}): increase the step", (end - start) / step + 1);
        }
        let range = end - start;

        // The tier depends on the expression's longest range, and the macros
        // depend on the tier: pick it from a first parse and repeat if it changes.
        let mut tier = Tier::pick(start - LOOKBACK, step, now);
        let mut expr;
        loop {
            let text = expand_macros(query, step, tier.res(), range);
            expr = parse(&text)?;
            let mut sels = Vec::new();
            collect(&expr, &mut sels)?;
            let oldest = sels
                .iter()
                .map(|(vs, r)| at_secs(&vs.at, start, end).unwrap_or(start) - offset_secs(&vs.offset) - (*r).max(LOOKBACK))
                .min()
                .unwrap_or(start);
            let t = Tier::pick(oldest, step, now);
            if t == tier {
                break;
            }
            tier = t;
        }

        // With long steps ClickHouse groups into half-step buckets (a multiple
        // of the tier resolution) and returns the last sample of each. With
        // sparse samples short windows would come out empty, so they are widened
        // to two buckets (as Cloud Monitoring does with alignment).
        let res = tier.res();
        let group = res * (step / (2 * res)).max(1);
        let spacing = if tier == Tier::Raw && group == res { 0 } else { group };
        let lookback = LOOKBACK.max(2 * spacing);
        let min_range = 2 * spacing;

        let mut sels = Vec::new();
        collect(&expr, &mut sels)?;
        let mut needs: HashMap<String, Need> = HashMap::new();
        for (vs, r) in sels {
            let off = offset_secs(&vs.offset);
            let (a, b) = match at_secs(&vs.at, start, end) {
                Some(t) => (t, t),
                None => (start, end),
            };
            let window = if r == 0 { lookback } else { r.max(min_range) };
            let from = a - off - window;
            let to = b - off;
            let n = needs.entry(sel_key(vs)).or_insert(Need { vs, from, to });
            n.from = n.from.min(from);
            n.to = n.to.max(to);
        }

        // First the series of each selector, to know how much the query would read before reading
        // it: a few selectors over thousands of series and days of data took the server past
        // 18 GiB and the kernel killed it (and in a container, past its memory limit)
        let selects = needs.into_iter().map(|(k, n)| async move {
            let t = std::time::Instant::now();
            let series = self.store.select(&n.vs.name, &n.vs.matchers, n.from).await?;
            Ok::<_, anyhow::Error>((k, n.from, n.to, series, t.elapsed().as_millis()))
        });
        let selected = futures::future::try_join_all(selects).await?;
        let estimate: u64 = selected
            .iter()
            .map(|(_, from, to, series, _)| store::estimate_samples(series, *from, *to, tier, group))
            .sum();
        if estimate > self.max_samples {
            bail!(
                "the query would read about {estimate} samples, more than the {} allowed (ATALAYA_MAX_SAMPLES): narrow the selectors, shorten the range or increase the step",
                self.max_samples
            );
        }
        // Waits while other queries hold the budget (released when this one is done, result included)
        // (never more than the whole budget, or it would wait forever)
        let permits = estimate.clamp(1, self.max_samples) as u32;
        let _permit = budget.clone().acquire_many_owned(permits).await?;

        let fetches = selected.into_iter().map(|(k, from, to, series, series_ms)| async move {
            let t = std::time::Instant::now();
            let samples = self.store.samples(series, from, to, tier, group).await?;
            Ok::<_, anyhow::Error>((k, samples, series_ms, t.elapsed().as_millis()))
        });
        let mut timing = Timing::default();
        let mut samples_read = 0;
        let mut data: HashMap<String, Vec<Samples>> = HashMap::new();
        for (k, samples, series_ms, samples_ms) in futures::future::try_join_all(fetches).await? {
            timing.series_ms += series_ms;
            timing.samples_ms += samples_ms;
            samples_read += samples.iter().map(|s| s.ts.len()).sum::<usize>();
            data.insert(k, samples);
        }
        if samples_read as u64 > self.max_samples {
            bail!(
                "the query read {samples_read} samples, more than the {} allowed (ATALAYA_MAX_SAMPLES)",
                self.max_samples
            );
        }

        // Evaluating is CPU work (seconds for a large query): on the async runtime it held a worker
        // thread for all that time, and a few of them at once stalled every request and the alerts
        let t = std::time::Instant::now();
        let n = ((end - start) / step + 1) as usize;
        let output = tokio::task::spawn_blocking(move || {
            let ev = eval::Evaluator {
                start,
                end,
                step,
                n,
                lookback,
                min_range,
                data: &data,
            };
            ev.run(&expr)
        })
        .await
        .map_err(|e| anyhow::anyhow!("evaluation failed: {e}"))??;
        timing.eval_ms = t.elapsed().as_millis();
        Ok(Evaluated {
            output,
            tier,
            timing,
            samples_read,
        })
    }

    /// Instant query at `time`
    pub async fn instant(&self, query: &str, time: i64) -> Result<Evaluated> {
        self.range(query, time, time, 15).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_range_outside_the_epoch_or_far_ahead_is_refused() {
        let ch = crate::ch::ClickHouse::new("http://127.0.0.1:1/".into(), "u".into(), String::new());
        let engine = Engine::new(Store::new(ch, "atalaya".into()));
        let now = now();
        // i64::MIN would wrap `end - start` past the point limit
        for (start, end) in [(i64::MIN, now), (-1, now), (now, now + 2 * 86400), (now, i64::MAX)] {
            let err = engine.range("up", start, end, 15).await.err().expect("refused");
            assert!(err.to_string().contains("invalid range"), "{start}..{end}: {err}");
        }
        let err = engine.instant("up", i64::MIN).await.err().expect("refused");
        assert!(err.to_string().contains("invalid range"), "{err}");
    }

    /// A ClickHouse with one gauge, `x`, with `n` series of one point each. Returns its URL and
    /// whether it was asked for points
    async fn fake_clickhouse(n: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use axum::{Router, body::Bytes, routing::post};
        let read_points = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = read_points.clone();
        let app = Router::new().fallback(post(move |body: Bytes| {
            let flag = flag.clone();
            async move {
                let sql = String::from_utf8_lossy(&body).into_owned();
                if sql.contains("metric_descriptors") {
                    return serde_json::json!({ "data": [{ "m": "x", "dist": 0, "kind": "gauge" }] })
                        .to_string()
                        .into_bytes();
                }
                if sql.contains(".series") {
                    let rows: Vec<_> = (0..n)
                        .map(|i| serde_json::json!({ "id": i + 1, "m": "x", "s": "svc", "r": {}, "l": { "i": i.to_string() }, "b": [] }))
                        .collect();
                    return serde_json::json!({ "data": rows }).to_string().into_bytes();
                }
                if sql.contains(".points") {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Vec::new()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/"), read_points)
    }

    /// A query that would read more samples than allowed is refused before reading them: a few such
    /// queries took the server past 18 GiB until the kernel killed it
    #[tokio::test]
    async fn a_query_over_the_sample_budget_is_refused_before_reading() {
        let (url, read_points) = fake_clickhouse(100).await;
        let engine = Engine::with_max_samples(Store::new(crate::ch::ClickHouse::new(url, "u".into(), String::new()), "atalaya".into()), 1000);
        let end = now() - 60;
        // 100 series × an hour and its lookback at 15 s: ~26,000 samples
        let err = engine.range("x", end - 3600, end, 15).await.err().expect("refused");
        assert!(err.to_string().contains("ATALAYA_MAX_SAMPLES"), "{err}");
        assert!(!read_points.load(std::sync::atomic::Ordering::SeqCst), "it did not read the points");
        // Within the budget it runs
        let (url, read_points) = fake_clickhouse(2).await;
        let engine = Engine::with_max_samples(Store::new(crate::ch::ClickHouse::new(url, "u".into(), String::new()), "atalaya".into()), 1000);
        engine.range("x", end - 3600, end, 15).await.expect("within the budget");
        assert!(read_points.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Queries from the UI holding the whole budget wait for it, but alert rules do not: they have
    /// a budget of their own
    #[tokio::test]
    async fn alerts_do_not_wait_for_the_budget_of_other_queries() {
        let (url, _) = fake_clickhouse(2).await;
        let engine = Engine::with_max_samples(Store::new(crate::ch::ClickHouse::new(url, "u".into(), String::new()), "atalaya".into()), 1000);
        let _busy = engine.in_flight.clone().acquire_many_owned(1000).await.unwrap();
        let t = now() - 60;
        let wait = std::time::Duration::from_millis(500);
        assert!(tokio::time::timeout(wait, engine.instant("x", t)).await.is_err(), "a UI query waits");
        let r = tokio::time::timeout(wait, engine.instant_for_alerts("x", t)).await;
        assert!(r.is_ok_and(|r| r.is_ok()), "an alert rule does not");
    }
}
