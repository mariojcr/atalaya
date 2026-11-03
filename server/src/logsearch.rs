// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Discover-style log explorer: structured filters over the columns and JSON
//! attributes, present fields with their most frequent values, a histogram
//! broken down by a field, and documents surrounding a given one.
//!
//! Filters (`f=` repeated in the URL):
//!   field:value      equals           -field:value     not equal
//!   field:*          exists           -field:*         does not exist
//!   field>n  field<n  field>=n  field<=n   numeric
//!   field~text       contains (case-insensitive)   -field~text  does not contain
//!   field=~regex     matches the whole value (RE2)  -field=~regex  does not match
//! Fields: service, host, level, stream, body and any key of Attributes or
//! Resource (nomad.job, nomad.task…).

use anyhow::{Result, bail};

use atalaya::ch::Param;

/// SQL expression for a field (parameters are pushed to `params`)
pub fn field_sql(field: &str, params: &mut Vec<Param>, idx: usize) -> Result<String> {
    Ok(match field {
        "service" => "Service".into(),
        "host" => "Host".into(),
        "level" => "Level".into(),
        "stream" => "Stream".into(),
        "body" | "message" => "Body".into(),
        k => {
            if k.is_empty() || k.len() > 128 || !k.chars().all(|c| c.is_ascii_alphanumeric() || "._-/@".contains(c)) {
                bail!("invalid field: {k}");
            }
            params.push((format!("fk{idx}"), k.to_owned()));
            // The key may come from the message (Attributes) or the source (Resource: nomad.job…)
            format!("if(mapContains(Attributes, {{fk{idx}:String}}), Attributes[{{fk{idx}:String}}], Resource[{{fk{idx}:String}}])")
        }
    })
}

/// Translates the filters into parameterized SQL conditions
pub fn filters_sql(filters: &[String], params: &mut Vec<Param>) -> Result<Vec<String>> {
    let mut conds = Vec::new();
    for (i, raw) in filters.iter().enumerate() {
        let f = raw.trim();
        if f.is_empty() {
            continue;
        }
        let (neg, f) = match f.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, f),
        };
        // Operator: the first one that appears
        let pos = f
            .find(|c| ":~<>=".contains(c))
            .ok_or_else(|| anyhow::anyhow!("filter without operator: {raw}"))?;
        let (field, rest) = (&f[..pos], &f[pos..]);
        let (op, value) = if let Some(v) = rest.strip_prefix(">=") {
            (">=", v)
        } else if let Some(v) = rest.strip_prefix("<=") {
            ("<=", v)
        } else if let Some(v) = rest.strip_prefix("=~") {
            ("=~", v)
        } else {
            (&rest[..1], &rest[1..])
        };
        let col = field_sql(field, params, i)?;
        let exists = if field == "service" || field == "host" || field == "level" || field == "stream" || field == "body" || field == "message" {
            format!("{col} != ''")
        } else {
            format!("(mapContains(Attributes, {{fk{i}:String}}) OR mapContains(Resource, {{fk{i}:String}}))")
        };
        let cond = match (op, value) {
            (":", "*") => exists,
            (":", v) => {
                params.push((format!("fv{i}"), v.to_owned()));
                format!("{col} = {{fv{i}:String}}")
            }
            ("~", v) => {
                params.push((format!("fv{i}"), v.to_owned()));
                format!("positionCaseInsensitiveUTF8({col}, {{fv{i}:String}}) > 0")
            }
            // Anchored, as PromQL's =~, so a dashboard variable works the same
            // in both (ClickHouse's match is RE2: linear time)
            ("=~", v) => {
                params.push((format!("fv{i}"), format!("^(?:{v})$")));
                format!("match({col}, {{fv{i}:String}})")
            }
            (o @ (">" | "<" | ">=" | "<="), v) => {
                let n: f64 = v.parse().map_err(|_| anyhow::anyhow!("non-numeric value in {raw}"))?;
                params.push((format!("fv{i}"), n.to_string()));
                format!("toFloat64OrNull({col}) {o} {{fv{i}:Float64}}")
            }
            _ => bail!("invalid filter: {raw}"),
        };
        conds.push(if neg { format!("NOT ({cond})") } else { cond });
    }
    Ok(conds)
}

