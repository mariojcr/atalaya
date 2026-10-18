// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The collector: scrapes targets, reads the host and writes series and points.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Mutex;
use xxhash_rust::xxh3::xxh3_64;

use crate::config::Config;
use crate::nomad::{AllocInfo, Nomad};
use crate::prom::{self, Descriptor, Kind, Labels, Scrape};
use crate::rules::Rules;
use crate::sink::Sink;
use crate::{host, smart};

/// Series and descriptors already sent (re-sent every hour to refresh
/// LastSeen).
pub(crate) struct Registry {
    pub(crate) series: HashMap<u64, Instant>,
    pub(crate) descriptors: HashMap<String, Instant>,
    /// Targets already warned about for invalid metric names (once per refresh period)
    invalid_names_reported: HashSet<String>,
    pruned: Instant,
}

pub(crate) const REFRESH: Duration = Duration::from_secs(3600);

/// Most a scrape may return: a larger answer fails that scrape instead of
/// filling the collector's memory
const MAX_SCRAPE: usize = 32 << 20;

/// A scrape's body, refused past `max` bytes: by its Content-Length, or as it arrives
/// The client for scrapes: a short deadline, the cluster's CA if there is one,
/// and no redirects: a target found in Nomad is scraped only at its
/// allocation's own address, and a redirect would take the scrape anywhere else
pub(crate) fn scrape_client(ca: Option<&reqwest::Certificate>) -> reqwest::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = ca {
        b = b.add_root_certificate(ca.clone());
    }
    b.build()
}

async fn read_body(mut resp: reqwest::Response, max: usize) -> anyhow::Result<String> {
    let over = || anyhow::anyhow!("response over {} MiB", max >> 20);
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(over());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(over());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            series: HashMap::new(),
            descriptors: HashMap::new(),
            invalid_names_reported: HashSet::new(),
            pruned: Instant::now(),
        }
    }

    /// Forgets series and descriptors not sent for two refresh periods. A live
    /// one is re-sent every period, so only what stopped reporting goes; without
    /// this the maps keep every series that ever existed.
    fn prune(&mut self, now: Instant) {
        if now.duration_since(self.pruned) < REFRESH {
            return;
        }
        self.pruned = now;
        self.series.retain(|_, t| now.duration_since(*t) <= 2 * REFRESH);
        self.descriptors.retain(|_, t| now.duration_since(*t) <= 2 * REFRESH);
        self.invalid_names_reported.clear();
    }
}

pub(crate) fn series_id(metric: &str, service: &str, resource: &Labels, labels: &Labels) -> u64 {
    let mut key = String::with_capacity(256);
    for part in [metric, service] {
        key.push_str(part);
        key.push('\u{1f}');
    }
    for map in [resource, labels] {
        for (k, v) in map {
            key.push_str(k);
            key.push('=');
            key.push_str(v);
            key.push('\u{1e}');
        }
        key.push('\u{1f}');
    }
    xxh3_64(key.as_bytes())
}

pub(crate) struct Collector {
    pub(crate) cfg: Config,
    pub(crate) sink: Arc<Sink>,
    /// None without the nomad section: static targets only
    pub(crate) nomad: Option<Nomad>,
    pub(crate) http: reqwest::Client,
    pub(crate) rules: Mutex<Rules>,
    pub(crate) host: Option<Arc<std::sync::Mutex<host::Host>>>,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) stats: Mutex<BTreeMap<String, f64>>,
}

/// Resource of a target discovered in Nomad: the host plus the Nomad service,
/// task group and allocation index, so the replicas and services of one job
/// stay apart (the server turns them into the PromQL labels `nomad_service`,
/// `nomad_group` and `nomad_alloc_index`)
fn nomad_resource(host: &Labels, service: &str, alloc: Option<AllocInfo>) -> Labels {
    let mut res = host.clone();
    res.insert("nomad.service".into(), service.to_owned());
    if let Some(a) = alloc {
        res.insert("nomad.group".into(), a.group);
        res.insert("nomad.alloc_index".into(), a.index);
    }
    res
}

