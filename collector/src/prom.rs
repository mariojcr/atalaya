// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Prometheus text format → atalaya's model.
//!
//! - gauge and untyped → scalar points
//! - counter → cumulative scalar points (Counter = true)
//! - histogram → one distribution per label set (without `le`):
//!   count, sum and cumulative buckets with their bounds
//! - summary → quantiles as gauges (label `quantile`) and _sum/_count as
//!   cumulative
//! - _created is dropped
//! - a sample whose metric name is not `[a-zA-Z_:][a-zA-Z0-9_:]*` is dropped
//!   (and counted): names reach the server's queries and its UI

use std::collections::{BTreeMap, HashMap};

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gauge,
    Cumulative,
    Distribution,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Gauge => "gauge",
            Kind::Cumulative => "cumulative",
            Kind::Distribution => "distribution",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Descriptor {
    pub kind: Kind,
    pub unit: String,
    pub help: String,
}

#[derive(Debug, Clone)]
pub struct Scalar {
    pub metric: String,
    pub labels: Labels,
    pub value: f64,
    pub counter: bool,
}

#[derive(Debug, Clone)]
pub struct Distribution {
    pub metric: String,
    pub labels: Labels,
    pub count: u64,
    pub sum: f64,
    /// Upper bounds (without +Inf) and cumulative buckets (one more: +Inf)
    pub bounds: Vec<f64>,
    pub buckets: Vec<u64>,
}

#[derive(Debug, Default)]
pub struct Scrape {
    pub scalars: Vec<Scalar>,
    pub distributions: Vec<Distribution>,
    pub descriptors: HashMap<String, Descriptor>,
    /// Samples dropped for their metric name, and the first such name
    pub invalid_names: usize,
    pub invalid_example: String,
}

impl Scrape {
    /// Adds a sample, and its metric's descriptor the first time; a value that
    /// is not finite is skipped
    pub fn put(&mut self, metric: &str, kind: Kind, unit: &str, help: &str, labels: Labels, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.descriptors.entry(metric.to_owned()).or_insert_with(|| Descriptor {
            kind,
            unit: unit.to_owned(),
            help: help.to_owned(),
        });
        self.scalars.push(Scalar {
            metric: metric.to_owned(),
            labels,
            value,
            counter: kind == Kind::Cumulative,
        });
    }
}

/// A Prometheus metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

struct Sample {
    name: String,
    labels: Labels,
    value: f64,
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        _ => s.parse().ok(),
    }
}

/// `name{a="1",b="x\"y"} 3.5 [timestamp]`
fn parse_sample(line: &str) -> Option<Sample> {
    let (name, rest) = {
        let i = line.find(|c: char| c == '{' || c.is_whitespace())?;
        (&line[..i], &line[i..])
    };
    let mut labels = Labels::new();
    let rest = if let Some(body) = rest.strip_prefix('{') {
        let mut chars = body.char_indices().peekable();
        let end = loop {
            // spaces and commas
            while let Some(&(_, c)) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }
            match chars.peek() {
                Some(&(i, '}')) => break i,
                None => return None,
                _ => {}
            }
            let mut key = String::new();
            while let Some(&(_, c)) = chars.peek() {
                if c == '=' {
                    break;
                }
                key.push(c);
                chars.next();
            }
            chars.next(); // '='
            if chars.next().map(|(_, c)| c) != Some('"') {
                return None;
            }
            let mut val = String::new();
            loop {
                match chars.next()?.1 {
                    '\\' => match chars.next()?.1 {
                        'n' => val.push('\n'),
                        c => val.push(c),
                    },
                    '"' => break,
                    c => val.push(c),
                }
            }
            labels.insert(key.trim().to_owned(), val);
        };
        &body[end + 1..]
    } else {
        rest
    };
    let value = rest.split_whitespace().next().and_then(parse_value)?;
    Some(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

fn infer_unit(name: &str) -> &'static str {
    if name.contains("_bytes") {
        "By"
    } else if name.contains("_seconds") {
        "s"
    } else if name.ends_with("_ratio") {
        "1"
    } else {
        ""
    }
}

