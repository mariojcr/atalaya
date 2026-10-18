// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Reads series and samples from ClickHouse for the PromQL engine.
//!
//! Each stored metric looks as it would in Prometheus: scalars under their
//! name and distributions (histograms) as `<name>_bucket{le}`, `_sum` and
//! `_count`. Labels are the exporter's plus `job` (the atalaya service) and
//! `instance` (the host).

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use promql_parser::label::{MatchOp, Matcher, Matchers};
use tokio::sync::Mutex;

use crate::ch::{ClickHouse, Param};

use super::Labels;

/// Retention of each tier (must match the schema TTLs)
pub const RAW_DAYS: i64 = 7;
pub const ONE_MINUTE_DAYS: i64 = 42;
/// Maximum series per selector
const MAX_SERIES: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Raw,
    OneMinute,
    TenMinutes,
}

impl Tier {
    /// Resolution in seconds
    pub fn res(self) -> i64 {
        match self {
            Tier::Raw => 15,
            Tier::OneMinute => 60,
            Tier::TenMinutes => 600,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Tier::Raw => "raw",
            Tier::OneMinute => "1m",
            Tier::TenMinutes => "10m",
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Tier::Raw => "",
            Tier::OneMinute => "_1m",
            Tier::TenMinutes => "_10m",
        }
    }
    /// The coarsest tier that loses no resolution for the requested step and
    /// still holds the oldest data needed.
    pub fn pick(oldest: i64, step: i64, now: i64) -> Tier {
        let age = now - oldest;
        if step >= 600 || age > ONE_MINUTE_DAYS * 86400 {
            Tier::TenMinutes
        } else if step >= 60 || age > RAW_DAYS * 86400 {
            Tier::OneMinute
        } else {
            Tier::Raw
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Value,
    Bucket(usize),
    Sum,
    Count,
}

/// Kind of a stored metric: cumulative counters (rates come from each bucket's
/// maximum), gauges or distributions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gauge,
    Counter,
    Distribution,
}

pub struct Catalog {
    metrics: Vec<(String, Kind)>,
}

impl Catalog {
    /// Names as PromQL sees them, with the stored metric and field
    fn virtual_names(&self) -> impl Iterator<Item = (String, &str, Kind, Option<Field>)> + '_ {
        self.metrics.iter().flat_map(|(m, k)| {
            let v: Vec<(String, &str, Kind, Option<Field>)> = if *k == Kind::Distribution {
                vec![
                    (format!("{m}_bucket"), m.as_str(), *k, None),
                    (format!("{m}_sum"), m.as_str(), *k, Some(Field::Sum)),
                    (format!("{m}_count"), m.as_str(), *k, Some(Field::Count)),
                ]
            } else {
                vec![(m.clone(), m.as_str(), *k, Some(Field::Value))]
            };
            v
        })
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.virtual_names().map(|(n, ..)| n).collect();
        // A scalar X_bucket (e.g. VictoriaMetrics vmrange histograms) and the
        // one from a distribution X can collide
        v.sort();
        v.dedup();
        v
    }
}

/// A series as PromQL sees it
pub struct SeriesRef {
    pub labels: Labels,
    id: u64,
    metric: String,
    kind: Kind,
    field: Field,
}

/// Samples of a series (Unix seconds, value), ordered by time
#[derive(Clone, Default)]
pub struct Samples {
    pub labels: Labels,
    pub ts: Vec<i64>,
    pub vals: Vec<f64>,
}

/// Resolved series rows of a query (cached for 60 s)
type SeriesList = Arc<Vec<serde_json::Value>>;
/// What is wanted from a stored metric: PromQL name, kind and field
type Wanted = (String, Kind, Option<Field>);
/// Distribution row: time, count, sum and buckets
type DistRow = (i64, f64, f64, Vec<f64>);