/// SQL for the present fields: document count and top 5 values of each, for
/// fixed columns and Attributes/Resource keys
pub fn fields_sql(table: &str, filter: &str) -> String {
    format!(
        "SELECT field, count() AS docs, uniq(value) AS distinct, topK(5)(value) AS top FROM (
           SELECT 'service' AS field, Service AS value FROM {table} WHERE {filter}
           UNION ALL SELECT 'host', Host FROM {table} WHERE {filter}
           UNION ALL SELECT 'level', Level FROM {table} WHERE {filter} AND Level != ''
           UNION ALL SELECT 'stream', Stream FROM {table} WHERE {filter}
           UNION ALL SELECT k, v FROM {table} ARRAY JOIN mapKeys(Attributes) AS k, mapValues(Attributes) AS v WHERE {filter}
           UNION ALL SELECT k, v FROM {table} ARRAY JOIN mapKeys(Resource) AS k, mapValues(Resource) AS v WHERE {filter}
         ) GROUP BY field ORDER BY docs DESC, field LIMIT 200"
    )
}

/// Most frequent values of a field starting with what was typed (search bar
/// autocomplete); the prefix goes in the `prefix` parameter
pub fn values_sql(table: &str, filter: &str, col: &str, limit: u32) -> String {
    format!(
        "SELECT value, count() AS n FROM (SELECT {col} AS value FROM {table} WHERE {filter})
         WHERE value != '' AND ({{prefix:String}} = '' OR startsWith(lowerUTF8(value), lowerUTF8({{prefix:String}})))
         GROUP BY value ORDER BY n DESC, value LIMIT {limit}"
    )
}

/// Histogram broken down by the 6 most frequent values of a field (the rest
/// is grouped as "…")
pub fn breakdown_sql(table: &str, filter: &str, col: &str) -> String {
    format!(
        "WITH (SELECT topK(6)({col}) FROM {table} WHERE {filter}) AS tops
         SELECT toUnixTimestamp(toStartOfInterval(Time, INTERVAL {{step:Int64}} SECOND)) AS ts,
                if(has(tops, {col}), {col}, '…') AS value, count() AS n
         FROM {table} WHERE {filter} GROUP BY ts, value ORDER BY ts"
    )
}

/// What a log query computes per group and bucket: its lines, or a function of
/// a numeric field over the lines that have one
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// The latest value (a state that lines report, like a queue's length)
    Last,
    Quantile(f64),
}

impl Agg {
    /// count, sum, avg, min, max, last, or p50/p90/p95/p99 (any p1–p99)
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "" | "count" => Agg::Count,
            "sum" => Agg::Sum,
            "avg" => Agg::Avg,
            "min" => Agg::Min,
            "max" => Agg::Max,
            "last" => Agg::Last,
            p => match p.strip_prefix('p').and_then(|n| n.parse::<u8>().ok()) {
                Some(n) if (1..=99).contains(&n) => Agg::Quantile(f64::from(n) / 100.0),
                _ => bail!("invalid agg: {s} (count, sum, avg, min, max, last, p50, p95…)"),
            },
        })
    }

    /// Whether a bucket without lines is a 0 (a count, a sum) or a gap
    pub fn zero_when_empty(self) -> bool {
        matches!(self, Agg::Count | Agg::Sum)
    }

    fn sql(self, v: &str) -> String {
        match self {
            Agg::Count => "count()".into(),
            Agg::Sum => format!("sum({v})"),
            Agg::Avg => format!("avg({v})"),
            Agg::Min => format!("min({v})"),
            Agg::Max => format!("max({v})"),
            Agg::Last => format!("argMax({v}, Time)"),
            Agg::Quantile(q) => format!("quantile({q})({v})"),
        }
    }
}