impl Collector {
    /// Writes a scrape: new descriptors and series, and the points.
    pub(crate) async fn write(&self, service: &str, resource: &Labels, scrape: Scrape, now: i64) {
        let mut reg = self.registry.lock().await;
        let t = Instant::now();
        reg.prune(t);
        for (metric, d) in &scrape.descriptors {
            if reg.descriptors.get(metric).is_none_or(|s| t.duration_since(*s) > REFRESH) {
                reg.descriptors.insert(metric.clone(), t);
                self.push_descriptor(metric, d, now);
            }
        }
        let mut points = 0u64;
        for s in scrape.scalars {
            if !s.value.is_finite() {
                continue;
            }
            let id = series_id(&s.metric, service, resource, &s.labels);
            if reg.series.get(&id).is_none_or(|x| t.duration_since(*x) > REFRESH) {
                reg.series.insert(id, t);
                self.sink.push(
                    "series",
                    json!({"SeriesId": id, "Metric": s.metric, "Service": service, "Resource": resource, "Labels": s.labels, "Bounds": [], "LastSeen": now}),
                );
            }
            self.sink.push(
                "points",
                json!({"Metric": s.metric, "SeriesId": id, "Time": now, "Value": s.value, "Counter": s.counter}),
            );
            points += 1;
        }
        for d in scrape.distributions {
            let id = series_id(&d.metric, service, resource, &d.labels);
            if reg.series.get(&id).is_none_or(|x| t.duration_since(*x) > REFRESH) {
                reg.series.insert(id, t);
                self.sink.push(
                    "series",
                    json!({"SeriesId": id, "Metric": d.metric, "Service": service, "Resource": resource, "Labels": d.labels, "Bounds": d.bounds, "LastSeen": now}),
                );
            }
            self.sink.push(
                "dist_points",
                json!({"Metric": d.metric, "SeriesId": id, "Time": now, "Count": d.count, "Sum": d.sum, "Buckets": d.buckets}),
            );
            points += 1;
        }
        drop(reg);
        *self.stats.lock().await.entry("points".into()).or_default() += points as f64;
    }

    pub(crate) fn push_descriptor(&self, metric: &str, d: &Descriptor, now: i64) {
        self.sink.push(
            "metric_descriptors",
            json!({"Metric": metric, "Kind": d.kind.as_str(), "Unit": d.unit, "Description": d.help, "Updated": now}),
        );
    }

    pub(crate) async fn scrape_one(&self, service: String, url: String, resource: Labels, now: i64) {
        let started = Instant::now();
        let result = async {
            let resp = self.http.get(&url).send().await?;
            // Anything but a 2xx is a failed scrape, a redirect too (it is not followed)
            anyhow::ensure!(resp.status().is_success(), "HTTP {}", resp.status());
            let text = read_body(resp, MAX_SCRAPE).await?;
            anyhow::Ok(prom::parse(&text))
        }
        .await;
        let up = match result {
            Ok(s) => {
                if s.invalid_names > 0 && self.registry.lock().await.invalid_names_reported.insert(url.clone()) {
                    tracing::warn!(
                        "scraping {url}: dropped {} samples whose metric name is not valid, such as {:?}",
                        s.invalid_names,
                        s.invalid_example
                    );
                }
                let s = self.rules.lock().await.apply(&service, &url, s);
                self.write(&service, &resource, s, now).await;
                1.0
            }
            Err(e) => {
                tracing::warn!("scraping {url}: {e:#}");
                *self.stats.lock().await.entry("scrape_errors".into()).or_default() += 1.0;
                0.0
            }
        };
        // up and scrape_duration_seconds as in Prometheus: on the target's own
        // series
        let mut s = Scrape::default();
        for (metric, value) in [("up", up), ("scrape_duration_seconds", started.elapsed().as_secs_f64())] {
            s.descriptors.insert(
                metric.to_owned(),
                Descriptor {
                    kind: Kind::Gauge,
                    unit: String::new(),
                    help: String::new(),
                },
            );
            s.scalars.push(prom::Scalar {
                metric: metric.to_owned(),
                labels: Labels::new(),
                value,
                counter: false,
            });
        }
        self.write(&service, &resource, s, now).await;
    }

