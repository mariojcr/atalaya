// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! PromQL on VictoriaMetrics: the same answers as the engine over ClickHouse (Output, Labels),
//! computed by VictoriaMetrics. The atalaya-collector writes each series with the labels the engine
//! gives it (label_view), so dashboards, the home page and alert rules read the same series here.
//! The Grafana macros are expanded as the engine does: with the resolution of the data the range
//! reaches. That is the scrape interval (15 s) for recent data, and the interval VictoriaMetrics
//! downsamples to once the data is older (`with_downsampling`, the same periods as its
//! `-downsampling.period`), so a zoom into old data still finds a sample in every window.

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
    /// vmselect's -httpAuth (user, password)
    auth: Option<(String, String)>,
    /// (age, interval) in seconds, by age: data older than `age` keeps one sample per `interval`
    downsampling: Vec<(i64, i64)>,
}

/// Parses VictoriaMetrics' `-downsampling.period` (`7d:1m,42d:10m`): from each age on, one sample
/// per interval. Periods with a series filter (`{job="x"}:30d:5m`) apply only to some series, and
/// the resolution here is per query: better to refuse them than to widen every window for them
pub fn parse_downsampling(spec: &str) -> Result<Vec<(i64, i64)>> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        anyhow::ensure!(!part.contains('{'), "downsampling period with a series filter is not supported: {part}");
        let (age, every) = part.split_once(':').ok_or_else(|| anyhow!("downsampling period without ':': {part}"))?;
        let (age, every) = (duration_secs(age)?, duration_secs(every)?);
        anyhow::ensure!(every > 0, "downsampling interval must be positive: {part}");
        out.push((age, every));
    }
    out.sort();
    Ok(out)
}

/// `30s`, `5m`, `2h`, `7d`, `1w`, `1y` in seconds
fn duration_secs(s: &str) -> Result<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| anyhow!("duration without a unit: {s}"))?;
    let (n, unit) = s.split_at(split);
    let n: i64 = n.parse().map_err(|_| anyhow!("bad duration: {s}"))?;
    let k = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        "y" => 365 * 86400,
        _ => anyhow::bail!("bad duration unit: {s}"),
    };
    n.checked_mul(k).ok_or_else(|| anyhow!("duration too large: {s}"))
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
            auth: None,
            downsampling: Vec::new(),
        })
    }

    /// Basic auth on every request (vmselect's -httpAuth.username and -httpAuth.password)
    pub fn with_basic_auth(mut self, user: String, password: String) -> Self {
        self.auth = Some((user, password));
        self
    }

    /// The periods VictoriaMetrics downsamples with (`parse_downsampling`)
    pub fn with_downsampling(mut self, periods: Vec<(i64, i64)>) -> Self {
        self.downsampling = periods;
        self
    }

    /// Seconds between samples of data from `t`: the scrape interval, or the downsampling interval
    /// its age falls in
    fn resolution(&self, t: i64, now: i64) -> i64 {
        // A time from the request: saturating, so an absurd one cannot overflow
        let age = now.saturating_sub(t);
        self.downsampling
            .iter()
            .filter(|(from, _)| age > *from)
            .map(|(_, every)| *every)
            .fold(RES, i64::max)
    }

    /// GET <path> with the parameters; the `data` of a successful answer
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut req = self.http.get(format!("{}{path}", self.url)).query(params);
        if let Some((user, password)) = &self.auth {
            req = req.basic_auth(user, Some(password));
        }
        let resp = req.send().await.map_err(|e| anyhow!(Unavailable(format!("unreachable: {e}"))))?;
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
        // The oldest point decides: its data is the coarsest in the range
        let q = expand_macros(query, step, self.resolution(start, super::now()), end - start);
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
        let res = self.resolution(time, super::now());
        let q = expand_macros(query, res, res, 0);
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

    /// A VictoriaMetrics that answers only with the right basic auth, and 401 otherwise
    async fn guarded(expected: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = s.read(&mut chunk).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let ok = head.lines().any(|l| {
                        l.split_once(':')
                            .is_some_and(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.trim() == expected)
                    });
                    let body = r#"{"status":"success","data":{"resultType":"scalar","result":[1700000000,"1"]}}"#;
                    let resp = if ok {
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 12\r\nconnection: close\r\n\r\nUnauthorized".to_owned()
                    };
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn queries_carry_the_basic_auth_vmselect_asks_for() {
        let url = guarded("Basic cmVhZGVyOnMzY3JldA==").await; // reader:s3cret
        let vm = VictoriaMetrics::new(&url).unwrap().with_basic_auth("reader".into(), "s3cret".into());
        vm.instant("1", 1700000000).await.unwrap();
        let anonymous = VictoriaMetrics::new(&url).unwrap();
        let err = anonymous.instant("1", 1700000000).await.err().expect("an anonymous query has to fail");
        assert!(format!("{err:#}").contains("401"), "{err:#}");
    }

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

    #[test]
    fn the_rate_window_follows_the_resolution_of_old_data() {
        let periods = parse_downsampling("42d:10m, 7d:1m").unwrap();
        assert_eq!(periods, vec![(7 * 86400, 60), (42 * 86400, 600)]);
        let vm = VictoriaMetrics::new("http://127.0.0.1:1").unwrap().with_downsampling(periods);
        let now = 2_000_000_000;
        assert_eq!(vm.resolution(now - 3600, now), RES);
        assert_eq!(vm.resolution(now - 20 * 86400, now), 60);
        assert_eq!(vm.resolution(now - 60 * 86400, now), 600);
        assert_eq!(
            vm.resolution(i64::MIN, now),
            600,
            "the oldest possible time: the coarsest interval, no overflow"
        );
        // 6 h from 60 days ago at a 1-minute step: a window of 4 samples of 10 minutes, as the
        // engine does on its 10-minute tier (with 15 s it was 1 minute: most steps came out empty)
        let res = vm.resolution(now - 60 * 86400, now);
        assert_eq!(expand_macros("rate(x[$__rate_interval])", 60, res, 6 * 3600), "rate(x[2400s])");
        assert_eq!(VictoriaMetrics::new("http://127.0.0.1:1").unwrap().resolution(now - 60 * 86400, now), RES);
        for bad in ["7d", "7d:0m", "7x:1m", "{job=\"a\"}:30d:5m", "d:1m"] {
            assert!(parse_downsampling(bad).is_err(), "{bad}");
        }
        assert!(parse_downsampling("").unwrap().is_empty());
    }
}
