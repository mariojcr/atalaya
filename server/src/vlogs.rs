// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Logs on VictoriaLogs: the log explorer's filters (logsearch) translated to LogsQL, and the rows
//! read back in the shape the UI gets from ClickHouse.
//!
//! The atalaya-collector writes each line with its service as `job` (the same label as the metrics),
//! `host`, `stream` and `level`, the message as `_msg`, and the keys of its resource (nomad.job,
//! nomad.task…) and attributes as they are, dotted. An attribute that clashes with one of those is
//! kept as `attr.<key>`. So a filter on `service` reads `job`, on `body` reads `_msg`, and any other
//! field reads itself.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

/// VictoriaLogs did not answer, or answered with a server error: 502 in the API
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VictoriaLogs: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// Fields of a stored line that are the line itself, not its resource or attributes
const CORE: &[&str] = &["_time", "_msg", "_stream", "_stream_id", "job", "host", "stream", "level"];
/// Resource keys (where the line comes from); the rest are attributes (what the line says)
const RESOURCE_PREFIXES: &[&str] = &["nomad.", "container.", "import.", "host."];

pub struct VictoriaLogs {
    http: reqwest::Client,
    url: String,
}

impl VictoriaLogs {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .pool_idle_timeout(Duration::from_secs(5))
                .build()?,
            url: url.trim_end_matches('/').to_owned(),
        })
    }

    /// The JSON lines a LogsQL query returns (rows, or the rows of its stats pipe)
    pub async fn query(&self, q: String) -> Result<Vec<Value>> {
        let resp = self
            .http
            .post(format!("{}/select/logsql/query", self.url))
            .form(&[("query", q.as_str())])
            .send()
            .await
            .map_err(|e| anyhow!(Unavailable(format!("unreachable: {e}"))))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| anyhow!(Unavailable(format!("reading the answer: {e}"))))?;
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(anyhow!(Unavailable(format!("{status}: {}", body.trim().chars().take(400).collect::<String>()))));
        }
        if !status.is_success() {
            bail!("{}", body.trim().chars().take(400).collect::<String>());
        }
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| anyhow!("VictoriaLogs answered a line that is not JSON: {e}")))
            .collect()
    }

    /// Lines of a filter, the newest first
    pub async fn rows(&self, filter: &str, limit: u32) -> Result<Vec<Value>> {
        Ok(self
            .query(format!("{filter} | sort by (_time desc) | limit {limit}"))
            .await?
            .iter()
            .map(row)
            .collect())
    }

    pub async fn count(&self, filter: &str) -> Result<u64> {
        let r = self.query(format!("{filter} | stats count() as n")).await?;
        Ok(r.first().map(|v| num(&v["n"]) as u64).unwrap_or(0))
    }
}

pub fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The stored field a filter or breakdown field names
pub fn field_name(field: &str) -> Result<String> {
    Ok(match field {
        "service" => "job".into(),
        "body" | "message" => "_msg".into(),
        "host" | "level" | "stream" => field.into(),
        k => {
            if k.is_empty() || k.len() > 128 || !k.chars().all(|c| c.is_ascii_alphanumeric() || "._-/@".contains(c)) {
                bail!("invalid field: {k}");
            }
            k.into()
        }
    })
}

/// The field as LogsQL writes a field name (quoted: dots, dashes…)
fn field(f: &str) -> Result<String> {
    Ok(quote(&field_name(f)?))
}

/// `_time:[start, end)` in Unix seconds
pub fn time_range(start: i64, end: i64) -> String {
    format!("_time:[{start}, {end})")
}

/// The explorer's filters (logsearch) as LogsQL: the same operators, and a regex anchored to the
/// whole value as in PromQL
pub fn filters(fs: &[String]) -> Result<String> {
    let mut out = Vec::new();
    for raw in fs {
        let f = raw.trim();
        if f.is_empty() {
            continue;
        }
        let (neg, f) = match f.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, f),
        };
        let pos = f.find(|c| ":~<>=".contains(c)).ok_or_else(|| anyhow!("filter without operator: {raw}"))?;
        let (name, rest) = (&f[..pos], &f[pos..]);
        let (op, value) = if let Some(v) = rest.strip_prefix(">=") {
            (">=", v)
        } else if let Some(v) = rest.strip_prefix("<=") {
            ("<=", v)
        } else if let Some(v) = rest.strip_prefix("=~") {
            ("=~", v)
        } else {
            (&rest[..1], &rest[1..])
        };
        let fl = field(name)?;
        let cond = match (op, value) {
            (":", "*") => format!("{fl}:*"),
            (":", v) => format!("{fl}:={}", quote(v)),
            ("~", v) => format!("{fl}:~{}", quote(&format!("(?i){}", regex_escape(v)))),
            ("=~", v) => format!("{fl}:~{}", quote(&format!("^(?:{v})$"))),
            (o @ (">" | "<" | ">=" | "<="), v) => {
                let n: f64 = v.parse().map_err(|_| anyhow!("non-numeric value in {raw}"))?;
                format!("{fl}:{o}{n}")
            }
            _ => bail!("invalid filter: {raw}"),
        };
        out.push(if neg { format!("!({cond})") } else { cond });
    }
    Ok(out.join(" "))
}