    /// A collector self metric, written like any other
    pub(crate) async fn self_metric(&self, metric: &str, resource: &Labels, value: f64, counter: bool, now: i64) {
        let mut s = Scrape::default();
        s.descriptors.insert(
            metric.to_owned(),
            Descriptor {
                kind: if counter { Kind::Cumulative } else { Kind::Gauge },
                unit: String::new(),
                help: String::new(),
            },
        );
        s.scalars.push(prom::Scalar {
            metric: metric.to_owned(),
            labels: Labels::new(),
            value,
            counter,
        });
        self.write("atalaya-collector", resource, s, now).await;
    }

    pub(crate) async fn scrape_all(self: &Arc<Self>) {
        let now = chrono::Utc::now().timestamp();
        let host: Labels = [("host.name".to_owned(), self.cfg.node.clone())].into();
        let mut jobs = Vec::new();
        for t in &self.cfg.scrape.static_targets {
            jobs.push((t.service.clone(), t.url.clone(), host.clone()));
        }
        if let Some(nomad) = &self.nomad {
            match nomad.targets().await {
                Ok(targets) => {
                    for t in targets {
                        let res = nomad_resource(&host, &t.service, nomad.alloc(&t.alloc).await);
                        jobs.push((t.job, t.url, res));
                    }
                }
                Err(e) => tracing::warn!("discovering Nomad services: {e:#}"),
            }
        }
        let mut handles: Vec<_> = Vec::new();
        if let Some(h) = self.host.clone() {
            let me = self.clone();
            let res = host.clone();
            handles.push(tokio::spawn(async move {
                match tokio::task::spawn_blocking(move || h.lock().unwrap().collect()).await {
                    Ok(s) => {
                        let s = me.rules.lock().await.apply(host::SERVICE, "sysfs", s);
                        me.write(host::SERVICE, &res, s, now).await;
                    }
                    Err(e) => tracing::warn!("reading sysfs: {e}"),
                }
            }));
        }
        handles.extend(jobs.into_iter().map(|(service, url, res)| {
            let me = self.clone();
            tokio::spawn(async move { me.scrape_one(service, url, res, now).await })
        }));
        for h in handles {
            let _ = h.await;
        }
    }

    /// One SMART pass over all disks, one after another
    pub(crate) async fn smart_all(&self) {
        let now = chrono::Utc::now().timestamp();
        let host: Labels = [("host.name".to_owned(), self.cfg.node.clone())].into();
        let mut s = Scrape::default();
        for d in smart::devices(&self.cfg.host.root.join("sys")) {
            match smart::run(&self.cfg.host.smart.smartctl, &d.path).await {
                Some(v) => smart::parse(&d.name, d.rotational, &v, &mut s),
                None => tracing::warn!("smartctl {}: no data", d.path),
            }
        }
        let s = self.rules.lock().await.apply(host::SERVICE, "smartctl", s);
        self.write(host::SERVICE, &host, s, now).await;
    }

