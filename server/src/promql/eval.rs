// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! In-memory evaluation. Each instant vector is held as series with one value
//! (or none) per step; range functions read the raw samples in their window.
//! Semantics follow Prometheus (rate extrapolation, vector matching,
//! histogram_quantile…).

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use promql_parser::{
    label::MatchOp,
    parser::{AggregateExpr, BinaryExpr, Call, Expr, LabelModifier, MatrixSelector, VectorMatchCardinality, VectorSelector},
};
use regex::Regex;

use super::{Labels, Output, at_secs, offset_secs, sel_key, store::Samples};

#[derive(Clone, Debug)]
pub struct Series {
    pub labels: Labels,
    pub vals: Vec<Option<f64>>,
}

pub enum Val {
    Vector(Vec<Series>),
    Scalar(Vec<f64>),
    Str(String),
}

pub struct Evaluator<'a> {
    pub start: i64,
    pub end: i64,
    pub step: i64,
    pub n: usize,
    pub lookback: i64,
    /// Minimum window for range functions (two buckets in aggregated tiers)
    pub min_range: i64,
    pub data: &'a HashMap<String, Vec<Samples>>,
}

fn drop_name(mut l: Labels) -> Labels {
    l.remove("__name__");
    l
}

fn is_arith(op: &str) -> bool {
    matches!(op, "+" | "-" | "*" | "/" | "%" | "^" | "atan2")
}

fn is_cmp(op: &str) -> bool {
    matches!(op, "==" | "!=" | ">" | "<" | ">=" | "<=")
}

/// Binary operation on two values. Comparisons also return whether they hold.
fn apply(op: &str, a: f64, b: f64) -> Result<(f64, bool)> {
    Ok(match op {
        "+" => (a + b, true),
        "-" => (a - b, true),
        "*" => (a * b, true),
        "/" => (a / b, true),
        "%" => (a % b, true),
        "^" => (a.powf(b), true),
        "atan2" => (a.atan2(b), true),
        "==" => (a, a == b),
        "!=" => (a, a != b),
        ">" => (a, a > b),
        "<" => (a, a < b),
        ">=" => (a, a >= b),
        "<=" => (a, a <= b),
        _ => bail!("unsupported operator: {op}"),
    })
}

/// {a="1", b="2"} for error messages
fn fmt_labels(l: &Labels) -> String {
    format!("{{{}}}", l.iter().map(|(k, v)| format!("{k}={v:?}")).collect::<Vec<_>>().join(", "))
}