pub struct Store {
    ch: ClickHouse,
    db: String,
    catalog: Mutex<Option<(Instant, Arc<Catalog>)>>,
    /// `series` rows per filter: every dashboard query and alert rule repeats
    /// the same selectors and the list rarely changes (5 min; at 60 s the
    /// alerts, evaluated every 30 s, missed it every other round)
    series_cache: Mutex<HashMap<String, (Instant, SeriesList)>>,
}
const SERIES_CACHE_TTL: Duration = Duration::from_secs(300);
const SERIES_CACHE_MAX: usize = 1000;

fn guess_kind(metric: &str) -> Kind {
    if ["_total", "_count", "_sum", "_bucket"].iter().any(|s| metric.ends_with(s)) {
        Kind::Counter
    } else {
        Kind::Gauge
    }
}

/// PromQL labels of a stored series. Exporter labels that clash with job or
/// instance become exported_*, as in Prometheus. From the resource, the host is
/// `instance` and every other key is a label with its dots as underscores
/// (`nomad.group` → `nomad_group`), unless the series already has that label:
/// without them, two replicas of a job on one host were the same series.
fn label_view(name: &str, service: &str, resource: &BTreeMap<String, String>, labels: &BTreeMap<String, String>) -> Labels {
    let mut l: Labels = labels.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.clone(), v.clone())).collect();
    for k in ["job", "instance"] {
        if let Some(v) = l.remove(k) {
            l.insert(format!("exported_{k}"), v);
        }
    }
    if !service.is_empty() {
        l.insert("job".into(), service.into());
    }
    if let Some(h) = resource.get("host.name").filter(|h| !h.is_empty()) {
        l.insert("instance".into(), h.clone());
    }
    for (k, v) in resource {
        if k != "host.name" && !v.is_empty() {
            l.entry(label_name(k)).or_insert_with(|| v.clone());
        }
    }
    l.insert("__name__".into(), name.into());
    l
}

/// A resource key as a PromQL label name: anything outside [A-Za-z0-9_]
/// becomes `_`, and a leading digit gets a `_` in front
fn label_name(key: &str) -> String {
    let mut name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// `le` formatted as in Prometheus
fn fmt_le(b: f64) -> String {
    if b == f64::INFINITY { "+Inf".into() } else { super::fmt_value(b) }
}

fn matches(ms: &Matchers, labels: &Labels) -> bool {
    let one = |m: &Matcher| m.is_match(labels.get(&m.name).map(String::as_str).unwrap_or(""));
    let all = ms.matchers.iter().all(one);
    if ms.or_matchers.is_empty() {
        all
    } else {
        ms.or_matchers.iter().any(|group| group.iter().all(one))
    }
}

/// A selector's matchers, with the name added as a __name__ matcher
pub fn all_matchers(name: &Option<String>, ms: &Matchers) -> Matchers {
    let mut ms = ms.clone();
    if let Some(n) = name {
        ms.matchers.push(Matcher::new(MatchOp::Equal, "__name__", n));
        for g in ms.or_matchers.iter_mut() {
            g.push(Matcher::new(MatchOp::Equal, "__name__", n));
        }
    }
    ms
}

fn name_matchers_ok(ms: &Matchers, name: &str) -> bool {
    let check = |group: &[Matcher]| group.iter().filter(|m| m.name == "__name__").all(|m| m.is_match(name));
    if ms.or_matchers.is_empty() {
        check(&ms.matchers)
    } else {
        ms.or_matchers.iter().any(|g| check(g))
    }
}

/// RowBinary reader: little-endian integers and floats, arrays prefixed with
/// their LEB128 length
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn new(b: &[u8]) -> Cursor<'_> {
        Cursor { b, i: 0 }
    }
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.i..self.i + N)?;
        self.i += N;
        s.try_into().ok()
    }
    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_le_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take::<8>().map(i64::from_le_bytes)
    }
    fn f64(&mut self) -> Option<f64> {
        self.take::<8>().map(f64::from_le_bytes)
    }
    fn varint(&mut self) -> Option<usize> {
        let (mut n, mut shift) = (0usize, 0);
        loop {
            let byte = *self.b.get(self.i)?;
            self.i += 1;
            n |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                return Some(n);
            }
            shift += 7;
            if shift > 56 {
                return None; // no real array needs more than 8 length bytes
            }
        }
    }
    fn u64_array(&mut self) -> Option<Vec<f64>> {
        let n = self.varint()?;
        if n > (self.b.len() - self.i) / 8 {
            return None; // more elements than the remaining bytes can hold
        }
        (0..n).map(|_| self.u64().map(|x| x as f64)).collect()
    }
    /// After reading rows the body must end exactly on a row boundary. Leftover
    /// bytes mean the response was truncated or ClickHouse appended an
    /// exception midway (it can start with 200 and fail later).
    fn finish(&self, what: &str) -> Result<()> {
        // An exception appended at the end can be exactly as long as one or more
        // rows and be read as data, so look for its text at the end of the body
        let tail = &self.b[self.b.len().saturating_sub(4096)..];
        if let Some(k) = tail.windows(15).position(|w| w == b"DB::Exception: ") {
            let from = tail[..k].windows(6).rposition(|w| w == b"Code: ").unwrap_or(k);
            bail!(
                "ClickHouse ({what}): {}",
                String::from_utf8_lossy(&tail[from..]).trim().chars().take(400).collect::<String>()
            );
        }
        if self.i == self.b.len() {
            return Ok(());
        }
        let rest = String::from_utf8_lossy(&self.b[self.i..]);
        match rest.find("Code: ").or_else(|| rest.find("DB::Exception")) {
            Some(k) => Err(crate::ch::Unavailable(format!("{what}: {}", rest[k..].trim().chars().take(400).collect::<String>())).into()),
            None => Err(crate::ch::Unavailable(format!(
                "{what}: incomplete response, {} trailing bytes do not form a full row",
                self.b.len() - self.i
            ))
            .into()),
        }
    }
}