/// The explorer's fixed parameters (service, host, level list, text) as LogsQL
pub fn params_filter(service: &str, host: &str, level: &str, q: &str) -> String {
    let mut out = Vec::new();
    if !service.is_empty() {
        out.push(format!("job:={}", quote(service)));
    }
    if !host.is_empty() {
        out.push(format!("host:={}", quote(host)));
    }
    if !level.is_empty() {
        let ls: Vec<String> = level.split(',').map(|l| quote(l.trim())).collect();
        out.push(format!("level:in({})", ls.join(",")));
    }
    if !q.is_empty() {
        out.push(format!("_msg:~{}", quote(&format!("(?i){}", regex_escape(q)))));
    }
    out.join(" ")
}

/// A stats or field value as a number (VictoriaLogs writes numbers as strings)
pub fn num(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)
}

/// `_time` (RFC 3339) in Unix milliseconds
pub fn millis(v: &Value) -> i64 {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

/// A stored line in the shape of the logs table's rows: ts (ms), service, host, level, stream, body,
/// resource and attributes
pub fn row(v: &Value) -> Value {
    let mut resource = Map::new();
    let mut attributes = Map::new();
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            if CORE.contains(&k.as_str()) {
                continue;
            }
            if let Some(orig) = k.strip_prefix("attr.") {
                attributes.insert(orig.to_owned(), val.clone());
            } else if RESOURCE_PREFIXES.iter().any(|p| k.starts_with(p)) {
                resource.insert(k.clone(), val.clone());
            } else {
                attributes.insert(k.clone(), val.clone());
            }
        }
    }
    let s = |k: &str| v.get(k).cloned().unwrap_or_else(|| Value::String(String::new()));
    json!({
        "ts": millis(&v["_time"]), "service": s("job"), "host": s("host"), "level": s("level"), "stream": s("stream"),
        "body": s("_msg"), "resource": resource, "attributes": attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_filters_become_logsql() {
        let f = |s: &[&str]| filters(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap();
        assert_eq!(f(&["service:dns"]), r#""job":="dns""#);
        assert_eq!(f(&["-level:*"]), r#"!("level":*)"#);
        assert_eq!(f(&["host=~Thor|Odin"]), r#""host":~"^(?:Thor|Odin)$""#);
        assert_eq!(f(&["body~a.b"]), r#""_msg":~"(?i)a\\.b""#);
        assert_eq!(f(&["http.response.status_code>=500"]), r#""http.response.status_code":>=500"#);
        assert_eq!(f(&["nomad.job:a\"b"]), r#""nomad.job":="a\"b""#);
        assert!(filters(&["host=Thor".into()]).is_err(), "= alone is not an operator");
        assert!(filters(&["bad field:x".into()]).is_err());
        assert_eq!(
            params_filter("dns", "", "ERROR,WARN", "time out"),
            r#"job:="dns" level:in("ERROR","WARN") _msg:~"(?i)time out""#
        );
    }

    #[test]
    fn a_stored_line_reads_like_a_logs_table_row() {
        let v = json!({"_time": "2026-11-24T22:47:06.761Z", "_msg": "boom", "_stream": "{}", "job": "api", "host": "Thor", "level": "ERROR",
                       "stream": "stderr", "nomad.job": "api", "nomad.task": "web", "status": "500", "attr.level": "error"});
        let r = row(&v);
        assert_eq!(r["ts"], 1795560426761i64);
        assert_eq!(
            (r["service"].as_str(), r["host"].as_str(), r["body"].as_str()),
            (Some("api"), Some("Thor"), Some("boom"))
        );
        assert_eq!(r["resource"], json!({"nomad.job": "api", "nomad.task": "web"}));
        assert_eq!(r["attributes"], json!({"status": "500", "level": "error"}));
    }
}
