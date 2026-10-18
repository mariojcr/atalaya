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
            .find(|c| ":~<>".contains(c))
            .ok_or_else(|| anyhow::anyhow!("filter without operator: {raw}"))?;
        let (field, rest) = (&f[..pos], &f[pos..]);
        let (op, value) = if let Some(v) = rest.strip_prefix(">=") {
            (">=", v)
        } else if let Some(v) = rest.strip_prefix("<=") {
            ("<=", v)
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
pub fn values_sql(table: &str, filter: &str, col: &str) -> String {
    format!(
        "SELECT value, count() AS n FROM (SELECT {col} AS value FROM {table} WHERE {filter})
         WHERE value != '' AND ({{prefix:String}} = '' OR startsWith(lowerUTF8(value), lowerUTF8({{prefix:String}})))
         GROUP BY value ORDER BY n DESC, value LIMIT 10"
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