fn parse_map(v: &serde_json::Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect())
        .unwrap_or_default()
}

impl Store {
    pub fn new(ch: ClickHouse, db: String) -> Self {
        Self {
            ch,
            db,
            catalog: Mutex::new(None),
            series_cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn catalog(&self) -> Result<Arc<Catalog>> {
        self.catalog_fresh(Duration::from_secs(300)).await
    }

    /// The catalog if younger than `max_age`; an empty one (fresh install) is
    /// only kept for 15 s, so new data shows up without waiting five minutes
    async fn catalog_fresh(&self, max_age: Duration) -> Result<Arc<Catalog>> {
        let mut guard = self.catalog.lock().await;
        if let Some((at, c)) = guard.as_ref() {
            let ttl = if c.metrics.is_empty() {
                max_age.min(Duration::from_secs(15))
            } else {
                max_age
            };
            if at.elapsed() < ttl {
                return Ok(c.clone());
            }
        }
        let db = &self.db;
        let rows = self
            .ch
            .rows(
                &format!(
                    "SELECT s.Metric AS m, s.dist AS dist, toString(d.Kind) AS kind
                     FROM (SELECT Metric, max(length(Bounds)) > 0 AS dist FROM {db}.series GROUP BY Metric) AS s
                     LEFT JOIN (SELECT Metric, argMax(Kind, Updated) AS Kind FROM {db}.metric_descriptors GROUP BY Metric) AS d
                     USING Metric"
                ),
                &[],
            )
            .await?;
        let metrics = rows
            .iter()
            .filter_map(|r| {
                let m = r["m"].as_str()?.to_owned();
                let kind = if r["dist"].as_u64() == Some(1) || r["kind"] == "distribution" {
                    Kind::Distribution
                } else {
                    match r["kind"].as_str() {
                        Some("cumulative") => Kind::Counter,
                        Some("gauge") | Some("delta") => Kind::Gauge,
                        _ => guess_kind(&m),
                    }
                };
                Some((m, kind))
            })
            .collect();
        let c = Arc::new(Catalog { metrics });
        *guard = Some((Instant::now(), c.clone()));
        Ok(c)
    }

    /// Series matching a selector that have data since `from`
    pub async fn select(&self, name: &Option<String>, ms: &Matchers, from: i64) -> Result<Vec<SeriesRef>> {
        let ms = all_matchers(name, ms);
        let mut catalog = self.catalog().await?;
        // A metric the catalog does not know yet (just created): reload it if
        // older than 30 s instead of waiting for the five minutes to expire
        if let Some(n) = name
            && !catalog.virtual_names().any(|(v, ..)| v == *n)
        {
            catalog = self.catalog_fresh(Duration::from_secs(30)).await?;
        }
        let mut wanted: HashMap<&str, Vec<Wanted>> = HashMap::new();
        for (vname, metric, kind, field) in catalog.virtual_names() {
            if name_matchers_ok(&ms, &vname) {
                wanted.entry(metric).or_default().push((vname, kind, field));
            }
        }
        if wanted.is_empty() {
            return Ok(vec![]);
        }
        let db = &self.db;
        // Round to the hour so the cache key does not change on every query
        let from = from - from.rem_euclid(3600);
        let mut params: Vec<Param> = vec![("from".into(), from.to_string())];
        // With a name, filter by metric (the table's sort order); without one
        // ({job="x"}), by service or host, filtered in ClickHouse
        let filter = if wanted.len() <= 500 {
            let metrics: Vec<&str> = wanted.keys().copied().collect();
            params.push(("metrics".into(), crate::ch::array_param(&metrics)));
            "Metric IN {metrics:Array(String)}".to_owned()
        } else {
            let eq = |label: &str| {
                ms.matchers
                    .iter()
                    .find(|m| m.name == label && matches!(m.op, MatchOp::Equal))
                    .map(|m| m.value.clone())
            };
            let mut f = Vec::new();
            if let Some(job) = eq("job") {
                params.push(("svc".into(), job));
                f.push("Service = {svc:String}");
            }
            if let Some(host) = eq("instance") {
                params.push(("host".into(), host));
                f.push("Resource['host.name'] = {host:String}");
            }
            if f.is_empty() {
                bail!("the selector spans {} metrics; narrow it by name, job or instance", wanted.len());
            }
            f.join(" AND ")
        };
        // LastSeen is refreshed hourly (collector) or on import: 1 h margin
        let sql = format!(
            "SELECT SeriesId AS id, Metric AS m, Service AS s, Resource AS r, Labels AS l, Bounds AS b
             FROM {db}.series FINAL
             WHERE {filter} AND LastSeen >= toDateTime({{from:Int64}}) - 3600"
        );
        let key = format!("{sql}|{params:?}");
        let cached = self
            .series_cache
            .lock()
            .await
            .get(&key)
            .filter(|(t, _)| t.elapsed() < SERIES_CACHE_TTL)
            .map(|(_, r)| r.clone());
        let rows = match cached {
            Some(r) => r,
            None => {
                let r = Arc::new(self.ch.rows(&sql, &params).await?);
                let mut c = self.series_cache.lock().await;
                if c.len() >= SERIES_CACHE_MAX {
                    c.retain(|_, (t, _)| t.elapsed() < SERIES_CACHE_TTL);
                    if c.len() >= SERIES_CACHE_MAX {
                        c.clear();
                    }
                }
                c.insert(key, (Instant::now(), r.clone()));
                r
            }
        };
        let mut out = Vec::new();
        for r in rows.iter() {
            let (Some(id), Some(metric)) = (r["id"].as_u64(), r["m"].as_str()) else {
                continue;
            };
            let service = r["s"].as_str().unwrap_or_default();
            let resource = parse_map(&r["r"]);
            let labels = parse_map(&r["l"]);
            let Some(cands) = wanted.get(metric) else { continue };
            for (vname, kind, field) in cands {
                match field {
                    Some(f) => {
                        let l = label_view(vname, service, &resource, &labels);
                        if matches(&ms, &l) {
                            out.push(SeriesRef {
                                labels: l,
                                id,
                                metric: metric.into(),
                                kind: *kind,
                                field: *f,
                            });
                        }
                    }
                    None => {
                        // One bucket per bound, plus +Inf
                        let bounds: Vec<f64> = r["b"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
                        for (i, b) in bounds.iter().copied().chain([f64::INFINITY]).enumerate() {
                            let mut l = label_view(vname, service, &resource, &labels);
                            l.insert("le".into(), fmt_le(b));
                            if matches(&ms, &l) {
                                out.push(SeriesRef {
                                    labels: l,
                                    id,
                                    metric: metric.into(),
                                    kind: *kind,
                                    field: Field::Bucket(i),
                                });
                            }
                        }
                    }
                }
            }
            if out.len() > MAX_SERIES {
                bail!("the selector returns more than {MAX_SERIES} series; narrow it");
            }
        }
        Ok(out)
    }

    /// Samples of the series in (from, to]. In aggregated tiers each bucket
    /// yields one sample at the time of its last sample: the maximum for
    /// counters and the mean otherwise. With `group` (seconds, a multiple of
    /// the tier resolution) ClickHouse keeps the last sample of each group, like
    /// Cloud Monitoring alignment: a year at 10 minutes is not transferred in
    /// full when the chart has one point every half day.
    pub async fn samples(&self, series: Vec<SeriesRef>, from: i64, to: i64, tier: Tier, group: i64) -> Result<Vec<Samples>> {
        if series.is_empty() {
            return Ok(vec![]);
        }
        let mut ids: Vec<u64> = series.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        let mut metrics: Vec<&str> = series.iter().map(|s| s.metric.as_str()).collect();
        metrics.sort_unstable();
        metrics.dedup();
        let has_dist = series.iter().any(|s| s.kind == Kind::Distribution);
        let has_scalar = series.iter().any(|s| s.kind != Kind::Distribution);
        let db = &self.db;
        let res = tier.res();
        let sfx = tier.suffix();
        let params: Vec<Param> = vec![
            ("metrics".into(), crate::ch::array_param(&metrics)),
            ("ids".into(), serde_json::to_string(&ids)?),
            ("from".into(), (from - if tier == Tier::Raw { 0 } else { res }).to_string()),
            ("to".into(), to.to_string()),
        ];
        let filter = "Metric IN {metrics:Array(String)} AND SeriesId IN {ids:Array(UInt64)}
                      AND Time > toDateTime({from:Int64}) AND Time <= toDateTime({to:Int64})";

        // id → (t, gauge value, counter value, bucket minimum)
        let mut scalar: HashMap<u64, Vec<(i64, f64, f64, f64)>> = HashMap::new();
        // id → (t, count, sum, buckets)
        let mut dist: HashMap<u64, Vec<DistRow>> = HashMap::new();

        // One row per sample: (SeriesId, t, gauge value, counter value, bucket minimum)
        let scalar_rows = if tier == Tier::Raw {
            format!("SELECT SeriesId, toUnixTimestamp(Time) AS t, Value AS g, Value AS c, Value AS mn FROM {db}.points WHERE {filter}")
        } else {
            // GROUP BY on the sort key prefix (Metric, SeriesId, Time): ClickHouse
            // aggregates in streaming, without a hash table for the whole range
            format!(
                // The current bucket can have a LastTime past the end of the window
                // (the collector lags behind nowDelay): clamp it to `to`, or the only
                // bucket of a new series would be "in the future" and nothing would show
                "SELECT SeriesId, toUnixTimestamp(least(max(LastTime), toDateTime({{to:Int64}}))) AS t, sum(Sum) / sum(Count) AS g, max(Max) AS c, min(Min) AS mn
                 FROM {db}.points{sfx} WHERE {filter} GROUP BY Metric, SeriesId, Time"
            )
        };
        // (SeriesId, t, count, sum, buckets)
        let dist_rows = if tier == Tier::Raw {
            format!("SELECT SeriesId, toUnixTimestamp(Time) AS t, Count AS n, Sum AS s, Buckets AS b FROM {db}.dist_points WHERE {filter}")
        } else {
            format!(
                "SELECT SeriesId, t, l.1 AS n, l.2 AS s, l.3 AS b FROM (
                   SELECT SeriesId, Time, least(toUnixTimestamp(Time) + {res}, {{to:Int64}}) AS t, argMaxMerge(Last) AS l FROM {db}.dist_points{sfx}
                   WHERE {filter} GROUP BY Metric, SeriesId, Time)"
            )
        };
        // RowBinary with fixed types (UInt64, Int64, Float64, Array(UInt64)):
        // parsing text for hundreds of thousands of rows cost more than the
        // query itself
        let (scalar_sql, dist_sql) = if group > res {
            (
                format!(
                    "SELECT toUInt64(SeriesId), toInt64(max(t)) AS tt, toFloat64(argMax(g, t)), toFloat64(argMax(c, t)), toFloat64(argMax(mn, t)) FROM ({scalar_rows})
                     GROUP BY SeriesId, intDiv(t, {group}) ORDER BY SeriesId, tt FORMAT RowBinary"
                ),
                format!(
                    "SELECT toUInt64(SeriesId), toInt64(max(t)) AS tt, toUInt64(argMax(n, t)), toFloat64(argMax(s, t)), argMax(b, t) FROM ({dist_rows})
                     GROUP BY SeriesId, intDiv(t, {group}) ORDER BY SeriesId, tt FORMAT RowBinary"
                ),
            )
        } else {
            (
                format!(
                    "SELECT toUInt64(SeriesId), toInt64(t), toFloat64(g), toFloat64(c), toFloat64(mn) FROM ({scalar_rows}) ORDER BY SeriesId, t FORMAT RowBinary"
                ),
                format!("SELECT toUInt64(SeriesId), toInt64(t), toUInt64(n), toFloat64(s), b FROM ({dist_rows}) ORDER BY SeriesId, t FORMAT RowBinary"),
            )
        };
        let settings: &[(&str, &str)] = &[("optimize_aggregation_in_order", "1"), ("max_threads", "8")];
        let (a, b) = tokio::join!(
            async {
                if has_scalar {
                    self.ch.bytes(&scalar_sql, &params, settings).await.map(Some)
                } else {
                    Ok(None)
                }
            },
            async {
                if has_dist {
                    self.ch.bytes(&dist_sql, &params, settings).await.map(Some)
                } else {
                    Ok(None)
                }
            },
        );
        if let Some(body) = a? {
            let mut r = Cursor::new(&body);
            while let (Some(id), Some(t), Some(g), Some(c), Some(mn)) = (r.u64(), r.i64(), r.f64(), r.f64(), r.f64()) {
                let v = scalar.entry(id).or_default();
                // Duplicate points (e.g. an overlapping import): keep the first
                if v.last().is_some_and(|p| p.0 == t) {
                    continue;
                }
                v.push((t, g, c, mn));
            }
            r.finish("points")?;
        }
        if let Some(body) = b? {
            let mut r = Cursor::new(&body);
            while let (Some(id), Some(t), Some(c), Some(s), Some(bk)) = (r.u64(), r.i64(), r.u64(), r.f64(), r.u64_array()) {
                let v = dist.entry(id).or_default();
                if v.last().is_some_and(|p| p.0 == t) {
                    continue;
                }
                v.push((t, c as f64, s, bk));
            }
            r.finish("distributions")?;
        }

        Ok(series
            .into_iter()
            .map(|s| {
                let mut out = Samples {
                    labels: s.labels,
                    ..Default::default()
                };
                match s.field {
                    Field::Value => {
                        let pts = scalar.get(&s.id).map(Vec::as_slice).unwrap_or_default();
                        out.ts.extend(pts.iter().map(|p| p.0));
                        if s.kind == Kind::Counter {
                            out.vals = counter_levels(pts, group.max(res));
                        } else {
                            out.vals.extend(pts.iter().map(|p| p.1));
                        }
                    }
                    f => {
                        for (t, c, sum, bk) in dist.get(&s.id).map(Vec::as_slice).unwrap_or_default() {
                            let v = match f {
                                Field::Count => Some(*c),
                                Field::Sum => Some(*sum),
                                Field::Bucket(i) => bk.get(i).copied(),
                                Field::Value => None,
                            };
                            if let Some(v) = v {
                                out.ts.push(*t);
                                out.vals.push(v);
                            }
                        }
                    }
                }
                out
            })
            .collect())
    }

    /// Values of a label among the series matching the selectors
    pub async fn label_values(&self, label: &str, selectors: &[(Option<String>, Matchers)], from: i64) -> Result<Vec<String>> {
        let mut vals = std::collections::BTreeSet::new();
        if label == "__name__" && selectors.is_empty() {
            return Ok(self.catalog().await?.names());
        }
        if selectors.is_empty() {
            return self.all_label_values(label, from).await;
        }
        for (name, ms) in selectors {
            for s in self.select(name, ms, from).await? {
                if let Some(v) = s.labels.get(label) {
                    vals.insert(v.clone());
                }
            }
        }
        Ok(vals.into_iter().collect())
    }

    /// Values of a label across all series (label_values(job) in Grafana)
    async fn all_label_values(&self, label: &str, from: i64) -> Result<Vec<String>> {
        let expr = match label {
            "job" => "Service".to_owned(),
            "instance" => "Resource['host.name']".to_owned(),
            _ => "Labels[{label:String}]".to_owned(),
        };
        let db = &self.db;
        let rows = self
            .ch
            .rows(
                &format!(
                    "SELECT DISTINCT {expr} AS v FROM {db}.series
                     WHERE LastSeen >= toDateTime({{from:Int64}}) - 3600 AND v != '' ORDER BY v LIMIT 10000"
                ),
                &[("label".into(), label.into()), ("from".into(), from.to_string())],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| r["v"].as_str().map(str::to_owned)).collect())
    }

    pub async fn series(&self, selectors: &[(Option<String>, Matchers)], from: i64) -> Result<Vec<Labels>> {
        let mut out = std::collections::BTreeSet::new();
        for (name, ms) in selectors {
            for s in self.select(name, ms, from).await? {
                out.insert(s.labels);
            }
        }
        Ok(out.into_iter().collect())
    }
}

/// Counter level for each bucket. The level is the bucket maximum at the time
/// of its last sample, but if the counter reset inside the bucket (its minimum
/// drops below the previous bucket's maximum while its maximum continues it)
/// that maximum predates the reset and the rate spikes: the real level at that
/// time is, give or take one sample, the first value of the next bucket (if
/// contiguous; otherwise this bucket's first value).
fn counter_levels(pts: &[(i64, f64, f64, f64)], span: i64) -> Vec<f64> {
    let mut out: Vec<f64> = pts.iter().map(|p| p.2).collect();
    for i in 1..pts.len() {
        let (prev, cur) = (&pts[i - 1], &pts[i]);
        if cur.3 < prev.2 && cur.2 >= prev.2 {
            out[i] = match pts.get(i + 1) {
                Some(next) if next.0 - cur.0 <= 2 * span => next.3,
                _ => cur.3,
            };
        }
    }
    out
}

#[cfg(test)]
mod label_tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn resource_keys_become_labels_without_overriding_the_series_own() {
        let resource = map(&[
            ("host.name", "node-1"),
            ("nomad.service", "web-metrics"),
            ("nomad.group", "api"),
            ("nomad.alloc_index", "2"),
            // A collector before nomad.alloc_index stored the replica as `instance`,
            // which is the host in PromQL and must stay so
            ("instance", "0"),
        ]);
        let labels = map(&[("path", "/"), ("nomad_group", "from-exporter")]);
        let l = label_view("http_requests_total", "web", &resource, &labels);
        let got: Vec<(&str, &str)> = l.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            got,
            [
                ("__name__", "http_requests_total"),
                ("instance", "node-1"),
                ("job", "web"),
                ("nomad_alloc_index", "2"),
                ("nomad_group", "from-exporter"),
                ("nomad_service", "web-metrics"),
                ("path", "/"),
            ]
        );
        // Two replicas on one host are two series
        let other = map(&[("host.name", "node-1"), ("nomad.alloc_index", "3")]);
        let first = map(&[("host.name", "node-1"), ("nomad.alloc_index", "2")]);
        assert_ne!(
            label_view("up", "web", &first, &BTreeMap::new()),
            label_view("up", "web", &other, &BTreeMap::new())
        );
    }

    #[test]
    fn label_names_are_valid_promql() {
        assert_eq!(label_name("nomad.alloc_index"), "nomad_alloc_index");
        assert_eq!(label_name("container-name"), "container_name");
        assert_eq!(label_name("1st"), "_1st");
    }
}

