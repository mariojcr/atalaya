// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! PromQL on VictoriaMetrics: the same answers as the engine over ClickHouse (Output, Labels),
//! computed by VictoriaMetrics. The atalaya-collector writes each series with the labels the engine
//! gives it (label_view), so dashboards, the home page and alert rules read the same series here.
//! The Grafana macros are expanded as the engine does on its raw tier (15 s, the scrape interval).

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{Evaluated, Labels, Output, Timing, expand_macros, store::Tier};

/// VictoriaMetrics did not answer, or answered with a server error: the API answers 502, as for
/// ClickHouse, rather than calling the query bad
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VictoriaMetrics: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// Seconds between samples of a series: the collectors' scrape interval
const RES: i64 = 15;

pub struct VictoriaMetrics {
    http: reqwest::Client,
    url: String,
}

impl VictoriaMetrics {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                // Under VictoriaMetrics' keep-alive, like the collector's sink
                .pool_idle_timeout(Duration::from_secs(5))
                .build()?,
            url: url.trim_end_matches('/').to_owned(),
        })
    }

    /// GET <path> with the parameters; the `data` of a successful answer
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.url))
            .query(params)
            .send()
            .await
            .map_err(|e| anyhow!(Unavailable(format!("unreachable: {e}"))))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| anyhow!(Unavailable(format!("reading the answer: {e}"))))?;
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(anyhow!(Unavailable(format!("{status}: {}", body.trim().chars().take(400).collect::<String>()))));
        }
        let v: Value =
            serde_json::from_str(&body).map_err(|_| anyhow!("VictoriaMetrics answered {status}: {}", body.trim().chars().take(400).collect::<String>()))?;
        if v["status"] != "success" {
            return Err(anyhow!("{}", v["error"].as_str().unwrap_or("query failed")));
        }
        Ok(v["data"].clone())
    }

    pub async fn range(&self, query: &str, start: i64, end: i64, step: i64) -> Result<Evaluated> {
        if step <= 0 {
            anyhow::bail!("step must be positive");
        }
        if end < start {
            anyhow::bail!("end is before start");
        }
        let q = expand_macros(query, step, RES, end - start);
        let t0 = std::time::Instant::now();
        let data = self
            .get(
                "/api/v1/query_range",
                &[
                    ("query", q),
                    ("start", start.to_string()),
                    ("end", end.to_string()),
                    ("step", format!("{step}s")),
                ],
            )
            .await?;
        Ok(evaluated(matrix(&data)?, t0))
    }

    pub async fn instant(&self, query: &str, time: i64) -> Result<Evaluated> {
        let q = expand_macros(query, RES, RES, 0);
        let t0 = std::time::Instant::now();
        let data = self.get("/api/v1/query", &[("query", q), ("time", time.to_string())]).await?;
        let output = match data["resultType"].as_str() {
            Some("vector") | Some("matrix") => matrix(&data)?,
            Some("scalar") => Output::Scalar(point(&data["result"]).into_iter().collect()),
            Some("string") => Output::Str(data["result"][1].as_str().unwrap_or_default().to_owned()),
            other => anyhow::bail!("unexpected result type {other:?}"),
        };
        Ok(evaluated(output, t0))
    }

    /// Values of a label in the series matching the selectors (all series without them) since `from`
    pub async fn label_values(&self, label: &str, matches: &[&str], from: i64) -> Result<Vec<String>> {
        // A label name goes in the path: only what Prometheus allows in one, so it cannot reach another endpoint
        if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || label.starts_with(|c: char| c.is_ascii_digit()) {
            anyhow::bail!("invalid label name: {label}");
        }
        let mut params: Vec<(&str, String)> = matches.iter().map(|m| ("match[]", (*m).to_owned())).collect();
        params.push(("start", from.to_string()));
        let path = format!("/api/v1/label/{}/values", urlencode(label));
        let data = self.get(&path, &params).await?;
        Ok(data
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default())
    }

    /// Label sets of the series matching the selectors since `from`
    pub async fn series(&self, matches: &[&str], from: i64) -> Result<Vec<Labels>> {
        let mut params: Vec<(&str, String)> = matches.iter().map(|m| ("match[]", (*m).to_owned())).collect();
        params.push(("start", from.to_string()));
        let data = self.get("/api/v1/series", &params).await?;
        Ok(data.as_array().map(|a| a.iter().map(labels).collect()).unwrap_or_default())
    }

    /// Metric names, for the metrics explorer
    pub async fn metric_names(&self, from: i64) -> Result<Vec<String>> {
        self.label_values("__name__", &[], from).await
    }
}

fn evaluated(output: Output, t0: std::time::Instant) -> Evaluated {
    Evaluated {
        output,
        // The data is what VictoriaMetrics keeps (raw, or downsampled with age): no tier to choose
        tier: Tier::Raw,
        timing: Timing {
            eval_ms: t0.elapsed().as_millis(),
            ..Default::default()
        },
        samples_read: 0,
    }
}

fn labels(v: &Value) -> Labels {
    v.as_object()
        .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned()))).collect())
        .unwrap_or_default()
}

/// [t, "v"] as the Prometheus API writes a sample
fn point(v: &Value) -> Option<(i64, f64)> {
    let t = v[0].as_f64()?;
    let s = v[1].as_str()?;
    let x = match s {
        "NaN" => f64::NAN,
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        _ => s.parse().ok()?,
    };
    Some((t.floor() as i64, x))
}

/// A vector or matrix answer as series with their points
fn matrix(data: &Value) -> Result<Output> {
    let result = data["result"].as_array().ok_or_else(|| anyhow!("answer without result"))?;
    let series = result
        .iter()
        .map(|s| {
            let points = match s.get("values") {
                Some(vs) => vs.as_array().map(|a| a.iter().filter_map(point).collect()).unwrap_or_default(),
                None => point(&s["value"]).into_iter().collect(),
            };
            (labels(&s["metric"]), points)
        })
        .collect();
    Ok(Output::Matrix(series))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prometheus_answers_become_the_engines_output() {
        let data = json!({"resultType": "matrix", "result": [
            {"metric": {"__name__": "up", "job": "a"}, "values": [[1700000000, "1"], [1700000015, "NaN"], [1700000030.5, "+Inf"]]},
        ]});
        let Output::Matrix(m) = matrix(&data).unwrap() else { panic!() };
        assert_eq!(
            m[0].0,
            Labels::from([("__name__".to_owned(), "up".to_owned()), ("job".to_owned(), "a".to_owned())])
        );
        assert_eq!(m[0].1[0], (1700000000, 1.0));
        assert!(m[0].1[1].1.is_nan());
        assert_eq!(m[0].1[2], (1700000030, f64::INFINITY));
        let vector = json!({"resultType": "vector", "result": [{"metric": {}, "value": [1700000000, "2.5"]}]});
        let Output::Matrix(m) = matrix(&vector).unwrap() else { panic!() };
        assert_eq!(m, vec![(Labels::new(), vec![(1700000000, 2.5)])]);
        assert_eq!(urlencode("__name__"), "__name__");
        let vm = VictoriaMetrics::new("http://127.0.0.1:1").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for bad in ["../../api/v1/admin/tsdb/delete_series", "a b", "", "9x"] {
            let e = rt.block_on(vm.label_values(bad, &[], 0)).unwrap_err();
            assert!(e.to_string().contains("invalid label name"), "{bad}: {e}");
        }
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