/// What `counts_sql` computes: `agg` of `value` (a field's SQL; none for a
/// count), per value of `per` and added up when it is set
#[derive(Debug, Clone, Copy)]
pub struct Measure<'a> {
    pub agg: Agg,
    pub value: Option<&'a str>,
    pub per: Option<&'a str>,
}

/// SQL for line counts grouped by key expressions (`k0`, `k1`… in the result),
/// keeping the `top` most frequent groups of the range: one count per group,
/// or one per group and bucket of `{step:Int64}` seconds (`ts`).
///
/// With another `agg`, `n` is that function of `value` (a field's SQL, read as a
/// number; lines where it is not one do not take part). With `per`, it is first
/// computed for each value of that field and then summed: the 3 replicas'
/// latest queue lengths add up, as `sum(gauge)` does in PromQL.
pub fn counts_sql(table: &str, filter: &str, keys: &[String], top: u32, bucketed: bool, m: Measure) -> String {
    let Measure { agg, value, per } = m;
    let names: Vec<String> = (0..keys.len()).map(|i| format!("k{i}")).collect();
    let mut select: Vec<String> = keys.iter().zip(&names).map(|(k, n)| format!("{k} AS {n}")).collect();
    let mut group = names.clone();
    let mut outer_group = names;
    let v = value.map(|v| format!("toFloat64OrNull({v})")).unwrap_or_default();
    let mut cond = if agg == Agg::Count {
        filter.to_owned()
    } else {
        format!("{filter} AND isNotNull({v})")
    };
    let order = if bucketed {
        select.insert(0, "toUnixTimestamp(toStartOfInterval(Time, INTERVAL {step:Int64} SECOND)) AS ts".into());
        group.insert(0, "ts".into());
        outer_group.insert(0, "ts".into());
        if !keys.is_empty() {
            let k = keys.join(", ");
            cond = format!("{cond} AND ({k}) IN (SELECT {k} FROM {table} WHERE {filter} GROUP BY {k} ORDER BY count() DESC LIMIT {top})");
        }
        " ORDER BY ts".to_owned()
    } else if keys.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY n DESC LIMIT {top}")
    };
    let group_by = |g: &[String]| if g.is_empty() { String::new() } else { format!(" GROUP BY {}", g.join(", ")) };
    match per {
        None => {
            select.push(format!("{} AS n", agg.sql(&v)));
            format!("SELECT {} FROM {table} WHERE {cond}{}{order}", select.join(", "), group_by(&group))
        }
        Some(per) => {
            select.push(format!("{per} AS per"));
            group.push("per".into());
            select.push(format!("{} AS n", agg.sql(&v)));
            let inner = format!("SELECT {} FROM {table} WHERE {cond}{}", select.join(", "), group_by(&group));
            let mut outer: Vec<String> = outer_group.clone();
            outer.push("sum(n) AS n".into());
            format!("SELECT {} FROM ({inner}){}{order}", outer.join(", "), group_by(&outer_group))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_regex_filter_matches_the_whole_value() {
        let mut params = Vec::new();
        let conds = filters_sql(&["host=~Thor|Odin".into(), "-qname=~.*\\.local\\.".into(), "url:a=~b".into()], &mut params).unwrap();
        assert_eq!(conds[0], "match(Host, {fv0:String})");
        assert!(conds[1].starts_with("NOT (match(if(mapContains(Attributes, {fk1:String})"), "{}", conds[1]);
        assert_eq!(
            conds[2],
            "if(mapContains(Attributes, {fk2:String}), Attributes[{fk2:String}], Resource[{fk2:String}]) = {fv2:String}"
        );
        let p = |k: &str| params.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(
            (p("fv0"), p("fv1"), p("fv2")),
            ("^(?:Thor|Odin)$".into(), "^(?:.*\\.local\\.)$".into(), "a=~b".into())
        );
        assert!(filters_sql(&["host=Thor".into()], &mut params).is_err(), "= alone is not an operator");
    }

    /// The lines themselves
    const COUNT: Measure = Measure {
        agg: Agg::Count,
        value: None,
        per: None,
    };

    #[test]
    fn counts_keep_the_most_frequent_groups() {
        let keys = ["Host".to_owned(), "Attributes['qname']".to_owned()];
        assert_eq!(counts_sql("logs", "F", &[], 10, false, COUNT), "SELECT count() AS n FROM logs WHERE F");
        assert_eq!(
            counts_sql("logs", "F", &keys, 5, false, COUNT),
            "SELECT Host AS k0, Attributes['qname'] AS k1, count() AS n FROM logs WHERE F GROUP BY k0, k1 ORDER BY n DESC LIMIT 5"
        );
        assert_eq!(
            counts_sql("logs", "F", &keys[..1], 5, true, COUNT),
            "SELECT toUnixTimestamp(toStartOfInterval(Time, INTERVAL {step:Int64} SECOND)) AS ts, Host AS k0, count() AS n FROM logs \
             WHERE F AND (Host) IN (SELECT Host FROM logs WHERE F GROUP BY Host ORDER BY count() DESC LIMIT 5) GROUP BY ts, k0 ORDER BY ts"
        );
    }

    #[test]
    fn a_field_can_be_summed_averaged_or_taken_last() {
        let keys = ["A".to_owned()];
        // Only the lines whose field reads as a number take part
        assert_eq!(
            counts_sql(
                "logs",
                "F",
                &keys,
                5,
                false,
                Measure {
                    agg: Agg::Sum,
                    value: Some("C"),
                    per: None
                }
            ),
            "SELECT A AS k0, sum(toFloat64OrNull(C)) AS n FROM logs WHERE F AND isNotNull(toFloat64OrNull(C)) GROUP BY k0 ORDER BY n DESC LIMIT 5"
        );
        assert_eq!(
            counts_sql(
                "logs",
                "F",
                &[],
                5,
                false,
                Measure {
                    agg: Agg::Quantile(0.95),
                    value: Some("C"),
                    per: None
                }
            ),
            "SELECT quantile(0.95)(toFloat64OrNull(C)) AS n FROM logs WHERE F AND isNotNull(toFloat64OrNull(C))"
        );
        // Per replica first, then added up, in each bucket
        assert_eq!(
            counts_sql(
                "logs",
                "F",
                &keys,
                5,
                true,
                Measure {
                    agg: Agg::Last,
                    value: Some("C"),
                    per: Some("Host")
                }
            ),
            "SELECT ts, k0, sum(n) AS n FROM (SELECT toUnixTimestamp(toStartOfInterval(Time, INTERVAL {step:Int64} SECOND)) AS ts, A AS k0, \
             Host AS per, argMax(toFloat64OrNull(C), Time) AS n FROM logs WHERE F AND isNotNull(toFloat64OrNull(C)) \
             AND (A) IN (SELECT A FROM logs WHERE F GROUP BY A ORDER BY count() DESC LIMIT 5) GROUP BY ts, k0, per) GROUP BY ts, k0 ORDER BY ts"
        );
    }

    #[test]
    fn aggs_parse() {
        assert_eq!(Agg::parse("").unwrap(), Agg::Count);
        assert_eq!(Agg::parse("p95").unwrap(), Agg::Quantile(0.95));
        assert_eq!(Agg::parse("last").unwrap(), Agg::Last);
        for bad in ["p0", "p100", "median", "sum(x)"] {
            assert!(Agg::parse(bad).is_err(), "{bad}");
        }
        assert!(Agg::Sum.zero_when_empty() && !Agg::Avg.zero_when_empty());
    }
}