#[cfg(test)]
mod counter_tests {
    use super::counter_levels;

    #[test]
    fn reset_inside_bucket_uses_next_minimum() {
        // (t, g, max, min): reset inside bucket 1200 (min 37, stale max 7000)
        let pts = [
            (600, 0.0, 6500.0, 6000.0),
            (1200, 0.0, 7000.0, 37.0),
            (1800, 0.0, 1012.0, 442.0),
            (2400, 0.0, 1591.0, 1022.0),
        ];
        assert_eq!(counter_levels(&pts, 600), vec![6500.0, 442.0, 1012.0, 1591.0]);
    }

    #[test]
    fn reset_between_buckets_is_untouched() {
        let pts = [(600, 0.0, 7000.0, 6500.0), (1200, 0.0, 85.0, 37.0), (1800, 0.0, 600.0, 95.0)];
        assert_eq!(counter_levels(&pts, 600), vec![7000.0, 85.0, 600.0]);
    }

    #[test]
    fn reset_inside_last_bucket_or_before_a_gap_uses_own_minimum() {
        let pts = [(600, 0.0, 6500.0, 6000.0), (1200, 0.0, 7000.0, 37.0), (6000, 0.0, 5000.0, 4400.0)];
        assert_eq!(counter_levels(&pts, 600), vec![6500.0, 37.0, 5000.0]);
        let pts = [(600, 0.0, 6500.0, 6000.0), (1200, 0.0, 7000.0, 37.0)];
        assert_eq!(counter_levels(&pts, 600), vec![6500.0, 37.0]);
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn row(id: u64, t: i64, v: f64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(id.to_le_bytes());
        b.extend(t.to_le_bytes());
        b.extend(v.to_le_bytes());
        b
    }

    fn read_all(body: &[u8]) -> Result<usize> {
        let mut r = Cursor::new(body);
        let mut n = 0;
        while let (Some(_), Some(_), Some(_)) = (r.u64(), r.i64(), r.f64()) {
            n += 1;
        }
        r.finish("test")?;
        Ok(n)
    }

    #[test]
    fn complete_body_is_accepted() {
        let mut b = row(1, 10, 1.0);
        b.extend(row(1, 20, 2.0));
        assert_eq!(read_all(&b).unwrap(), 2);
        assert_eq!(read_all(&[]).unwrap(), 0);
    }

    #[test]
    fn truncated_body_is_an_error() {
        let mut b = row(1, 10, 1.0);
        b.extend(&row(1, 20, 2.0)[..13]);
        assert!(read_all(&b).unwrap_err().to_string().contains("incomplete"));
    }

    #[test]
    fn exception_after_rows_is_an_error() {
        let mut b = row(1, 10, 1.0);
        b.extend(b"Code: 241. DB::Exception: Memory limit exceeded");
        let e = read_all(&b).unwrap_err().to_string();
        assert!(e.contains("Memory limit exceeded"), "{e}");
    }

    #[test]
    fn absurd_array_lengths_are_rejected() {
        // Length of 10 elements with only 8 bytes behind it
        let mut b = vec![10u8];
        b.extend(1u64.to_le_bytes());
        assert!(Cursor::new(&b).u64_array().is_none());
        // varint that never ends
        assert!(Cursor::new(&[0xff; 20]).varint().is_none());
    }
}