/// Signature used to match series: on(...) keeps those labels; ignoring(...)
/// (or nothing) drops those and the name.
fn signature(l: &Labels, matching: &Option<LabelModifier>) -> Labels {
    match matching {
        Some(LabelModifier::Include(on)) => on.labels.iter().filter_map(|k| l.get(k).map(|v| (k.clone(), v.clone()))).collect(),
        Some(LabelModifier::Exclude(ign)) => l
            .iter()
            .filter(|(k, _)| *k != "__name__" && !ign.labels.contains(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        None => l.iter().filter(|(k, _)| *k != "__name__").map(|(k, v)| (k.clone(), v.clone())).collect(),
    }
}

fn group_key(l: &Labels, m: &Option<LabelModifier>) -> Labels {
    match m {
        Some(LabelModifier::Include(by)) => by.labels.iter().filter_map(|k| l.get(k).map(|v| (k.clone(), v.clone()))).collect(),
        Some(LabelModifier::Exclude(without)) => l
            .iter()
            .filter(|(k, _)| *k != "__name__" && !without.labels.contains(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        None => Labels::new(),
    }
}

/// Merges series with the same labels (output of operations that can produce
/// the same series at different steps)
struct Collector {
    idx: HashMap<Labels, usize>,
    out: Vec<Series>,
    n: usize,
}

impl Collector {
    fn new(n: usize) -> Self {
        Self {
            idx: HashMap::new(),
            out: Vec::new(),
            n,
        }
    }
    fn slot(&mut self, l: Labels) -> usize {
        let n = self.n;
        let out = &mut self.out;
        *self.idx.entry(l.clone()).or_insert_with(|| {
            out.push(Series {
                labels: l,
                vals: vec![None; n],
            });
            out.len() - 1
        })
    }
    fn set(&mut self, l: Labels, i: usize, v: f64) {
        let s = self.slot(l);
        self.out[s].vals[i] = Some(v);
    }
    fn finish(self) -> Vec<Series> {
        self.out
    }
}

// ── Functions over a window of samples ──────────────────────────────────────

/// rate/increase/delta as in Prometheus: extrapolates to the window edges when
/// samples do not reach them, and corrects counter resets.
fn extrapolated(ts: &[i64], vs: &[f64], from: i64, to: i64, counter: bool, is_rate: bool) -> Option<f64> {
    if ts.len() < 2 {
        return None;
    }
    let first = vs[0];
    let last = *vs.last()?;
    let mut result = last - first;
    if counter {
        let mut prev = first;
        for &v in &vs[1..] {
            if v < prev {
                result += prev;
            }
            prev = v;
        }
    }
    let sampled = (ts[ts.len() - 1] - ts[0]) as f64;
    if sampled <= 0.0 {
        return None;
    }
    let mut to_start = (ts[0] - from) as f64;
    let to_end = (to - ts[ts.len() - 1]) as f64;
    let avg = sampled / (ts.len() - 1) as f64;
    if counter && result > 0.0 && first >= 0.0 {
        let to_zero = sampled * (first / result);
        if to_zero < to_start {
            to_start = to_zero;
        }
    }
    let threshold = avg * 1.1;
    let mut interval = sampled;
    interval += if to_start < threshold { to_start } else { avg / 2.0 };
    interval += if to_end < threshold { to_end } else { avg / 2.0 };
    result *= interval / sampled;
    if is_rate {
        result /= (to - from) as f64;
    }
    Some(result)
}

fn instant_delta(ts: &[i64], vs: &[f64], counter: bool, is_rate: bool) -> Option<f64> {
    let n = ts.len();
    if n < 2 {
        return None;
    }
    let (a, b) = (vs[n - 2], vs[n - 1]);
    let mut d = b - a;
    if counter && b < a {
        d = b;
    }
    if is_rate {
        let dt = (ts[n - 1] - ts[n - 2]) as f64;
        if dt <= 0.0 {
            return None;
        }
        d /= dt;
    }
    Some(d)
}

fn linear_regression(ts: &[i64], vs: &[f64], at: i64) -> Option<(f64, f64)> {
    if ts.len() < 2 {
        return None;
    }
    let n = ts.len() as f64;
    let (mut sx, mut sy, mut sxy, mut sxx) = (0.0, 0.0, 0.0, 0.0);
    for (&t, &v) in ts.iter().zip(vs) {
        let x = (t - at) as f64;
        sx += x;
        sy += v;
        sxy += x * v;
        sxx += x * x;
    }
    let cov = sxy - sx * sy / n;
    let var = sxx - sx * sx / n;
    if var == 0.0 {
        return None;
    }
    let slope = cov / var;
    let intercept = sy / n - slope * sx / n;
    Some((slope, intercept))
}

fn quantile(q: f64, vals: &mut [f64]) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    if q < 0.0 {
        return f64::NEG_INFINITY;
    }
    if q > 1.0 {
        return f64::INFINITY;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = q * (vals.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(vals.len() - 1);
    let w = rank - lo as f64;
    vals[lo] * (1.0 - w) + vals[hi] * w
}

fn stdvar(vals: &[f64]) -> f64 {
    let n = vals.len() as f64;
    let mean = vals.iter().sum::<f64>() / n;
    vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n
}

/// Quantile of a histogram with cumulative buckets (le, count), like
/// Prometheus histogram_quantile
fn bucket_quantile(q: f64, buckets: &mut [(f64, f64)]) -> f64 {
    if q < 0.0 {
        return f64::NEG_INFINITY;
    }
    if q > 1.0 {
        return f64::INFINITY;
    }
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if buckets.len() < 2 || buckets.last().map(|b| b.0) != Some(f64::INFINITY) {
        return f64::NAN;
    }
    // Counts must be monotonic (partial scrapes can break that)
    for i in 1..buckets.len() {
        if buckets[i].1 < buckets[i - 1].1 {
            buckets[i].1 = buckets[i - 1].1;
        }
    }
    let total = buckets[buckets.len() - 1].1;
    if total == 0.0 {
        return f64::NAN;
    }
    let rank = q * total;
    let b = buckets.iter().position(|(_, c)| *c >= rank).unwrap_or(buckets.len() - 1);
    if b == buckets.len() - 1 {
        return buckets[buckets.len() - 2].0;
    }
    if b == 0 && buckets[0].0 <= 0.0 {
        return buckets[0].0;
    }
    let (start, prev_count) = if b == 0 { (0.0, 0.0) } else { (buckets[b - 1].0, buckets[b - 1].1) };
    let (end, count) = buckets[b];
    let in_bucket = count - prev_count;
    if in_bucket == 0.0 {
        return end;
    }
    start + (end - start) * ((rank - prev_count) / in_bucket)
}

impl Evaluator<'_> {
    fn time(&self, i: usize) -> i64 {
        self.start + i as i64 * self.step
    }

    pub fn run(&self, e: &Expr) -> Result<Output> {
        Ok(match self.eval(e)? {
            Val::Vector(v) => Output::Matrix(
                v.into_iter()
                    .filter_map(|s| {
                        let pts: Vec<(i64, f64)> = s.vals.iter().enumerate().filter_map(|(i, v)| v.map(|v| (self.time(i), v))).collect();
                        (!pts.is_empty()).then_some((s.labels, pts))
                    })
                    .collect(),
            ),
            Val::Scalar(s) => Output::Scalar(s.into_iter().enumerate().map(|(i, v)| (self.time(i), v)).collect()),
            Val::Str(s) => Output::Str(s),
        })
    }

    fn eval(&self, e: &Expr) -> Result<Val> {
        match e {
            Expr::NumberLiteral(n) => Ok(Val::Scalar(vec![n.val; self.n])),
            Expr::StringLiteral(s) => Ok(Val::Str(s.val.clone())),
            Expr::Paren(p) => self.eval(&p.expr),
            Expr::Unary(u) => match self.eval(&u.expr)? {
                Val::Scalar(s) => Ok(Val::Scalar(s.into_iter().map(|v| -v).collect())),
                Val::Vector(v) => Ok(Val::Vector(
                    v.into_iter()
                        .map(|s| Series {
                            labels: drop_name(s.labels),
                            vals: s.vals.into_iter().map(|v| v.map(|v| -v)).collect(),
                        })
                        .collect(),
                )),
                Val::Str(_) => bail!("cannot negate a string"),
            },
            Expr::VectorSelector(vs) => Ok(Val::Vector(self.select(vs))),
            Expr::MatrixSelector(_) => bail!("a range vector can only be used inside a function (rate, *_over_time…)"),
            Expr::Subquery(_) => bail!("subqueries are not supported"),
            Expr::Aggregate(a) => self.aggregate(a),
            Expr::Binary(b) => self.binary(b),
            Expr::Call(c) => self.call(c),
            Expr::Extension(_) => bail!("unsupported expression"),
        }
    }

    fn vector(&self, e: &Expr) -> Result<Vec<Series>> {
        match self.eval(e)? {
            Val::Vector(v) => Ok(v),
            _ => bail!("expected an instant vector in {e}"),
        }
    }

    fn scalar(&self, e: &Expr) -> Result<Vec<f64>> {
        match self.eval(e)? {
            Val::Scalar(s) => Ok(s),
            _ => bail!("expected a scalar in {e}"),
        }
    }

    fn string(&self, e: &Expr) -> Result<String> {
        match self.eval(e)? {
            Val::Str(s) => Ok(s),
            _ => bail!("expected a string in {e}"),
        }
    }

    /// Evaluation times of a selector (with offset and @)
    fn eval_times(&self, vs: &VectorSelector) -> Vec<i64> {
        let off = offset_secs(&vs.offset);
        let at = at_secs(&vs.at, self.start, self.end);
        (0..self.n).map(|i| at.unwrap_or(self.time(i)) - off).collect()
    }

    fn samples(&self, vs: &VectorSelector) -> &[Samples] {
        self.data.get(&sel_key(vs)).map(Vec::as_slice).unwrap_or_default()
    }

    /// Latest value of each series in (t - lookback, t]
    fn select(&self, vs: &VectorSelector) -> Vec<Series> {
        self.select_with(vs, |_, v| v)
    }

    /// Instant selector: the latest sample within the lookback at each step,
    /// mapped through `f(sample time, value)`
    fn select_with(&self, vs: &VectorSelector, f: impl Fn(i64, f64) -> f64) -> Vec<Series> {
        let times = self.eval_times(vs);
        self.samples(vs)
            .iter()
            .filter_map(|s| {
                let mut vals = vec![None; self.n];
                let mut any = false;
                for (i, &t) in times.iter().enumerate() {
                    let j = s.ts.partition_point(|&x| x <= t);
                    // Nor past a staleness marker after it, as in Prometheus
                    let k = s.stale.partition_point(|&x| x <= t);
                    if j > 0 && s.ts[j - 1] > t - self.lookback && (k == 0 || s.stale[k - 1] <= s.ts[j - 1]) {
                        vals[i] = Some(f(s.ts[j - 1], s.vals[j - 1]));
                        any = true;
                    }
                }
                any.then(|| Series {
                    labels: s.labels.clone(),
                    vals,
                })
            })
            .collect()
    }

    /// Applies `f` to the window (t - range, t] of each series at each step
    fn over_range(&self, ms: &MatrixSelector, keep_name: bool, f: impl Fn(&[i64], &[f64], i64, i64) -> Option<f64>) -> Vec<Series> {
        let range = (ms.range.as_secs() as i64).max(self.min_range);
        let times = self.eval_times(&ms.vs);
        self.samples(&ms.vs)
            .iter()
            .filter_map(|s| {
                let mut vals = vec![None; self.n];
                let mut any = false;
                for (i, &t) in times.iter().enumerate() {
                    let lo = s.ts.partition_point(|&x| x <= t - range);
                    let hi = s.ts.partition_point(|&x| x <= t);
                    if lo < hi
                        && let Some(v) = f(&s.ts[lo..hi], &s.vals[lo..hi], t - range, t)
                    {
                        vals[i] = Some(v);
                        any = true;
                    }
                }
                let labels = if keep_name { s.labels.clone() } else { drop_name(s.labels.clone()) };
                any.then_some(Series { labels, vals })
            })
            .collect()
    }

    fn aggregate(&self, a: &AggregateExpr) -> Result<Val> {
        let op = a.op.to_string();
        let op = op.as_str();
        let input = self.vector(&a.expr)?;
        let param = match &a.param {
            Some(p) if matches!(op, "topk" | "bottomk" | "quantile" | "limitk") => Some(self.scalar(p)?),
            Some(p) if op == "count_values" => {
                let label = self.string(p)?;
                return Ok(Val::Vector(self.count_values(&label, input, &a.modifier)));
            }
            _ => None,
        };

        // Groups: key → series
        let mut groups: Vec<(Labels, Vec<usize>)> = Vec::new();
        let mut gidx: HashMap<Labels, usize> = HashMap::new();
        for (i, s) in input.iter().enumerate() {
            let k = group_key(&s.labels, &a.modifier);
            let g = *gidx.entry(k.clone()).or_insert_with(|| {
                groups.push((k, Vec::new()));
                groups.len() - 1
            });
            groups[g].1.push(i);
        }

        if matches!(op, "topk" | "bottomk" | "limitk") {
            let k = param.unwrap_or_default();
            let mut out: Vec<Series> = input
                .iter()
                .map(|s| Series {
                    labels: s.labels.clone(),
                    vals: vec![None; self.n],
                })
                .collect();
            for (_, members) in &groups {
                for (i, ki) in k.iter().enumerate().take(self.n) {
                    let kk = ki.max(0.0) as usize;
                    let mut vals: Vec<(usize, f64)> = members.iter().filter_map(|&m| input[m].vals[i].map(|v| (m, v))).collect();
                    match op {
                        "topk" => vals.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Less)),
                        "bottomk" => vals.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Less)),
                        _ => {}
                    }
                    for &(m, v) in vals.iter().take(kk) {
                        out[m].vals[i] = Some(v);
                    }
                }
            }
            return Ok(Val::Vector(out.into_iter().filter(|s| s.vals.iter().any(Option::is_some)).collect()));
        }

        let mut out = Vec::new();
        for (key, members) in groups {
            let mut vals = vec![None; self.n];
            for (i, slot) in vals.iter_mut().enumerate() {
                let mut xs: Vec<f64> = members.iter().filter_map(|&m| input[m].vals[i]).collect();
                if xs.is_empty() {
                    continue;
                }
                *slot = Some(match op {
                    "sum" => xs.iter().sum(),
                    "avg" => xs.iter().sum::<f64>() / xs.len() as f64,
                    "min" => xs.iter().copied().filter(|v| !v.is_nan()).fold(f64::NAN, f64::min),
                    "max" => xs.iter().copied().filter(|v| !v.is_nan()).fold(f64::NAN, f64::max),
                    "count" => xs.len() as f64,
                    "group" => 1.0,
                    "stddev" => stdvar(&xs).sqrt(),
                    "stdvar" => stdvar(&xs),
                    "quantile" => quantile(param.as_ref().map(|p| p[i]).unwrap_or(f64::NAN), &mut xs),
                    _ => bail!("unsupported aggregation: {op}"),
                });
            }
            out.push(Series { labels: key, vals });
        }
        Ok(Val::Vector(out))
    }

    fn count_values(&self, label: &str, input: Vec<Series>, m: &Option<LabelModifier>) -> Vec<Series> {
        let mut c = Collector::new(self.n);
        let mut counts: HashMap<(Labels, usize), f64> = HashMap::new();
        for s in &input {
            for (i, v) in s.vals.iter().enumerate() {
                if let Some(v) = v {
                    let mut k = group_key(&s.labels, m);
                    k.insert(label.to_owned(), super::fmt_value(*v));
                    *counts.entry((k, i)).or_default() += 1.0;
                }
            }
        }
        for ((k, i), n) in counts {
            c.set(k, i, n);
        }
        c.finish()
    }

    fn binary(&self, b: &BinaryExpr) -> Result<Val> {
        let op = b.op.to_string();
        let op = op.as_str();
        let return_bool = b.return_bool();
        let lhs = self.eval(&b.lhs)?;
        let rhs = self.eval(&b.rhs)?;
        let matching = b.modifier.as_ref().and_then(|m| m.matching.clone());
        let card = b.modifier.as_ref().map(|m| m.card.clone()).unwrap_or(VectorMatchCardinality::OneToOne);
        let drop = is_arith(op) || return_bool;

        Ok(match (lhs, rhs) {
            (Val::Scalar(a), Val::Scalar(c)) => {
                let mut out = Vec::with_capacity(self.n);
                for i in 0..self.n {
                    let (v, keep) = apply(op, a[i], c[i])?;
                    out.push(if is_cmp(op) { if keep { 1.0 } else { 0.0 } } else { v });
                }
                Val::Scalar(out)
            }
            (Val::Vector(v), Val::Scalar(s)) => Val::Vector(self.vector_scalar(op, v, &s, false, return_bool, drop)?),
            (Val::Scalar(s), Val::Vector(v)) => Val::Vector(self.vector_scalar(op, v, &s, true, return_bool, drop)?),
            (Val::Vector(l), Val::Vector(r)) => match op {
                "and" | "or" | "unless" => Val::Vector(self.set_op(op, l, r, &matching)),
                _ => Val::Vector(self.vector_vector(op, l, r, &matching, card, return_bool, drop)?),
            },
            _ => bail!("invalid operands for {op}"),
        })
    }

    fn vector_scalar(&self, op: &str, v: Vec<Series>, s: &[f64], swap: bool, return_bool: bool, drop: bool) -> Result<Vec<Series>> {
        let mut out = Vec::new();
        for ser in v {
            let mut vals = vec![None; self.n];
            for i in 0..self.n {
                let Some(x) = ser.vals[i] else { continue };
                let (a, c) = if swap { (s[i], x) } else { (x, s[i]) };
                let (mut r, keep) = apply(op, a, c)?;
                // A comparison always keeps the vector's value
                if is_cmp(op) {
                    r = x;
                }
                if return_bool {
                    vals[i] = Some(if keep { 1.0 } else { 0.0 });
                } else if keep {
                    vals[i] = Some(r);
                }
            }
            if vals.iter().any(Option::is_some) {
                out.push(Series {
                    labels: if drop { drop_name(ser.labels) } else { ser.labels },
                    vals,
                });
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn vector_vector(
        &self,
        op: &str,
        l: Vec<Series>,
        r: Vec<Series>,
        matching: &Option<LabelModifier>,
        card: VectorMatchCardinality,
        return_bool: bool,
        drop: bool,
    ) -> Result<Vec<Series>> {
        // The "many" side goes on the left; group_right swaps them
        let (many, one, swapped, include) = match &card {
            VectorMatchCardinality::OneToMany(inc) => (r, l, true, inc.labels.clone()),
            VectorMatchCardinality::ManyToOne(inc) => (l, r, false, inc.labels.clone()),
            _ => (l, r, false, Vec::new()),
        };
        let one_to_one = matches!(card, VectorMatchCardinality::OneToOne);
        let many_sig: Vec<Labels> = many.iter().map(|s| signature(&s.labels, matching)).collect();
        let one_sig: Vec<Labels> = one.iter().map(|s| signature(&s.labels, matching)).collect();
        let mut by_sig: HashMap<&Labels, Vec<usize>> = HashMap::new();
        for (j, sg) in one_sig.iter().enumerate() {
            by_sig.entry(sg).or_default().push(j);
        }

        let mut c = Collector::new(self.n);
        let mut label_cache: HashMap<(usize, usize), Labels> = HashMap::new();
        // One-to-one: each signature may appear only once per step on the left side
        let mut seen: HashMap<(usize, &Labels), usize> = HashMap::new();
        for (m, ms) in many.iter().enumerate() {
            let Some(cands) = by_sig.get(&many_sig[m]) else { continue };
            for i in 0..self.n {
                let Some(mv) = ms.vals[i] else { continue };
                // As in Prometheus, the "one" side must be unique at each step; with
                // two candidates the result would depend on series order
                let mut with_value = cands.iter().filter_map(|&j| one[j].vals[i].map(|v| (j, v)));
                let Some((j, ov)) = with_value.next() else {
                    continue;
                };
                if with_value.next().is_some() {
                    bail!(
                        "found duplicate series for the match group {} on the {} hand-side of the operation; many-to-many matching not allowed: matching labels must be unique on one side",
                        fmt_labels(&many_sig[m]),
                        if swapped { "left" } else { "right" }
                    );
                }
                if one_to_one
                    && let Some(prev) = seen.insert((i, &many_sig[m]), m)
                    && prev != m
                {
                    bail!(
                        "found duplicate series for the match group {} on the left hand-side of the operation; many-to-many matching not allowed: matching labels must be unique on one side",
                        fmt_labels(&many_sig[m])
                    );
                }
                let (a, bv) = if swapped { (ov, mv) } else { (mv, ov) };
                let (mut v, keep) = apply(op, a, bv)?;
                if is_cmp(op) {
                    v = a;
                }
                if !keep && !return_bool {
                    continue;
                }
                if return_bool {
                    v = if keep { 1.0 } else { 0.0 };
                }
                let labels = label_cache
                    .entry((m, j))
                    .or_insert_with(|| {
                        let mut out = ms.labels.clone();
                        if drop {
                            out.remove("__name__");
                        }
                        if one_to_one {
                            match matching {
                                Some(LabelModifier::Include(on)) => out.retain(|k, _| on.labels.contains(k)),
                                Some(LabelModifier::Exclude(ign)) => out.retain(|k, _| !ign.labels.contains(k)),
                                None => {}
                            }
                        }
                        for k in &include {
                            match one[j].labels.get(k) {
                                Some(v) => {
                                    out.insert(k.clone(), v.clone());
                                }
                                None => {
                                    out.remove(k);
                                }
                            }
                        }
                        out
                    })
                    .clone();
                c.set(labels, i, v);
            }
        }
        Ok(c.finish())
    }

    fn set_op(&self, op: &str, l: Vec<Series>, r: Vec<Series>, matching: &Option<LabelModifier>) -> Vec<Series> {
        let rsig: Vec<Labels> = r.iter().map(|s| signature(&s.labels, matching)).collect();
        let lsig: Vec<Labels> = l.iter().map(|s| signature(&s.labels, matching)).collect();
        // Per step: signatures present on each side
        let present = |side: &[Series], sigs: &[Labels], i: usize| -> HashSet<Labels> {
            side.iter().zip(sigs).filter(|(s, _)| s.vals[i].is_some()).map(|(_, g)| g.clone()).collect()
        };
        let mut out: Vec<Series> = Vec::new();
        match op {
            "and" | "unless" => {
                let mut res: Vec<Series> = l
                    .iter()
                    .map(|s| Series {
                        labels: s.labels.clone(),
                        vals: vec![None; self.n],
                    })
                    .collect();
                for i in 0..self.n {
                    let rp = present(&r, &rsig, i);
                    for (k, s) in l.iter().enumerate() {
                        if let Some(v) = s.vals[i]
                            && rp.contains(&lsig[k]) == (op == "and")
                        {
                            res[k].vals[i] = Some(v);
                        }
                    }
                }
                out.extend(res);
            }
            _ => {
                let mut extra: Vec<Series> = r
                    .iter()
                    .map(|s| Series {
                        labels: s.labels.clone(),
                        vals: vec![None; self.n],
                    })
                    .collect();
                for i in 0..self.n {
                    let lp = present(&l, &lsig, i);
                    for (k, s) in r.iter().enumerate() {
                        if let Some(v) = s.vals[i]
                            && !lp.contains(&rsig[k])
                        {
                            extra[k].vals[i] = Some(v);
                        }
                    }
                }
                out.extend(l);
                // As in Prometheus, a range query merges what each step yields per
                // label set: `sum(x) or vector(0)` is ONE series {} with the left
                // side's steps and, where missing, the right side's.
                let mut idx: HashMap<Labels, usize> = out.iter().enumerate().map(|(i, s)| (s.labels.clone(), i)).collect();
                for e in extra {
                    match idx.get(&e.labels) {
                        Some(&i) => {
                            for (dst, v) in out[i].vals.iter_mut().zip(e.vals) {
                                if dst.is_none() {
                                    *dst = v;
                                }
                            }
                        }
                        None => {
                            idx.insert(e.labels.clone(), out.len());
                            out.push(e);
                        }
                    }
                }
            }
        }
        out.retain(|s| s.vals.iter().any(Option::is_some));
        out
    }

    fn matrix<'b>(&self, c: &'b Call, i: usize) -> Result<&'b MatrixSelector> {
        match c.args.args.get(i).map(|a| a.as_ref()) {
            Some(Expr::MatrixSelector(ms)) => Ok(ms),
            Some(Expr::Subquery(_)) => bail!("subqueries are not supported"),
            _ => bail!("{} expects a range vector", c.func.name),
        }
    }

    fn arg<'b>(&self, c: &'b Call, i: usize) -> Result<&'b Expr> {
        c.args
            .args
            .get(i)
            .map(|a| a.as_ref())
            .ok_or_else(|| anyhow!("{}: missing argument {}", c.func.name, i + 1))
    }

    fn map_vector(&self, c: &Call, f: impl Fn(f64, usize) -> f64) -> Result<Val> {
        let v = self.vector(self.arg(c, 0)?)?;
        Ok(Val::Vector(
            v.into_iter()
                .map(|s| Series {
                    labels: drop_name(s.labels),
                    vals: s.vals.into_iter().enumerate().map(|(i, v)| v.map(|v| f(v, i))).collect(),
                })
                .collect(),
        ))
    }

    fn opt_scalar(&self, c: &Call, i: usize, default: f64) -> Result<Vec<f64>> {
        match c.args.args.get(i) {
            Some(a) => self.scalar(a),
            None => Ok(vec![default; self.n]),
        }
    }

    fn call(&self, c: &Call) -> Result<Val> {
        let name = c.func.name;
        let counterish = |n: &str| matches!(n, "rate" | "increase" | "irate" | "resets");
        Ok(match name {
            "rate" | "increase" | "delta" => {
                let ms = self.matrix(c, 0)?;
                let counter = name != "delta";
                let is_rate = name == "rate";
                Val::Vector(self.over_range(ms, false, |ts, vs, from, to| extrapolated(ts, vs, from, to, counter, is_rate)))
            }
            "irate" | "idelta" => {
                let ms = self.matrix(c, 0)?;
                let counter = counterish(name);
                let is_rate = name == "irate";
                Val::Vector(self.over_range(ms, false, |ts, vs, _, _| instant_delta(ts, vs, counter, is_rate)))
            }
            "changes" | "resets" => {
                let ms = self.matrix(c, 0)?;
                let resets = name == "resets";
                Val::Vector(self.over_range(ms, false, |_, vs, _, _| {
                    Some(vs.windows(2).filter(|w| if resets { w[1] < w[0] } else { w[1] != w[0] }).count() as f64)
                }))
            }
            "deriv" => {
                let ms = self.matrix(c, 0)?;
                Val::Vector(self.over_range(ms, false, |ts, vs, _, _| linear_regression(ts, vs, ts[0]).map(|(s, _)| s)))
            }
            "predict_linear" => {
                let ms = self.matrix(c, 0)?;
                let secs = self.scalar(self.arg(c, 1)?)?;
                let d = secs.first().copied().unwrap_or(0.0);
                Val::Vector(self.over_range(ms, false, |ts, vs, _, to| linear_regression(ts, vs, to).map(|(s, i)| i + s * d)))
            }
            "avg_over_time" | "min_over_time" | "max_over_time" | "sum_over_time" | "count_over_time" | "last_over_time" | "present_over_time"
            | "stddev_over_time" | "stdvar_over_time" => {
                let ms = self.matrix(c, 0)?;
                let keep = name == "last_over_time";
                Val::Vector(self.over_range(ms, keep, |_, vs, _, _| {
                    Some(match name {
                        "avg_over_time" => vs.iter().sum::<f64>() / vs.len() as f64,
                        "min_over_time" => vs.iter().copied().fold(f64::INFINITY, f64::min),
                        "max_over_time" => vs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                        "sum_over_time" => vs.iter().sum(),
                        "count_over_time" => vs.len() as f64,
                        "last_over_time" => *vs.last()?,
                        "present_over_time" => 1.0,
                        "stddev_over_time" => stdvar(vs).sqrt(),
                        _ => stdvar(vs),
                    })
                }))
            }
            "quantile_over_time" => {
                let q = self.scalar(self.arg(c, 0)?)?;
                let q = q.first().copied().unwrap_or(f64::NAN);
                let ms = self.matrix(c, 1)?;
                Val::Vector(self.over_range(ms, false, |_, vs, _, _| Some(quantile(q, &mut vs.to_vec()))))
            }
            "abs" => self.map_vector(c, |v, _| v.abs())?,
            "ceil" => self.map_vector(c, |v, _| v.ceil())?,
            "floor" => self.map_vector(c, |v, _| v.floor())?,
            "exp" => self.map_vector(c, |v, _| v.exp())?,
            "ln" => self.map_vector(c, |v, _| v.ln())?,
            "log2" => self.map_vector(c, |v, _| v.log2())?,
            "log10" => self.map_vector(c, |v, _| v.log10())?,
            "sqrt" => self.map_vector(c, |v, _| v.sqrt())?,
            "sgn" => self.map_vector(c, |v, _| {
                if v > 0.0 {
                    1.0
                } else if v < 0.0 {
                    -1.0
                } else {
                    v
                }
            })?,
            "round" => {
                let to = self.opt_scalar(c, 1, 1.0)?;
                self.map_vector(c, |v, i| if to[i] == 0.0 { v } else { (v / to[i]).round() * to[i] })?
            }
            "clamp" => {
                let (lo, hi) = (self.scalar(self.arg(c, 1)?)?, self.scalar(self.arg(c, 2)?)?);
                self.map_vector(c, |v, i| if lo[i] > hi[i] { f64::NAN } else { v.max(lo[i]).min(hi[i]) })?
            }
            "clamp_min" => {
                let lo = self.scalar(self.arg(c, 1)?)?;
                self.map_vector(c, |v, i| v.max(lo[i]))?
            }
            "clamp_max" => {
                let hi = self.scalar(self.arg(c, 1)?)?;
                self.map_vector(c, |v, i| v.min(hi[i]))?
            }
            "timestamp" => {
                // On a selector, the time of the chosen sample (as in Prometheus, so
                // time() - timestamp(x) works); on any other expression, the step time
                if let Some(Expr::VectorSelector(vs)) = c.args.args.first().map(|a| a.as_ref()) {
                    Val::Vector(
                        self.select_with(vs, |t, _| t as f64)
                            .into_iter()
                            .map(|s| Series {
                                labels: drop_name(s.labels),
                                vals: s.vals,
                            })
                            .collect(),
                    )
                } else {
                    let times: Vec<f64> = (0..self.n).map(|i| self.time(i) as f64).collect();
                    self.map_vector(c, |_, i| times[i])?
                }
            }
            "scalar" => {
                let v = self.vector(self.arg(c, 0)?)?;
                Val::Scalar(
                    (0..self.n)
                        .map(|i| {
                            let mut it = v.iter().filter_map(|s| s.vals[i]);
                            match (it.next(), it.next()) {
                                (Some(x), None) => x,
                                _ => f64::NAN,
                            }
                        })
                        .collect(),
                )
            }
            "vector" => {
                let s = self.scalar(self.arg(c, 0)?)?;
                Val::Vector(vec![Series {
                    labels: Labels::new(),
                    vals: s.into_iter().map(Some).collect(),
                }])
            }
            "time" => Val::Scalar((0..self.n).map(|i| self.time(i) as f64).collect()),
            "pi" => Val::Scalar(vec![std::f64::consts::PI; self.n]),
            "sort" | "sort_desc" => {
                let mut v = self.vector(self.arg(c, 0)?)?;
                // By the value at the last step (the only one in an instant query);
                // series without a value there, or NaN, go last as in Prometheus
                let last = |s: &Series| s.vals.last().copied().flatten().filter(|x| !x.is_nan());
                v.sort_by(|a, b| match (last(a), last(b)) {
                    (Some(x), Some(y)) if name == "sort" => x.total_cmp(&y),
                    (Some(x), Some(y)) => y.total_cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
                Val::Vector(v)
            }
            "sort_by_label" | "sort_by_label_desc" => {
                let mut v = self.vector(self.arg(c, 0)?)?;
                let names = (1..c.args.args.len()).map(|i| self.string(self.arg(c, i)?)).collect::<Result<Vec<_>>>()?;
                let key = |s: &Series| names.iter().map(|n| s.labels.get(n).cloned().unwrap_or_default()).collect::<Vec<_>>();
                // Ties are broken by the whole label set, as in Prometheus
                v.sort_by(|a, b| {
                    let o = key(a).cmp(&key(b)).then_with(|| a.labels.cmp(&b.labels));
                    if name == "sort_by_label" { o } else { o.reverse() }
                });
                Val::Vector(v)
            }
            "label_replace" => {
                let v = self.vector(self.arg(c, 0)?)?;
                let dst = self.string(self.arg(c, 1)?)?;
                let repl = self.string(self.arg(c, 2)?)?;
                let src = self.string(self.arg(c, 3)?)?;
                let re = Regex::new(&format!("^(?:{})$", self.string(self.arg(c, 4)?)?))?;
                let mut col = Collector::new(self.n);
                for s in v {
                    let val = s.labels.get(&src).cloned().unwrap_or_default();
                    let mut labels = s.labels;
                    if let Some(caps) = re.captures(&val) {
                        let mut out = String::new();
                        caps.expand(&repl, &mut out);
                        if out.is_empty() {
                            labels.remove(&dst);
                        } else {
                            labels.insert(dst.clone(), out);
                        }
                    }
                    for (i, x) in s.vals.into_iter().enumerate() {
                        if let Some(x) = x {
                            col.set(labels.clone(), i, x);
                        }
                    }
                }
                Val::Vector(col.finish())
            }
            "label_join" => {
                let v = self.vector(self.arg(c, 0)?)?;
                let dst = self.string(self.arg(c, 1)?)?;
                let sep = self.string(self.arg(c, 2)?)?;
                let srcs: Vec<String> = (3..c.args.args.len()).map(|i| self.string(self.arg(c, i)?)).collect::<Result<_>>()?;
                let mut col = Collector::new(self.n);
                for s in v {
                    let joined = srcs.iter().map(|k| s.labels.get(k).cloned().unwrap_or_default()).collect::<Vec<_>>().join(&sep);
                    let mut labels = s.labels;
                    if joined.is_empty() {
                        labels.remove(&dst);
                    } else {
                        labels.insert(dst.clone(), joined);
                    }
                    for (i, x) in s.vals.into_iter().enumerate() {
                        if let Some(x) = x {
                            col.set(labels.clone(), i, x);
                        }
                    }
                }
                Val::Vector(col.finish())
            }
            "histogram_quantile" => {
                let q = self.scalar(self.arg(c, 0)?)?;
                let v = self.vector(self.arg(c, 1)?)?;
                // Group by every label except le
                let mut groups: HashMap<Labels, Vec<(f64, usize)>> = HashMap::new();
                for (k, s) in v.iter().enumerate() {
                    let Some(le) = s.labels.get("le").and_then(|x| if x == "+Inf" { Some(f64::INFINITY) } else { x.parse().ok() }) else {
                        continue;
                    };
                    let mut key = drop_name(s.labels.clone());
                    key.remove("le");
                    groups.entry(key).or_default().push((le, k));
                }
                let mut out = Vec::new();
                for (key, members) in groups {
                    let mut vals = vec![None; self.n];
                    for (i, slot) in vals.iter_mut().enumerate() {
                        let mut b: Vec<(f64, f64)> = members.iter().filter_map(|&(le, k)| v[k].vals[i].map(|x| (le, x))).collect();
                        if b.is_empty() {
                            continue;
                        }
                        let r = bucket_quantile(q[i], &mut b);
                        if !r.is_nan() {
                            *slot = Some(r);
                        }
                    }
                    out.push(Series { labels: key, vals });
                }
                Val::Vector(out)
            }
            "absent" | "absent_over_time" => {
                let arg = self.arg(c, 0)?;
                let v = if name == "absent_over_time" {
                    let ms = self.matrix(c, 0)?;
                    self.over_range(ms, false, |_, _, _, _| Some(1.0))
                } else {
                    self.vector(arg)?
                };
                let vs = match arg {
                    Expr::VectorSelector(vs) => Some(vs),
                    Expr::MatrixSelector(ms) => Some(&ms.vs),
                    _ => None,
                };
                // Labels: the selector's equality matchers
                let mut labels = Labels::new();
                if let Some(vs) = vs {
                    for m in &vs.matchers.matchers {
                        if matches!(m.op, MatchOp::Equal) && m.name != "__name__" {
                            labels.insert(m.name.clone(), m.value.clone());
                        }
                    }
                }
                let vals = (0..self.n)
                    .map(|i| if v.iter().any(|s| s.vals[i].is_some()) { None } else { Some(1.0) })
                    .collect();
                Val::Vector(vec![Series { labels, vals }])
            }
            "minute" | "hour" | "day_of_week" | "day_of_month" | "day_of_year" | "days_in_month" | "month" | "year" => {
                use chrono::{Datelike, TimeZone, Timelike, Utc};
                let part = |t: f64| -> f64 {
                    let Some(d) = Utc.timestamp_opt(t as i64, 0).single() else { return f64::NAN };
                    match name {
                        "minute" => d.minute() as f64,
                        "hour" => d.hour() as f64,
                        "day_of_week" => d.weekday().num_days_from_sunday() as f64,
                        "day_of_month" => d.day() as f64,
                        "day_of_year" => d.ordinal() as f64,
                        "month" => d.month() as f64,
                        "year" => d.year() as f64,
                        _ => {
                            let (y, m) = (d.year(), d.month());
                            let next = if m == 12 {
                                Utc.with_ymd_and_hms(y + 1, 1, 1, 0, 0, 0)
                            } else {
                                Utc.with_ymd_and_hms(y, m + 1, 1, 0, 0, 0)
                            };
                            next.single().map(|n| (n - chrono::Duration::days(1)).day() as f64).unwrap_or(f64::NAN)
                        }
                    }
                };
                if c.args.args.is_empty() {
                    Val::Vector(vec![Series {
                        labels: Labels::new(),
                        vals: (0..self.n).map(|i| Some(part(self.time(i) as f64))).collect(),
                    }])
                } else {
                    self.map_vector(c, |v, _| part(v))?
                }
            }
            _ => bail!("unsupported function: {name}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(kv: &[(&str, &str)]) -> Labels {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    type Out = Vec<(Labels, Vec<(i64, f64)>)>;

    /// An instant query keeping the order the engine returns (`run` sorts by
    /// labels): the `mount` label and the value of each series
    fn run_ordered(q: &str, data: Vec<(&str, Vec<Samples>)>) -> Vec<(String, f64)> {
        let expr = promql_parser::parser::parse(q).unwrap();
        let data: HashMap<String, Vec<Samples>> = data.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let ev = Evaluator {
            start: 0,
            end: 0,
            step: 15,
            n: 1,
            lookback: 300,
            min_range: 0,
            data: &data,
        };
        match ev.run(&expr).unwrap() {
            Output::Matrix(m) => m.into_iter().map(|(l, p)| (l.get("mount").cloned().unwrap_or_default(), p[0].1)).collect(),
            _ => panic!("expected a vector"),
        }
    }

    #[test]
    fn sort_functions_order_the_result() {
        let used = |mount: &str, v: f64| Samples {
            labels: labels(&[("__name__", "used"), ("mount", mount)]),
            ts: vec![0],
            vals: vec![v],
            ..Default::default()
        };
        let data = || vec![("used", vec![used("/b", 20.0), used("/a", 30.0), used("/c", f64::NAN), used("/d", 10.0)])];
        let mounts = |q: &str| run_ordered(q, data()).into_iter().map(|(m, _)| m).collect::<Vec<_>>();
        // NaN goes last in both directions, as in Prometheus
        assert_eq!(mounts("sort(used)"), ["/d", "/b", "/a", "/c"]);
        assert_eq!(mounts("sort_desc(used)"), ["/a", "/b", "/d", "/c"]);
        assert_eq!(mounts(r#"sort_by_label(used, "mount")"#), ["/a", "/b", "/c", "/d"]);
        assert_eq!(mounts(r#"sort_by_label_desc(used, "mount")"#), ["/d", "/c", "/b", "/a"]);
    }

    fn run(q: &str, data: Vec<(&str, Vec<Samples>)>, start: i64, end: i64, step: i64) -> Out {
        try_run(q, data, start, end, step).unwrap()
    }

    /// Evaluates against test data: each selector is looked up by its text
    fn try_run(q: &str, data: Vec<(&str, Vec<Samples>)>, start: i64, end: i64, step: i64) -> Result<Out> {
        let expr = promql_parser::parser::parse(q).unwrap();
        let data: HashMap<String, Vec<Samples>> = data.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let ev = Evaluator {
            start,
            end,
            step,
            n: ((end - start) / step + 1) as usize,
            lookback: 300,
            min_range: 0,
            data: &data,
        };
        Ok(match ev.run(&expr)? {
            Output::Matrix(m) => {
                let mut m = m;
                m.sort_by(|a, b| a.0.cmp(&b.0));
                m
            }
            Output::Scalar(s) => vec![(Labels::new(), s)],
            Output::Str(_) => vec![],
        })
    }

    fn counter(l: Labels, start: i64, n: i64, step: i64, inc: f64) -> Samples {
        Samples {
            labels: l,
            ts: (0..n).map(|i| start + i * step).collect(),
            vals: (0..n).map(|i| i as f64 * inc).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_stale_series_has_no_value_past_its_marker() {
        // Every 15 s until 60, marked stale at 75 (its target stopped), back at 300
        let s = Samples {
            labels: labels(&[("__name__", "mem")]),
            ts: vec![0, 15, 30, 45, 60, 300],
            vals: vec![1.0, 1.0, 1.0, 1.0, 1.0, 2.0],
            stale: vec![75],
        };
        let r = run("mem", vec![("mem", vec![s.clone()])], 60, 300, 60);
        assert_eq!(r[0].1, [(60, 1.0), (300, 2.0)], "not for the rest of the lookback");
        // A range still has the samples before the marker, as in Prometheus
        let r = run("max_over_time(mem[2m])", vec![("mem", vec![s])], 120, 120, 60);
        assert_eq!(r[0].1, [(120, 1.0)]);
    }

    #[test]
    fn rate_of_steady_counter() {
        let s = counter(labels(&[("__name__", "x_total"), ("job", "a")]), 0, 100, 15, 30.0);
        let r = run("rate(x_total[1m])", vec![("x_total", vec![s])], 600, 900, 60);
        assert_eq!(r.len(), 1);
        assert!(!r[0].0.contains_key("__name__"));
        for (_, v) in &r[0].1 {
            assert!((v - 2.0).abs() < 1e-9, "{v}");
        }
    }

    #[test]
    fn rate_handles_reset() {
        let mut s = counter(labels(&[("__name__", "x_total")]), 0, 9, 15, 10.0);
        s.vals = vec![0.0, 10.0, 20.0, 30.0, 5.0, 15.0, 25.0, 35.0, 45.0];
        let r = run("increase(x_total[2m])", vec![("x_total", vec![s])], 120, 120, 60);
        // Window (0, 120]: 10 to 45 with a reset from 30 → 65 over 105 s of
        // samples, extrapolated to 120 s
        let v = r[0].1[0].1;
        assert!((v - 65.0 * 120.0 / 105.0).abs() < 1e-9, "{v}");
    }

    #[test]
    fn aggregation_and_matching() {
        let a = Samples {
            labels: labels(&[("__name__", "used"), ("instance", "a"), ("mount", "/")]),
            ts: vec![0],
            vals: vec![30.0],
            ..Default::default()
        };
        let b = Samples {
            labels: labels(&[("__name__", "used"), ("instance", "b"), ("mount", "/")]),
            ts: vec![0],
            vals: vec![10.0],
            ..Default::default()
        };
        let ta = Samples {
            labels: labels(&[("__name__", "size"), ("instance", "a"), ("mount", "/")]),
            ts: vec![0],
            vals: vec![100.0],
            ..Default::default()
        };
        let tb = Samples {
            labels: labels(&[("__name__", "size"), ("instance", "b"), ("mount", "/")]),
            ts: vec![0],
            vals: vec![40.0],
            ..Default::default()
        };
        let r = run(
            "100 * used / size",
            vec![("used", vec![a.clone(), b.clone()]), ("size", vec![ta, tb])],
            0,
            0,
            15,
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1[0].1, 30.0);
        assert_eq!(r[1].1[0].1, 25.0);
        let r = run("sum by (mount) (used)", vec![("used", vec![a.clone(), b.clone()])], 0, 0, 15);
        assert_eq!(r[0].0, labels(&[("mount", "/")]));
        assert_eq!(r[0].1[0].1, 40.0);
        let r = run("used > 20", vec![("used", vec![a.clone(), b.clone()])], 0, 0, 15);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0.get("__name__").map(String::as_str), Some("used"));
        let r = run("topk(1, used)", vec![("used", vec![a, b])], 0, 0, 15);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1[0].1, 30.0);
    }

    #[test]
    fn group_left_copies_labels() {
        let m = Samples {
            labels: labels(&[("__name__", "m"), ("instance", "a"), ("cpu", "0")]),
            ts: vec![0],
            vals: vec![2.0],
            ..Default::default()
        };
        let info = Samples {
            labels: labels(&[("__name__", "info"), ("instance", "a"), ("nodename", "host-1")]),
            ts: vec![0],
            vals: vec![1.0],
            ..Default::default()
        };
        let r = run(
            "m * on(instance) group_left(nodename) info",
            vec![("m", vec![m]), ("info", vec![info])],
            0,
            0,
            15,
        );
        assert_eq!(r[0].0, labels(&[("cpu", "0"), ("instance", "a"), ("nodename", "host-1")]));
        assert_eq!(r[0].1[0].1, 2.0);
    }

    #[test]
    fn histogram_quantile_interpolates() {
        let mk = |le: &str, v: f64| Samples {
            labels: labels(&[("__name__", "h_bucket"), ("le", le)]),
            ts: vec![0],
            vals: vec![v],
            ..Default::default()
        };
        let r = run(
            "histogram_quantile(0.5, h_bucket)",
            vec![("h_bucket", vec![mk("0.1", 0.0), mk("1", 10.0), mk("+Inf", 10.0)])],
            0,
            0,
            15,
        );
        assert!((r[0].1[0].1 - 0.55).abs() < 1e-9, "{:?}", r);
    }

    #[test]
    fn ambiguous_matching_is_an_error_in_any_order() {
        let a = Samples {
            labels: labels(&[("__name__", "a"), ("job", "x")]),
            ts: vec![0],
            vals: vec![10.0],
            ..Default::default()
        };
        let b1 = Samples {
            labels: labels(&[("__name__", "b"), ("job", "x"), ("i", "1")]),
            ts: vec![0],
            vals: vec![2.0],
            ..Default::default()
        };
        let b2 = Samples {
            labels: labels(&[("__name__", "b"), ("job", "x"), ("i", "2")]),
            ts: vec![0],
            vals: vec![5.0],
            ..Default::default()
        };
        for bs in [vec![b1.clone(), b2.clone()], vec![b2.clone(), b1.clone()]] {
            let e = try_run("a / on(job) b", vec![("a", vec![a.clone()]), ("b", bs)], 0, 0, 15).unwrap_err();
            assert!(e.to_string().contains("many-to-many"), "{e}");
        }
        // A single candidate still works, and group_left allows many on the left
        let r = run("a / on(job) b", vec![("a", vec![a.clone()]), ("b", vec![b1.clone()])], 0, 0, 15);
        assert_eq!(r[0].1[0].1, 5.0);
        let r = run("b / on(job) group_left a", vec![("a", vec![a]), ("b", vec![b1, b2])], 0, 0, 15);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn timestamp_of_a_selector_is_the_sample_time() {
        let a = Samples {
            labels: labels(&[("__name__", "a")]),
            ts: vec![-60],
            vals: vec![1.0],
            ..Default::default()
        };
        let r = run("timestamp(a)", vec![("a", vec![a.clone()])], 0, 0, 15);
        assert_eq!(r[0].1[0].1, -60.0);
        let r = run("time() - timestamp(a)", vec![("a", vec![a])], 0, 0, 15);
        assert_eq!(r[0].1[0].1, 60.0);
    }

    #[test]
    fn set_operators_and_scalar() {
        let a = Samples {
            labels: labels(&[("__name__", "a"), ("x", "1")]),
            ts: vec![0],
            vals: vec![1.0],
            ..Default::default()
        };
        let r = run("a or vector(0)", vec![("a", vec![a.clone()])], 0, 0, 15);
        assert_eq!(r.len(), 2);
        // Same label set on both sides → a single series that fills the gaps
        let r = run("sum(a) or vector(0)", vec![("a", vec![a.clone()])], 0, 30, 15);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1.len(), 3);
        assert_eq!(r[0].1[0].1, 1.0);
        let r = run("absent(nothing{job=\"z\"})", vec![], 0, 0, 15);
        assert_eq!(r[0].0, labels(&[("job", "z")]));
        let r = run("scalar(a) * 2", vec![("a", vec![a])], 0, 0, 15);
        assert_eq!(r[0].1[0].1, 2.0);
    }
}
