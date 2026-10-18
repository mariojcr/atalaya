// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Time series queries in the style of the Cloud Monitoring timeSeries API:
//! label filter, alignment period, per-series aligner and cross-series reducer
//! grouped by labels.
//!
//! Storage (schema/atalaya.sql): `series` stores each series' identity once;
//! `points` holds raw points (series, time, value) for 7 days; `points_1m` and
//! `points_10m` hold, per bucket, min, max, sum, count and a DDSketch for
//! percentiles. Materialized views write all three tiers on every insert, so
//! the downsampled tiers also have recent data: the coarsest one the alignment
//! period allows is used.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use atalaya::ch::{ClickHouse, Param};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeriesQuery {
    pub metric: String,
    /// Label → value. Keys: `resource.<key>`, `metric.<key>` or `service`.
    /// Value: equals; `!x` not equal; `~re` regex match; `!~re` regex non-match.
    #[serde(default)]
    pub filter: BTreeMap<String, String>,
    #[serde(default)]
    pub interval: Interval,
    #[serde(default)]
    pub aggregation: Aggregation,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Interval {
    /// Unix seconds. If unset: `range` back from now.
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// "15m", "6h", "7d"... (default 6h)
    pub range: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Aggregation {
    /// "60s", "5m", "1h" or "auto" (default: ~300 points over the range)
    pub alignment_period: Option<String>,
    /// If unset: ALIGN_RATE for counters and ALIGN_MEAN for everything else
    pub per_series_aligner: Option<Aligner>,
    #[serde(default)]
    pub cross_series_reducer: Reducer,
    #[serde(default)]
    pub group_by_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Aligner {
    AlignMean,
    AlignMin,
    AlignMax,
    AlignSum,
    AlignCount,
    AlignRate,
    AlignDelta,
    #[serde(rename = "ALIGN_PERCENTILE_50")]
    AlignPercentile50,
    #[serde(rename = "ALIGN_PERCENTILE_95")]
    AlignPercentile95,
    #[serde(rename = "ALIGN_PERCENTILE_99")]
    AlignPercentile99,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Reducer {
    #[default]
    ReduceNone,
    ReduceMean,
    ReduceSum,
    ReduceMin,
    ReduceMax,
    ReduceCount,
    #[serde(rename = "REDUCE_PERCENTILE_50")]
    ReducePercentile50,
    #[serde(rename = "REDUCE_PERCENTILE_95")]
    ReducePercentile95,
    #[serde(rename = "REDUCE_PERCENTILE_99")]
    ReducePercentile99,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Raw,
    #[serde(rename = "1m")]
    OneMinute,
    #[serde(rename = "10m")]
    TenMinutes,
}

impl Tier {
    fn table(self) -> &'static str {
        match self {
            Tier::Raw => "points",
            Tier::OneMinute => "points_1m",
            Tier::TenMinutes => "points_10m",
        }
    }
    fn time_column(self) -> &'static str {
        match self {
            Tier::Raw => "Time",
            _ => "Time",
        }
    }
    /// Tier resolution in seconds
    fn resolution(self) -> i64 {
        match self {
            Tier::Raw => 15,
            Tier::OneMinute => 60,
            Tier::TenMinutes => 600,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeries {
    pub labels: BTreeMap<String, String>,
    /// [Unix seconds, value]
    pub points: Vec<(i64, Option<f64>)>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub metric: String,
    pub kind: String,
    pub unit: String,
    pub tier: Tier,
    pub aligner: Aligner,
    pub reducer: Reducer,
    pub alignment_period_seconds: i64,
    pub start: i64,
    pub end: i64,
    pub sql: String,
    pub elapsed_ms: u128,
    pub time_series: Vec<TimeSeries>,
}

/// Retention of each tier (must match the schema TTLs)
const RAW_DAYS: i64 = 7;
const ONE_MINUTE_DAYS: i64 = 42;
const MAX_SERIES: usize = 500;

pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().map_err(|_| anyhow::anyhow!("invalid duration: {s}"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => bail!("invalid unit in {s}"),
    };
    Ok(n * mult)
}

/// "Round" periods for auto mode
const NICE_PERIODS: &[i64] = &[15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400];

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Translates `resource.x`, `metric.x` or `service` into an expression over
/// `series`, with the key as a parameter.
fn label_expr(field: &str, params: &mut Vec<Param>, idx: usize, alias: &str) -> Result<String> {
    if field == "service" {
        return Ok(format!("{alias}Service"));
    }
    let (col, key) = if let Some(k) = field.strip_prefix("resource.") {
        ("Resource", k)
    } else if let Some(k) = field.strip_prefix("metric.") {
        ("Labels", k)
    } else {
        bail!("invalid label: {field} (use resource.*, metric.* or service)");
    };
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) {
        bail!("invalid label key: {key}");
    }
    let p = format!("k{idx}");
    params.push((p.clone(), key.to_owned()));
    Ok(format!("{alias}{col}[{{{p}:String}}]"))
}

pub async fn describe(ch: &ClickHouse, db: &str, metric: &str) -> Result<(String, String)> {
    let rows = ch
        .rows(
            &format!("SELECT toString(Kind) AS kind, Unit AS unit FROM {db}.metric_descriptors FINAL WHERE Metric = {{m:String}} LIMIT 1"),
            &[("m".into(), metric.to_owned())],
        )
        .await?;
    match rows.first() {
        Some(r) => Ok((r["kind"].as_str().unwrap_or("gauge").to_owned(), r["unit"].as_str().unwrap_or("").to_owned())),
        None => bail!("unknown metric: {metric}"),
    }
}

pub async fn run(ch: &ClickHouse, db: &str, q: &TimeSeriesQuery) -> Result<QueryResult> {
    let started = std::time::Instant::now();
    let (kind, unit) = describe(ch, db, &q.metric).await?;
    let counter = kind == "cumulative";

    let end = q.interval.end.unwrap_or_else(now);
    let start = match q.interval.start {
        Some(s) => s,
        None => end - parse_duration(q.interval.range.as_deref().unwrap_or("6h"))?,
    };
    if start >= end {
        bail!("the interval is empty");
    }
    let span = end - start;

    // Finest tier still holding data at the start's age
    let age = now() - start;
    let required = if age <= RAW_DAYS * 86400 {
        Tier::Raw
    } else if age <= ONE_MINUTE_DAYS * 86400 {
        Tier::OneMinute
    } else {
        Tier::TenMinutes
    };

    let period = match q.aggregation.alignment_period.as_deref() {
        None | Some("auto") | Some("") => {
            let target = span / 300;
            *NICE_PERIODS.iter().find(|&&p| p >= target).unwrap_or(&86400)
        }
        Some(s) => parse_duration(s)?,
    }
    .max(required.resolution());

    // The coarsest tier whose resolution fits the period, but no finer than the
    // age allows.
    let tier = [Tier::TenMinutes, Tier::OneMinute, Tier::Raw]
        .into_iter()
        .find(|t| t.resolution() <= period && t.resolution() >= required.resolution())
        .unwrap_or(required);

    let aligner = q
        .aggregation
        .per_series_aligner
        .unwrap_or(if counter { Aligner::AlignRate } else { Aligner::AlignMean });
    let reducer = q.aggregation.cross_series_reducer;

    let mut params: Vec<Param> = vec![
        ("m".into(), q.metric.clone()),
        ("start".into(), start.to_string()),
        ("end".into(), end.to_string()),
        ("p".into(), period.to_string()),
    ];

    // Filter on the series table: matching ids hit the points primary key
    // (Metric, SeriesId).
    let mut conds = vec!["Metric = {m:String}".to_owned()];
    for (i, (field, raw)) in q.filter.iter().enumerate() {
        let expr = label_expr(field, &mut params, i, "")?;
        let (op, value) = if let Some(v) = raw.strip_prefix("!~") {
            ("NOT match", v)
        } else if let Some(v) = raw.strip_prefix('~') {
            ("match", v)
        } else if let Some(v) = raw.strip_prefix('!') {
            ("!=", v)
        } else {
            ("=", raw.as_str())
        };
        let vp = format!("v{i}");
        params.push((vp.clone(), value.to_owned()));
        conds.push(match op {
            "match" | "NOT match" => format!("{op}({expr}, {{{vp}:String}})"),
            _ => format!("{expr} {op} {{{vp}:String}}"),
        });
    }
    let series_filter = format!("SELECT SeriesId FROM {db}.series FINAL WHERE {}", conds.join(" AND "));

    let table = format!("{db}.{}", tier.table());
    let time = tier.time_column();
    let raw = tier == Tier::Raw;
    let percentile = |p: f64, idx: usize| {
        if raw {
            format!("quantile({p})(Value)")
        } else {
            format!("quantilesDDMerge(0.01, 0.5, 0.95, 0.99)(Sketch)[{idx}]")
        }
    };
    // Last value of a counter in the bucket: its max (counters are monotonic; a
    // reset shows up as 0 in the rate, as in Cloud Monitoring).
    let last = if raw { "max(Value)" } else { "max(Max)" };
    let agg = match aligner {
        Aligner::AlignMean => {
            if raw {
                "avg(Value)".into()
            } else {
                "sum(Sum) / sum(Count)".into()
            }
        }
        Aligner::AlignMin => {
            if raw {
                "min(Value)".into()
            } else {
                "min(Min)".into()
            }
        }
        Aligner::AlignMax => {
            if raw {
                "max(Value)".into()
            } else {
                "max(Max)".into()
            }
        }
        Aligner::AlignSum => {
            if raw {
                "sum(Value)".into()
            } else {
                "sum(Sum)".into()
            }
        }
        Aligner::AlignCount => {
            if raw {
                "toFloat64(count())".into()
            } else {
                "toFloat64(sum(Count))".into()
            }
        }
        Aligner::AlignPercentile50 => percentile(0.5, 1),
        Aligner::AlignPercentile95 => percentile(0.95, 2),
        Aligner::AlignPercentile99 => percentile(0.99, 3),
        Aligner::AlignRate | Aligner::AlignDelta => last.to_owned(),
    };
    let bucket = format!("toStartOfInterval({time}, INTERVAL {{p:Int64}} SECOND)");

    let aligned = if matches!(aligner, Aligner::AlignRate | Aligner::AlignDelta) {
        // Difference between each bucket's last value and the previous one. One
        // extra bucket is read so the first bucket also has a predecessor.
        let value = if aligner == Aligner::AlignRate {
            "greatest(last - prev, 0) / dateDiff('second', prev_t, t)"
        } else {
            "greatest(last - prev, 0)"
        };
        format!(
            "SELECT SeriesId, t, {value} AS v FROM (\n\
             \x20 SELECT SeriesId, t, last, lagInFrame(last) OVER w AS prev, lagInFrame(t) OVER w AS prev_t, row_number() OVER w AS rn\n\
             \x20 FROM (\n\
             \x20   SELECT SeriesId, {bucket} AS t, {agg} AS last FROM {table}\n\
             \x20   WHERE Metric = {{m:String}} AND {time} >= toDateTime({{start:Int64}} - {{p:Int64}}) AND {time} < toDateTime({{end:Int64}})\n\
             \x20     AND SeriesId IN ({series_filter})\n\
             \x20   GROUP BY SeriesId, t)\n\
             \x20 WINDOW w AS (PARTITION BY SeriesId ORDER BY t ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW))\n\
             WHERE rn > 1 AND t >= toDateTime({{start:Int64}})"
        )
    } else {
        format!(
            "SELECT SeriesId, {bucket} AS t, {agg} AS v FROM {table}\n\
             WHERE Metric = {{m:String}} AND {time} >= toDateTime({{start:Int64}}) AND {time} < toDateTime({{end:Int64}})\n\
             \x20 AND SeriesId IN ({series_filter})\n\
             GROUP BY SeriesId, t"
        )
    };

    let labels_join =
        format!("INNER JOIN (SELECT SeriesId, Service, Resource, Labels FROM {db}.series FINAL WHERE SeriesId IN ({series_filter})) AS s USING (SeriesId)");

    let (sql, grouped) = if reducer == Reducer::ReduceNone {
        (
            format!(
                "WITH aligned AS (\n{aligned}\n)\n\
                 SELECT toString(SeriesId) AS sid, s.Service AS service, s.Resource AS resource, s.Labels AS attrs, toUnixTimestamp(t) AS ts, v\n\
                 FROM aligned {labels_join}\n\
                 ORDER BY sid, ts"
            ),
            false,
        )
    } else {
        let red = match reducer {
            Reducer::ReduceMean => "avg(v)",
            Reducer::ReduceSum => "sum(v)",
            Reducer::ReduceMin => "min(v)",
            Reducer::ReduceMax => "max(v)",
            Reducer::ReduceCount => "toFloat64(count())",
            Reducer::ReducePercentile50 => "quantile(0.5)(v)",
            Reducer::ReducePercentile95 => "quantile(0.95)(v)",
            Reducer::ReducePercentile99 => "quantile(0.99)(v)",
            Reducer::ReduceNone => unreachable!(),
        };
        let mut gexprs = Vec::new();
        for (i, f) in q.aggregation.group_by_fields.iter().enumerate() {
            let e = label_expr(f, &mut params, 100 + i, "s.")?;
            gexprs.push(format!("{e} AS g{i}"));
        }
        let gcols: Vec<String> = (0..gexprs.len()).map(|i| format!("g{i}")).collect();
        let select_g = if gexprs.is_empty() {
            String::new()
        } else {
            format!("{}, ", gexprs.join(", "))
        };
        let group_g = if gcols.is_empty() { String::new() } else { format!("{}, ", gcols.join(", ")) };
        (
            format!(
                "WITH aligned AS (\n{aligned}\n)\n\
                 SELECT {select_g}toUnixTimestamp(t) AS ts, {red} AS v\n\
                 FROM aligned {labels_join}\n\
                 GROUP BY {group_g}ts\n\
                 ORDER BY {group_g}ts"
            ),
            true,
        )
    };

    let rows = ch.rows(&sql, &params).await?;

    // Group rows into series
    let mut out: BTreeMap<String, TimeSeries> = BTreeMap::new();
    for r in rows {
        let ts = r["ts"].as_i64().unwrap_or(0);
        let v = r["v"].as_f64().filter(|x| x.is_finite());
        let mut labels = BTreeMap::new();
        let key = if grouped {
            for (i, f) in q.aggregation.group_by_fields.iter().enumerate() {
                labels.insert(f.clone(), value_str(&r[format!("g{i}")]));
            }
            labels.values().cloned().collect::<Vec<_>>().join("\u{1f}")
        } else {
            labels.insert("service".into(), value_str(&r["service"]));
            for (ns, col) in [("resource", "resource"), ("metric", "attrs")] {
                if let Some(m) = r[col].as_object() {
                    for (k, val) in m {
                        labels.insert(format!("{ns}.{k}"), value_str(val));
                    }
                }
            }
            value_str(&r["sid"])
        };
        let entry = out.entry(key).or_insert_with(|| TimeSeries { labels, points: Vec::new() });
        entry.points.push((ts, v));
        if out.len() > MAX_SERIES {
            bail!("more than {MAX_SERIES} series: filter or group");
        }
    }

    // The SQL with parameters substituted, for display
    let mut shown = sql.clone();
    for (k, v) in params.iter().rev() {
        let quoted = format!("'{}'", v.replace('\'', "\\'"));
        for ty in ["String", "Int64"] {
            let pat = format!("{{{k}:{ty}}}");
            shown = shown.replace(&pat, if ty == "Int64" { v } else { &quoted });
        }
    }

    Ok(QueryResult {
        metric: q.metric.clone(),
        kind,
        unit,
        tier,
        aligner,
        reducer,
        alignment_period_seconds: period,
        start,
        end,
        sql: shown,
        elapsed_ms: started.elapsed().as_millis(),
        time_series: out.into_values().collect(),
    })
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