    pub(crate) async fn report(&self) {
        let now = chrono::Utc::now().timestamp();
        let res: Labels = [("host.name".to_owned(), self.cfg.node.clone())].into();
        let st = &self.sink.stats;
        let local = self.stats.lock().await.clone();
        for (m, v, c) in [
            ("atalaya_collector_rows_sent_total", st.rows_sent.load(Ordering::Relaxed) as f64, true),
            ("atalaya_collector_batches_sent_total", st.batches_sent.load(Ordering::Relaxed) as f64, true),
            ("atalaya_collector_send_errors_total", st.send_errors.load(Ordering::Relaxed) as f64, true),
            (
                "atalaya_collector_spooled_batches_total",
                st.spooled_batches.load(Ordering::Relaxed) as f64,
                true,
            ),
            (
                "atalaya_collector_dropped_batches_total",
                st.dropped_batches.load(Ordering::Relaxed) as f64,
                true,
            ),
            ("atalaya_collector_spool_bytes", st.spool_bytes.load(Ordering::Relaxed) as f64, false),
            ("atalaya_collector_dropped_rows_total", st.dropped_rows.load(Ordering::Relaxed) as f64, true),
            ("atalaya_collector_points_total", local.get("points").copied().unwrap_or(0.0), true),
            ("atalaya_collector_log_lines_total", local.get("log_lines").copied().unwrap_or(0.0), true),
            ("atalaya_collector_syslog_lines_total", local.get("syslog_lines").copied().unwrap_or(0.0), true),
            (
                "atalaya_collector_log_lines_truncated_total",
                local.get("log_lines_truncated").copied().unwrap_or(0.0),
                true,
            ),
            (
                "atalaya_collector_scrape_errors_total",
                local.get("scrape_errors").copied().unwrap_or(0.0),
                true,
            ),
        ] {
            self.self_metric(m, &res, v, c, now).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An HTTP server answering every request with `body`, with or without a
    /// Content-Length (without one, the body runs until the connection closes)
    async fn serving(body: Vec<u8>, length: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(body);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let head = if length {
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len())
                    } else {
                        "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_owned()
                    };
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        format!("http://{addr}/metrics")
    }

    #[tokio::test]
    async fn a_scrape_answer_past_the_limit_fails_instead_of_filling_memory() {
        let http = reqwest::Client::new();
        let small = b"up 1\n".to_vec();
        let url = serving(small.clone(), true).await;
        assert_eq!(read_body(http.get(&url).send().await.unwrap(), 1024).await.unwrap(), "up 1\n");
        // Announced too big, or growing past the limit with no length announced
        for length in [true, false] {
            let url = serving(vec![b'#'; 4096], length).await;
            let err = read_body(http.get(&url).send().await.unwrap(), 1024).await.unwrap_err();
            assert!(err.to_string().contains("response over"), "{err}");
        }
    }

    #[tokio::test]
    async fn a_scrape_does_not_follow_a_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let elsewhere = serving(b"secret 1\n".to_vec(), true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let head = format!("HTTP/1.1 302 Found\r\nlocation: {elsewhere}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(head.as_bytes()).await;
            }
        });
        let resp = scrape_client(None).unwrap().get(format!("http://{addr}/metrics")).send().await.unwrap();
        assert_eq!(resp.status(), 302, "the redirect is the answer, not followed");
    }

    #[test]
    fn the_registry_forgets_what_stopped_reporting() {
        let start = Instant::now();
        let mut reg = Registry::new();
        reg.pruned = start;
        reg.series.insert(1, start);
        reg.descriptors.insert("gone".into(), start);
        reg.series.insert(2, start + REFRESH * 5 / 2);
        // Not due yet: nothing is forgotten before a refresh period has passed
        reg.prune(start + REFRESH / 2);
        assert_eq!(reg.series.len(), 2);
        reg.prune(start + 3 * REFRESH);
        assert!(!reg.series.contains_key(&1), "not sent for three periods");
        assert!(reg.series.contains_key(&2), "sent half a period ago");
        assert!(reg.descriptors.is_empty());
    }

    #[test]
    fn a_nomad_target_keeps_its_service_group_and_replica() {
        let host: Labels = [("host.name".to_owned(), "node-1".to_owned())].into();
        let alloc = AllocInfo {
            job: "web".into(),
            group: "api".into(),
            namespace: "default".into(),
            index: "2".into(),
            ..Default::default()
        };
        let res = nomad_resource(&host, "web-metrics", Some(alloc));
        let got: Vec<(&str, &str)> = res.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        // `instance` is the host in PromQL: the replica goes under its own name
        assert_eq!(
            got,
            [
                ("host.name", "node-1"),
                ("nomad.alloc_index", "2"),
                ("nomad.group", "api"),
                ("nomad.service", "web-metrics")
            ]
        );
    }
}
