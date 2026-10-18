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
    /// Samples read from ClickHouse
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
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Expands the Grafana macros that depend on the step and the tier
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

impl Engine {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Range query: steps start, start+step, ..., end
    pub async fn range(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        if step <= 0 {
            bail!("step must be positive");
        }
        if end < start {
            bail!("end is before start");
        }
        if (end - start) / step + 1 > MAX_POINTS {
            bail!("too many points per series ({}): increase the step", (end - start) / step + 1);
        }
        let now = now();
        let range = end - start;

        // The tier depends on the expression's longest range, and the macros
        // depend on the tier: pick it from a first parse and repeat if it changes.
        let mut tier = Tier::pick(start - LOOKBACK, step, now);
        let mut expr;
        loop {
            let text = expand_macros(query, step, tier.res(), range);
            expr = parser::parse(&text).map_err(|e| anyhow::anyhow!("{e}"))?;
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

        let fetches = needs.into_iter().map(|(k, n)| async move {
            let t = std::time::Instant::now();
            let series = self.store.select(&n.vs.name, &n.vs.matchers, n.from).await?;
            let series_ms = t.elapsed().as_millis();
            let t = std::time::Instant::now();
            let samples = self.store.samples(series, n.from, n.to, tier, group).await?;
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

        let t = std::time::Instant::now();
        let ev = eval::Evaluator {
            start,
            end,
            step,
            n: ((end - start) / step + 1) as usize,
            lookback,
            min_range,
            data: &data,
        };
        let output = ev.run(&expr)?;
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