pub fn parse(text: &str) -> Scrape {
    let mut types: HashMap<String, String> = HashMap::new();
    let mut helps: HashMap<String, String> = HashMap::new();
    let mut units: HashMap<String, String> = HashMap::new();
    let mut samples = Vec::new();
    let (mut invalid_names, mut invalid_example) = (0, String::new());

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(meta) = line.strip_prefix('#') {
            let mut it = meta.trim_start().splitn(3, ' ');
            let (kw, name, rest) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
            match kw {
                "TYPE" => {
                    types.insert(name.to_owned(), rest.trim().to_owned());
                }
                "HELP" => {
                    helps.insert(name.to_owned(), rest.to_owned());
                }
                "UNIT" => {
                    units.insert(name.to_owned(), rest.trim().to_owned());
                }
                _ => {}
            }
            continue;
        }
        if let Some(s) = parse_sample(line) {
            if valid_name(&s.name) {
                samples.push(s);
            } else {
                if invalid_names == 0 {
                    invalid_example = s.name.chars().take(64).collect();
                }
                invalid_names += 1;
            }
        }
    }

    // Family of each sample: the name as is if it has a TYPE, otherwise without its suffix
    let family_of = |name: &str| -> (String, String) {
        if let Some(t) = types.get(name) {
            return (name.to_owned(), t.clone());
        }
        for suf in ["_bucket", "_sum", "_count", "_total", "_created"] {
            if let Some(base) = name.strip_suffix(suf)
                && let Some(t) = types.get(base)
            {
                return (base.to_owned(), t.clone());
            }
        }
        (name.to_owned(), "untyped".to_owned())
    };

    let mut out = Scrape {
        invalid_names,
        invalid_example,
        ..Scrape::default()
    };
    // Histograms: (family, labels without le) → (buckets (le, count), sum, count)
    type Hist = (Vec<(f64, f64)>, f64, f64);
    let mut hist: BTreeMap<(String, Labels), Hist> = BTreeMap::new();

    for s in samples {
        if s.name.ends_with("_created") {
            continue;
        }
        let (family, ty) = family_of(&s.name);
        let unit = units.get(&family).cloned().unwrap_or_else(|| infer_unit(&family).to_owned());
        let help = helps.get(&family).cloned().unwrap_or_default();
        match ty.as_str() {
            "histogram" => {
                let mut labels = s.labels.clone();
                let le = labels.remove("le");
                let e = hist.entry((family.clone(), labels)).or_default();
                if s.name.ends_with("_bucket") {
                    if let Some(b) = le.as_deref().and_then(parse_value) {
                        e.0.push((b, s.value));
                    }
                } else if s.name.ends_with("_sum") {
                    e.1 = s.value;
                } else if s.name.ends_with("_count") {
                    e.2 = s.value;
                }
                out.descriptors.entry(family).or_insert(Descriptor {
                    kind: Kind::Distribution,
                    unit,
                    help,
                });
            }
            "counter" => {
                out.descriptors.entry(s.name.clone()).or_insert(Descriptor {
                    kind: Kind::Cumulative,
                    unit,
                    help,
                });
                out.scalars.push(Scalar {
                    metric: s.name,
                    labels: s.labels,
                    value: s.value,
                    counter: true,
                });
            }
            "summary" => {
                let counter = s.name.ends_with("_sum") || s.name.ends_with("_count");
                let kind = if counter { Kind::Cumulative } else { Kind::Gauge };
                out.descriptors.entry(s.name.clone()).or_insert(Descriptor { kind, unit, help });
                out.scalars.push(Scalar {
                    metric: s.name,
                    labels: s.labels,
                    value: s.value,
                    counter,
                });
            }
            _ => {
                // gauge / untyped. Without a TYPE (some exporters do not publish
                // it), _total, _count, _sum and _bucket are treated as counters.
                let counter = ty == "untyped" && ["_total", "_count", "_sum", "_bucket"].iter().any(|x| s.name.ends_with(x));
                let kind = if counter { Kind::Cumulative } else { Kind::Gauge };
                out.descriptors.entry(s.name.clone()).or_insert(Descriptor { kind, unit, help });
                out.scalars.push(Scalar {
                    metric: s.name,
                    labels: s.labels,
                    value: s.value,
                    counter,
                });
            }
        }
    }

    for ((metric, labels), (mut le_counts, sum, count)) in hist {
        le_counts.sort_by(|a, b| a.0.total_cmp(&b.0));
        let bounds: Vec<f64> = le_counts.iter().filter(|(b, _)| b.is_finite()).map(|(b, _)| *b).collect();
        let mut buckets: Vec<u64> = le_counts.iter().map(|(_, c)| *c as u64).collect();
        if le_counts.last().map(|(b, _)| b.is_finite()).unwrap_or(true) {
            buckets.push(count as u64); // no explicit +Inf bucket
        }
        out.distributions.push(Distribution {
            metric,
            labels,
            count: count as u64,
            sum,
            bounds,
            buckets,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_with_an_invalid_metric_name_are_dropped_and_counted() {
        let s = parse("ok_total 1\nx'y 2\nweird.name{a=\"1\"} 3\n:colon_ok 4\n9starts_with_digit 5\n");
        let names: Vec<&str> = s.scalars.iter().map(|x| x.metric.as_str()).collect();
        assert_eq!(names, ["ok_total", ":colon_ok"]);
        assert_eq!(s.invalid_names, 3);
        assert_eq!(s.invalid_example, "x'y");
        assert!(!s.descriptors.contains_key("x'y"));
    }

    #[test]
    fn parses_families() {
        let text = r#"
# HELP node_load1 1m load average.
# TYPE node_load1 gauge
node_load1 0.69
# TYPE node_cpu_seconds_total counter
node_cpu_seconds_total{cpu="0",mode="idle"} 1234.5
node_cpu_seconds_total{cpu="0",mode="user"} 99
# TYPE http_duration_seconds histogram
http_duration_seconds_bucket{path="/a",le="0.1"} 3
http_duration_seconds_bucket{path="/a",le="1"} 5
http_duration_seconds_bucket{path="/a",le="+Inf"} 6
http_duration_seconds_sum{path="/a"} 2.5
http_duration_seconds_count{path="/a"} 6
# TYPE go_gc summary
go_gc{quantile="0.5"} 0.001
go_gc_sum 1.5
go_gc_count 30
weird{a="x\"y",b="z"} NaN
"#;
        let s = parse(text);
        assert_eq!(s.descriptors["node_load1"].kind, Kind::Gauge);
        assert_eq!(s.descriptors["node_cpu_seconds_total"].kind, Kind::Cumulative);
        assert_eq!(s.descriptors["http_duration_seconds"].kind, Kind::Distribution);
        assert_eq!(s.descriptors["go_gc_count"].kind, Kind::Cumulative);
        let d = &s.distributions[0];
        assert_eq!(d.bounds, vec![0.1, 1.0]);
        assert_eq!(d.buckets, vec![3, 5, 6]);
        assert_eq!(d.count, 6);
        assert!(s.scalars.iter().any(|x| x.metric == "weird" && x.labels["a"] == "x\"y" && x.value.is_nan()));
        assert_eq!(s.scalars.iter().filter(|x| x.counter).count(), 4);
    }
}
