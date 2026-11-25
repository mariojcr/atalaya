// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Sending: to ClickHouse, to VictoriaMetrics and VictoriaLogs, or to both at once. Each destination
//! gets one gzip-compressed batch per table every flush interval; if it does not respond, the batch
//! goes to that destination's disk spool, retried in order, and when the spool is full the oldest
//! batches are dropped. Staleness markers are a ClickHouse convenience: a batch of them that cannot
//! be sent is dropped, never spooled, so they cannot hold up points and logs (a schema without their
//! table only loses the markers).
//!
//! Rows are pushed in ClickHouse's format (series registered once, then points by SeriesId). For
//! Victoria they are translated as they come in: each point carries the labels the server's PromQL
//! engine gives that series (`label_view`: job is the service, instance the host, the rest of the
//! resource with `_` for `.`, exporter labels that clash as `exported_*`), so dashboards and rules
//! read the same series from either store, and a distribution becomes `_bucket{le}`, `_sum` and
//! `_count`. A log line keeps its service as `job` (the same label as the metrics), its host, stream
//! and level, and its resource and attribute keys as they are, dotted, as VictoriaLogs flattens JSON.
//!
//! With two destinations a row is durable (and the log tailers may save their positions past it)
//! only once both have it, sent or on disk.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Map, Value, json};

pub const TABLES: &[&str] = &["metric_descriptors", "series", "points", "dist_points", "logs"];
/// The same, when ClickHouse takes only metrics or only logs (`Sink::clickhouse_signals`)
const CH_METRIC_TABLES: &[&str] = &["metric_descriptors", "series", "points", "dist_points"];
const CH_LOG_TABLES: &[&str] = &["logs"];
/// Victoria's batches: points for VictoriaMetrics' /api/v1/import and lines for VictoriaLogs'
/// /insert/jsonline, each in its own pipe and spool, so one being down does not hold up the other
const VM_TABLES: &[&str] = &["vm"];
const VL_TABLES: &[&str] = &["vl"];
/// The table of staleness markers (best effort, ClickHouse only)
const STALE: &str = "stale";

/// VictoriaLogs streams: where a line comes from (a task of a job on a host)
pub const LOG_STREAM_FIELDS: &str = "job,host,nomad.group,nomad.task,stream";
/// Fields of a log line that its resource or attributes cannot overwrite
const LOG_CORE: &[&str] = &["_time", "_msg", "_stream", "job", "host", "stream", "level"];

/// How long a connection may sit idle in the pool and still be reused: well under the server's
/// keep-alive (ClickHouse's `keep_alive_timeout`, 30 s by default), or reusing one just as the
/// server closes it fails the send
const POOL_IDLE: Duration = Duration::from_secs(5);

/// Cap on in-memory rows per table between two sends. Normally there are a few
/// thousand; if sending gets stuck, the oldest are dropped (counted in
/// dropped_rows) rather than exhausting the node's memory.
const MAX_PENDING_ROWS: usize = 500_000;

/// Spooled batches sent per flush at most (see `retry_spool`)
const SPOOL_PER_FLUSH: usize = 32;

/// A series' labels are forgotten once nothing has been written to it for this long. The collector
/// registers a live series again every hour (`collector::REFRESH`), and every point touches it
const SERIES_IDLE: Duration = Duration::from_secs(3 * 3600);

#[derive(Default)]
pub struct Stats {
    pub rows_sent: AtomicU64,
    pub batches_sent: AtomicU64,
    pub send_errors: AtomicU64,
    pub spooled_batches: AtomicU64,
    pub dropped_batches: AtomicU64,
    pub spool_bytes: AtomicU64,
    pub dropped_rows: AtomicU64,
}

enum Backend {
    ClickHouse {
        url: String,
        user: String,
        password: String,
        database: String,
    },
    Victoria {
        metrics_url: Option<String>,
        logs_url: Option<String>,
        auth: Option<(String, String)>,
    },
}

/// Failure sending a batch: if the destination rejects it for its content,
/// retrying will not fix it
enum SendError {
    Rejected(String),
    Retry(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Rejected(m) => write!(f, "rejected: {m}"),
            SendError::Retry(e) => write!(f, "{e:#}"),
        }
    }
}

fn gzip(lines: &[String]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    for l in lines {
        enc.write_all(l.as_bytes())?;
        enc.write_all(b"\n")?;
    }
    Ok(enc.finish()?)
}

/// One destination: its pending rows per table, its disk spool and its stats
struct Pipe {
    http: reqwest::Client,
    backend: Backend,
    tables: &'static [&'static str],
    spool: PathBuf,
    max_spool_bytes: u64,
    pending: Mutex<Vec<Vec<String>>>,
    stats: Arc<Stats>,
}

impl Pipe {
    fn new(backend: Backend, tables: &'static [&'static str], spool: PathBuf, max_spool_bytes: u64, pool_idle: Duration, stats: Arc<Stats>) -> Result<Self> {
        std::fs::create_dir_all(&spool).with_context(|| format!("creating {}", spool.display()))?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .pool_idle_timeout(pool_idle)
                .build()?,
            backend,
            tables,
            spool,
            max_spool_bytes,
            pending: Mutex::new(vec![Vec::new(); tables.len()]),
            stats,
        })
    }

    fn name(&self) -> &'static str {
        match self.backend {
            Backend::ClickHouse { .. } => "ClickHouse",
            Backend::Victoria { .. } => "Victoria",
        }
    }

    fn has(&self, table: &str) -> bool {
        self.tables.contains(&table)
    }

    fn pending_rows(&self, table: &str) -> usize {
        self.tables
            .iter()
            .position(|t| *t == table)
            .map(|i| self.pending.lock().unwrap()[i].len())
            .unwrap_or(0)
    }

    fn push_line(&self, table: &str, line: String) {
        let i = self.tables.iter().position(|t| *t == table).expect("unknown table");
        let mut p = self.pending.lock().unwrap();
        let rows = &mut p[i];
        if rows.len() >= MAX_PENDING_ROWS {
            // Drop a tenth at once so the vector is not shifted on every row
            let n = MAX_PENDING_ROWS / 10;
            rows.drain(..n);
            self.stats.dropped_rows.fetch_add(n as u64, Ordering::Relaxed);
        }
        rows.push(line);
    }

    async fn send(&self, table: &str, body: Vec<u8>) -> std::result::Result<(), SendError> {
        let req = match &self.backend {
            Backend::ClickHouse { url, user, password, database } => {
                let query = format!("INSERT INTO {database}.{table} FORMAT JSONEachRow");
                self.http
                    .post(url)
                    .basic_auth(user, Some(password))
                    // No async insert: the collector already sends large batches, and
                    // waiting for the server-side flush (up to 5 s per insert) slows it down.
                    .query(&[("query", query.as_str()), ("input_format_skip_unknown_fields", "1"), ("async_insert", "0")])
            }
            Backend::Victoria { metrics_url, logs_url, auth } => {
                let req = match (table, metrics_url, logs_url) {
                    ("vm", Some(u), _) => self.http.post(format!("{}/api/v1/import", u.trim_end_matches('/'))),
                    ("vl", _, Some(u)) => self.http.post(format!("{}/insert/jsonline", u.trim_end_matches('/'))).query(&[
                        ("_stream_fields", LOG_STREAM_FIELDS),
                        ("_time_field", "_time"),
                        ("_msg_field", "_msg"),
                    ]),
                    // A spooled batch for a destination no longer configured: it can never be sent
                    _ => return Err(SendError::Rejected(format!("no destination configured for {table}"))),
                };
                match auth {
                    Some((user, password)) => req.basic_auth(user, Some(password)),
                    None => req,
                }
            }
        };
        let resp = req
            .header("Content-Encoding", "gzip")
            .body(body)
            .send()
            .await
            .map_err(|e| SendError::Retry(e.into()))?;
        let status = resp.status();
        if !status.is_success() {
            let msg = format!("{status}: {}", resp.text().await.unwrap_or_default().trim());
            // A 4xx means the batch is bad (format, types), except a timeout, a rate
            // limit, wrong credentials (401/403) or a missing database or table (404):
            // those are fixed on the server, so the batch waits in the spool instead
            // of being dropped
            return Err(if status.is_client_error() && !matches!(status.as_u16(), 401 | 403 | 404 | 408 | 429) {
                SendError::Rejected(msg)
            } else {
                SendError::Retry(anyhow::anyhow!(msg))
            });
        }
        Ok(())
    }

    /// Sends what is pending; whatever fails goes to the disk spool. True when every row it took
    /// reached the destination or the disk; rows that reached neither go back to the front of the
    /// queue for the next flush.
    async fn flush(&self) -> bool {
        let batches: Vec<Vec<String>> = {
            let mut p = self.pending.lock().unwrap();
            p.iter_mut().map(std::mem::take).collect()
        };
        // What was waiting on disk goes first, to keep the order
        let spool_ok = self.retry_spool().await;
        let sends = batches.into_iter().enumerate().filter(|(_, l)| !l.is_empty()).map(|(i, lines)| async move {
            let table = self.tables[i];
            let n = lines.len() as u64;
            let body = match gzip(&lines) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("compressing {table}: {e:#}");
                    return Some((i, lines));
                }
            };
            if spool_ok {
                match self.send(table, body.clone()).await {
                    Ok(()) => {
                        self.stats.rows_sent.fetch_add(n, Ordering::Relaxed);
                        self.stats.batches_sent.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    Err(SendError::Rejected(m)) => {
                        self.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
                        tracing::error!("{} rejected {n} rows of {table}: {m}", self.name());
                        return None;
                    }
                    Err(e) => {
                        self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("sending {n} rows of {table} to {}: {e}; spooling to disk", self.name());
                    }
                }
            }
            if self.to_spool(table, &body) { None } else { Some((i, lines)) }
        });
        // Counting this flush as done would let the log tailers save positions past lines that were
        // never stored
        let unsaved: Vec<(usize, Vec<String>)> = futures::future::join_all(sends).await.into_iter().flatten().collect();
        self.trim_spool();
        if unsaved.is_empty() {
            return true;
        }
        let mut p = self.pending.lock().unwrap();
        for (i, mut lines) in unsaved {
            tracing::warn!(
                "{} rows of {} for {} kept in memory for the next flush",
                lines.len(),
                self.tables[i],
                self.name()
            );
            lines.append(&mut p[i]);
            p[i] = lines;
        }
        false
    }

    fn to_spool(&self, table: &str, body: &[u8]) -> bool {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = self.spool.join(format!("{nanos:024}-{table}.json.gz"));
        // Write to a temp file and rename: an interruption halfway does not leave a
        // partial batch under the final name (spool_files only looks at .gz)
        let tmp = path.with_extension("gz.tmp");
        match std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &path)) {
            Ok(()) => {
                self.stats.spooled_batches.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(e) => {
                tracing::error!("writing {}: {e}", path.display());
                let _ = std::fs::remove_file(&tmp);
                false
            }
        }
    }

    fn spool_files(&self) -> Vec<(PathBuf, u64)> {
        let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(&self.spool)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "gz"))
                    .map(|e| (e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// Sends the oldest batches of the disk spool, at most SPOOL_PER_FLUSH of them. True when the
    /// spool is empty afterwards: otherwise this flush's batches go to the spool behind the rest, so
    /// the order holds, and a long backlog drains a slice per flush
    async fn retry_spool(&self) -> bool {
        let files = self.spool_files();
        let all = files.len();
        for (path, _) in files.into_iter().take(SPOOL_PER_FLUSH) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
            let Some(table) = name.strip_suffix(".json.gz").and_then(|n| n.split_once('-')).map(|(_, t)| t.to_owned()) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let Ok(body) = std::fs::read(&path) else { continue };
            match self.send(&table, body).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(&path);
                    self.stats.batches_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(SendError::Rejected(m)) => {
                    // Do not block the rest: move it aside for inspection
                    let bad = self.spool.join("rejected");
                    let _ = std::fs::create_dir_all(&bad);
                    let _ = std::fs::rename(&path, bad.join(&name));
                    self.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("{} rejected batch {name} ({m}); moved to {}", self.name(), bad.display());
                }
                Err(e) => {
                    self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("disk spool for {} still cannot be sent: {e}", self.name());
                    return false;
                }
            }
        }
        all <= SPOOL_PER_FLUSH
    }

    fn trim_spool(&self) {
        let files = self.spool_files();
        let mut total: u64 = files.iter().map(|(_, s)| s).sum();
        for (path, size) in &files {
            if total <= self.max_spool_bytes {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= size;
                self.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
                tracing::error!("disk spool full: dropped {}", path.display());
            }
        }
        self.stats.spool_bytes.store(total, Ordering::Relaxed);
    }
}

async fn flush_pipe(p: Option<&Pipe>) -> bool {
    match p {
        Some(p) => p.flush().await,
        None => true,
    }
}

/// What Victoria needs of a registered series to label its points
struct VictoriaSeries {
    metric: String,
    /// The PromQL labels of the series, without `__name__`
    labels: Map<String, Value>,
    /// Bucket upper bounds (distributions)
    bounds: Vec<f64>,
    seen: Instant,
}

/// The Victoria destination: rows translated as they are pushed, points to VictoriaMetrics and log
/// lines to VictoriaLogs, each with its own pipe
struct Victoria {
    metrics: Option<Pipe>,
    logs: Option<Pipe>,
    series: Mutex<HashMap<u64, VictoriaSeries>>,
}

/// A resource key as a PromQL label name, as the server's `label_name`: anything outside
/// [A-Za-z0-9_] becomes `_`, and a leading digit gets a `_` in front
fn label_name(key: &str) -> String {
    let mut name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

fn non_empty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// The PromQL labels of a series as the server's `label_view` builds them, without `__name__`:
/// exporter labels that clash with job or instance become exported_*, job is the service, instance
/// the host, and every other resource key a label with its dots as underscores, unless the series
/// already has that label
fn label_view(service: &str, resource: &Map<String, Value>, labels: &Map<String, Value>) -> Map<String, Value> {
    let mut l: Map<String, Value> = labels.iter().filter_map(|(k, v)| non_empty(v).map(|s| (k.clone(), Value::from(s)))).collect();
    for k in ["job", "instance"] {
        if let Some(v) = l.remove(k) {
            l.insert(format!("exported_{k}"), v);
        }
    }
    if !service.is_empty() {
        l.insert("job".into(), service.into());
    }
    if let Some(h) = resource.get("host.name").and_then(non_empty) {
        l.insert("instance".into(), h.into());
    }
    // serde_json's map is sorted, like the server's BTreeMap: the same key wins a clash
    for (k, v) in resource {
        if let Some(s) = non_empty(v).filter(|_| k != "host.name") {
            l.entry(label_name(k)).or_insert_with(|| s.into());
        }
    }
    l
}

/// A bucket bound as the server writes `le` (Rust's `{}` for f64, +Inf for the last bucket)
fn fmt_le(b: f64) -> String {
    if b == f64::INFINITY { "+Inf".into() } else { format!("{b}") }
}

/// A row's `Time` ("2026-11-24 22:47:06.761", UTC) as RFC 3339
fn rfc3339(t: &str) -> String {
    if t.contains('T') {
        return t.to_owned();
    }
    format!("{}Z", t.replacen(' ', "T", 1))
}

fn import_line(labels: &Map<String, Value>, name: &str, extra: Option<(&str, String)>, value: f64, time: i64) -> String {
    let mut metric = labels.clone();
    metric.insert("__name__".into(), name.into());
    if let Some((k, v)) = extra {
        metric.insert(k.into(), v.into());
    }
    json!({"metric": metric, "values": [value], "timestamps": [time * 1000]}).to_string()
}

fn log_line(row: &Value) -> String {
    let mut o = Map::new();
    o.insert("_time".into(), rfc3339(row["Time"].as_str().unwrap_or_default()).into());
    o.insert("_msg".into(), row["Body"].clone());
    for (k, col) in [("job", "Service"), ("host", "Host"), ("stream", "Stream"), ("level", "Level")] {
        if let Some(s) = non_empty(&row[col]) {
            o.insert(k.into(), s.into());
        }
    }
    let empty = Map::new();
    let resource = row["Resource"].as_object().unwrap_or(&empty);
    for (k, v) in resource {
        if let Some(s) = non_empty(v).filter(|_| !LOG_CORE.contains(&k.as_str())) {
            o.entry(k.clone()).or_insert_with(|| s.into());
        }
    }
    // An attribute named like a core field or a resource key keeps its value under attr.<key>
    for (k, v) in row["Attributes"].as_object().unwrap_or(&empty) {
        let Some(s) = non_empty(v) else { continue };
        let key = if LOG_CORE.contains(&k.as_str()) || resource.contains_key(k) {
            format!("attr.{k}")
        } else {
            k.clone()
        };
        o.entry(key).or_insert_with(|| s.into());
    }
    Value::Object(o).to_string()
}

impl Victoria {
    fn push(&self, table: &str, row: &Value) {
        match table {
            "series" if self.metrics.is_some() => {
                let Some(id) = row["SeriesId"].as_u64() else { return };
                let empty = Map::new();
                let labels = label_view(
                    row["Service"].as_str().unwrap_or_default(),
                    row["Resource"].as_object().unwrap_or(&empty),
                    row["Labels"].as_object().unwrap_or(&empty),
                );
                let bounds = row["Bounds"]
                    .as_array()
                    .map(|b| b.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default();
                let metric = row["Metric"].as_str().unwrap_or_default().to_owned();
                self.series.lock().unwrap().insert(
                    id,
                    VictoriaSeries {
                        metric,
                        labels,
                        bounds,
                        seen: Instant::now(),
                    },
                );
            }
            "points" | "dist_points" => {
                let Some(pipe) = &self.metrics else { return };
                let lines = {
                    let mut series = self.series.lock().unwrap();
                    let Some(s) = row["SeriesId"].as_u64().and_then(|id| series.get_mut(&id)) else {
                        // A point always follows its series' registration: without it there are no labels
                        pipe.stats.dropped_rows.fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    s.seen = Instant::now();
                    let time = row["Time"].as_i64().unwrap_or_default();
                    if table == "points" {
                        row["Value"]
                            .as_f64()
                            .filter(|v| v.is_finite())
                            .map(|v| vec![import_line(&s.labels, &s.metric, None, v, time)])
                            .unwrap_or_default()
                    } else {
                        let mut out = Vec::new();
                        let buckets = row["Buckets"].as_array().map(Vec::as_slice).unwrap_or_default();
                        // One bucket per bound plus the last (+Inf), as the server reads them
                        for (i, b) in buckets.iter().take(s.bounds.len() + 1).enumerate() {
                            let le = s.bounds.get(i).copied().map_or_else(|| "+Inf".to_owned(), fmt_le);
                            if let Some(v) = b.as_f64() {
                                out.push(import_line(&s.labels, &format!("{}_bucket", s.metric), Some(("le", le)), v, time));
                            }
                        }
                        if let Some(v) = row["Sum"].as_f64().filter(|v| v.is_finite()) {
                            out.push(import_line(&s.labels, &format!("{}_sum", s.metric), None, v, time));
                        }
                        if let Some(v) = row["Count"].as_f64() {
                            out.push(import_line(&s.labels, &format!("{}_count", s.metric), None, v, time));
                        }
                        out
                    }
                };
                for l in lines {
                    pipe.push_line("vm", l);
                }
            }
            "logs" => {
                if let Some(pipe) = &self.logs {
                    pipe.push_line("vl", log_line(row));
                }
            }
            _ => {}
        }
    }

    fn prune(&self) {
        let now = Instant::now();
        self.series.lock().unwrap().retain(|_, s| now.duration_since(s.seen) < SERIES_IDLE);
    }
}

pub struct Sink {
    clickhouse: Option<Pipe>,
    victoria: Option<Victoria>,
    /// Staleness markers waiting for the next flush, and whether a failure to
    /// send them was already reported (once until they get through again)
    stale: Mutex<Vec<String>>,
    stale_warned: AtomicBool,
    /// Flushes that have taken the pending rows / the last flush whose rows all
    /// reached every destination or its disk spool. Rows queued before `taken`
    /// reaches k are durable once `done` reaches k (see `mark` and `is_durable`). A
    /// flush that cannot save a batch puts its rows back and leaves `done` where it was.
    taken: AtomicU64,
    done: AtomicU64,
    /// The first destination's stats (ClickHouse if there is one)
    pub stats: Arc<Stats>,
    /// Victoria's when it is the second destination
    pub victoria_stats: Option<Arc<Stats>>,
}

impl Sink {
    /// To ClickHouse; `and_victoria` adds Victoria
    pub fn new(url: String, user: String, password: String, database: String, spool: PathBuf, max_spool_bytes: u64) -> Result<Self> {
        Self::with_pool_idle(url, user, password, database, spool, max_spool_bytes, POOL_IDLE)
    }

    fn with_pool_idle(
        url: String,
        user: String,
        password: String,
        database: String,
        spool: PathBuf,
        max_spool_bytes: u64,
        pool_idle: Duration,
    ) -> Result<Self> {
        let stats = Arc::new(Stats::default());
        let backend = Backend::ClickHouse { url, user, password, database };
        Ok(Self::with(
            Some(Pipe::new(backend, TABLES, spool, max_spool_bytes, pool_idle, stats.clone())?),
            None,
            stats,
        ))
    }

    fn with(clickhouse: Option<Pipe>, victoria: Option<Victoria>, stats: Arc<Stats>) -> Self {
        Self {
            clickhouse,
            victoria,
            stale: Mutex::new(Vec::new()),
            stale_warned: AtomicBool::new(false),
            taken: AtomicU64::new(0),
            done: AtomicU64::new(0),
            stats,
            victoria_stats: None,
        }
    }

    /// Victoria's pipes share one set of stats; each has its own spool (`<spool>/metrics`,
    /// `<spool>/logs`) and half of the spool's size
    fn victoria_pipe(
        metrics_url: Option<String>,
        logs_url: Option<String>,
        auth: Option<(String, String)>,
        spool: PathBuf,
        max_spool_bytes: u64,
        stats: Arc<Stats>,
    ) -> Result<Victoria> {
        anyhow::ensure!(metrics_url.is_some() || logs_url.is_some(), "victoria needs metrics_url, logs_url or both");
        let pipe = |url: Option<String>, metrics: bool| -> Result<Option<Pipe>> {
            let Some(u) = url else { return Ok(None) };
            let (backend, tables, dir) = if metrics {
                (
                    Backend::Victoria {
                        metrics_url: Some(u),
                        logs_url: None,
                        auth: auth.clone(),
                    },
                    VM_TABLES,
                    "metrics",
                )
            } else {
                (
                    Backend::Victoria {
                        metrics_url: None,
                        logs_url: Some(u),
                        auth: auth.clone(),
                    },
                    VL_TABLES,
                    "logs",
                )
            };
            Ok(Some(Pipe::new(
                backend,
                tables,
                spool.join(dir),
                max_spool_bytes / 2,
                POOL_IDLE,
                stats.clone(),
            )?))
        };
        Ok(Victoria {
            metrics: pipe(metrics_url, true)?,
            logs: pipe(logs_url, false)?,
            series: Mutex::new(HashMap::new()),
        })
    }

    /// To VictoriaMetrics (metrics_url) and VictoriaLogs (logs_url) only, with basic auth if `auth`
    pub fn victoria(
        metrics_url: Option<String>,
        logs_url: Option<String>,
        auth: Option<(String, String)>,
        spool: PathBuf,
        max_spool_bytes: u64,
    ) -> Result<Self> {
        let stats = Arc::new(Stats::default());
        let v = Self::victoria_pipe(metrics_url, logs_url, auth, spool, max_spool_bytes, stats.clone())?;
        Ok(Self::with(None, Some(v), stats))
    }

    /// Also to VictoriaMetrics and VictoriaLogs, with their own disk spool
    pub fn and_victoria(
        mut self,
        metrics_url: Option<String>,
        logs_url: Option<String>,
        auth: Option<(String, String)>,
        spool: PathBuf,
        max_spool_bytes: u64,
    ) -> Result<Self> {
        let stats = Arc::new(Stats::default());
        self.victoria = Some(Self::victoria_pipe(metrics_url, logs_url, auth, spool, max_spool_bytes, stats.clone())?);
        self.victoria_stats = Some(stats);
        Ok(self)
    }

    /// What goes to ClickHouse: metrics, logs or both (both unless this is called). Its queue keeps
    /// only those tables, and the rest of the rows are not written there (nor the staleness
    /// markers, which go with the metrics). A batch already in its disk spool is still sent
    pub fn clickhouse_signals(mut self, metrics: bool, logs: bool) -> Self {
        if let Some(ch) = &mut self.clickhouse {
            let tables = match (metrics, logs) {
                (true, true) => TABLES,
                (true, false) => CH_METRIC_TABLES,
                (false, true) => CH_LOG_TABLES,
                (false, false) => &[],
            };
            ch.tables = tables;
            *ch.pending.get_mut().unwrap() = vec![Vec::new(); tables.len()];
        }
        self
    }

    /// Rows of a table waiting in memory for the next flush, in the destination that has the most.
    /// The log tailers stop reading past `crate::logs::HIGH_WATER` instead of letting the cap
    /// drop lines they then count as read
    pub fn pending_rows(&self, table: &str) -> usize {
        let ch = self.clickhouse.as_ref().map_or(0, |p| p.pending_rows(table));
        let v = match (table, &self.victoria) {
            ("logs", Some(Victoria { logs: Some(p), .. })) => p.pending_rows("vl"),
            ("points" | "dist_points", Some(Victoria { metrics: Some(p), .. })) => p.pending_rows("vm"),
            _ => 0,
        };
        ch.max(v)
    }

    pub fn push(&self, table: &str, row: Value) {
        if table == STALE {
            if self.clickhouse.as_ref().is_some_and(|c| c.has("points")) {
                let mut p = self.stale.lock().unwrap();
                if p.len() < MAX_PENDING_ROWS {
                    p.push(row.to_string());
                }
            }
            return;
        }
        if let Some(v) = &self.victoria {
            v.push(table, &row);
        }
        if let Some(ch) = self.clickhouse.as_ref().filter(|c| c.has(table)) {
            ch.push_line(table, row.to_string());
        }
    }

    /// Everything queued so far is safe once `is_durable(mark)` is true
    pub fn mark(&self) -> u64 {
        self.taken.load(Ordering::SeqCst) + 1
    }

    pub fn is_durable(&self, mark: u64) -> bool {
        self.done.load(Ordering::SeqCst) >= mark
    }

    pub async fn flush(&self) {
        // The round is numbered before any destination takes its rows: a row pushed after the
        // number was read (its mark is the next round) may still go in this one, never the reverse
        let round = self.taken.fetch_add(1, Ordering::SeqCst) + 1;
        let stale = std::mem::take(&mut *self.stale.lock().unwrap());
        if let Some(ch) = &self.clickhouse {
            self.send_stale(ch, stale).await;
        }
        if let Some(v) = &self.victoria {
            v.prune();
        }
        let (ch_ok, vm_ok, vl_ok) = tokio::join!(
            flush_pipe(self.clickhouse.as_ref()),
            flush_pipe(self.victoria.as_ref().and_then(|v| v.metrics.as_ref())),
            flush_pipe(self.victoria.as_ref().and_then(|v| v.logs.as_ref())),
        );
        if ch_ok && vm_ok && vl_ok {
            self.done.fetch_max(round, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    pub(crate) fn stale_rows(&self) -> Vec<Value> {
        self.stale.lock().unwrap().iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// Sends staleness markers, or drops them: they are never spooled
    async fn send_stale(&self, ch: &Pipe, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let n = lines.len();
        let sent = match gzip(&lines) {
            Ok(body) => ch.send(STALE, body).await.map_err(|e| e.to_string()),
            Err(e) => Err(format!("{e:#}")),
        };
        match sent {
            Ok(()) => self.stale_warned.store(false, Ordering::Relaxed),
            Err(e) => {
                if !self.stale_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!("dropping {n} staleness markers: {e} (until they get through again, this is not repeated)");
                }
            }
        }
    }

    #[cfg(test)]
    fn gzip(lines: &[String]) -> Result<Vec<u8>> {
        gzip(lines)
    }

    #[cfg(test)]
    fn to_spool(&self, table: &str, body: &[u8]) -> bool {
        self.clickhouse.as_ref().expect("ClickHouse").to_spool(table, body)
    }
}

/// The password of tests that never reach a real ClickHouse: generated, so
/// that no test carries a fixed credential
#[cfg(test)]
pub(crate) fn test_password() -> String {
    use std::hash::{BuildHasher, Hasher};
    format!("{:016x}", std::collections::hash_map::RandomState::new().build_hasher().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn rows_are_durable_only_after_their_flush_and_spool_has_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("atalaya-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // ClickHouse unreachable: the batch goes to the disk spool
        let sink = Sink::new(
            "http://127.0.0.1:1/".into(),
            "u".into(),
            test_password(),
            "atalaya".into(),
            dir.clone(),
            1 << 20,
        )
        .unwrap();
        sink.push("logs", json!({"Body": "a"}));
        let m = sink.mark();
        assert!(!sink.is_durable(m), "still only in memory");
        sink.flush().await;
        assert!(sink.is_durable(m), "in the disk spool");
        // Rows queued afterwards wait for the next flush
        sink.push("logs", json!({"Body": "b"}));
        let m2 = sink.mark();
        assert!(!sink.is_durable(m2));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].ends_with("-logs.json.gz"), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn spool_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atalaya-sink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        dir
    }

    fn spooled(dir: &std::path::Path) -> Vec<String> {
        use std::io::Read;
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().is_file()) {
            let mut s = String::new();
            flate2::read::GzDecoder::new(std::fs::File::open(e.path()).unwrap())
                .read_to_string(&mut s)
                .unwrap();
            out.push(s);
        }
        out
    }

    /// An HTTP server that answers every request with `status`. It reads the whole
    /// request first, so the client gets the status and not a reset connection.
    async fn answering(status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let Ok(n) = s.read(&mut chunk).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    let resp = format!("HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno");
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/")
    }

    /// A ClickHouse that keeps each connection open after answering, and closes
    /// it when the next request arrives on it, without answering: what a client
    /// sees when it reuses a connection just as the server's keep-alive ends.
    /// Returns its URL and how many connections it has accepted.
    async fn closing_reused_connections() -> (String, std::sync::Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(AtomicU64::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    for answer in [true, false] {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 8192];
                        loop {
                            let Ok(n) = s.read(&mut chunk).await else { return };
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                                let len = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if buf.len() >= end + 4 + len {
                                    break;
                                }
                            }
                        }
                        if !answer {
                            return;
                        }
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                    }
                });
            }
        });
        (format!("http://{addr}/"), accepted)
    }

    #[tokio::test]
    async fn a_connection_idle_past_the_pool_window_is_not_reused() {
        let dir = spool_dir("idle");
        let (url, accepted) = closing_reused_connections().await;
        let sink = Sink::with_pool_idle(
            url,
            "u".into(),
            test_password(),
            "atalaya".into(),
            dir.clone(),
            1 << 20,
            Duration::from_millis(50),
        )
        .unwrap();
        for body in ["a", "b"] {
            sink.push("logs", json!({ "Body": body }));
            sink.flush().await;
            // Longer than the pool keeps an idle connection, as between two flushes
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        assert_eq!(sink.stats.send_errors.load(Ordering::Relaxed), 0, "no send failed");
        assert!(spooled(&dir).is_empty(), "nothing went to the spool");
        assert_eq!(accepted.load(Ordering::SeqCst), 2, "each flush opened its own connection");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_long_spool_drains_a_slice_per_flush() {
        let dir = spool_dir("slices");
        std::fs::create_dir_all(&dir).unwrap();
        let sink = Sink::new(answering(200).await, "u".into(), test_password(), "atalaya".into(), dir.clone(), 1 << 30).unwrap();
        for i in 0..(SPOOL_PER_FLUSH + 8) {
            assert!(sink.to_spool("logs", &Sink::gzip(&[format!(r#"{{"Body":"old {i}"}}"#)]).unwrap()));
        }
        sink.push("logs", serde_json::json!({ "Body": "new" }));
        sink.flush().await;
        let left = spooled(&dir);
        assert_eq!(left.len(), 9, "the 8 old batches left and the new one behind them");
        assert!(left.iter().any(|b| b.contains("new")));
        sink.flush().await;
        assert!(spooled(&dir).is_empty(), "the next flush finishes it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clickhouse_can_take_only_the_logs_or_only_the_metrics() {
        let dir = std::env::temp_dir().join(format!("atalaya-sink-signals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = |logs_only: bool| {
            Sink::new(
                "http://127.0.0.1:1/".into(),
                "collector".into(),
                test_password(),
                "atalaya".into(),
                dir.join(if logs_only { "logs" } else { "metrics" }),
                1 << 20,
            )
            .unwrap()
            .clickhouse_signals(!logs_only, logs_only)
        };
        let rows = |s: &Sink| {
            s.push("series", json!({"SeriesId": 1, "Metric": "up"}));
            s.push("points", json!({"SeriesId": 1, "Time": 1, "Value": 1.0}));
            s.push("stale", json!({"Metric": "up", "SeriesId": 1, "Time": 2}));
            s.push("logs", json!({"Body": "hello"}));
        };
        let logs = sink(true);
        rows(&logs);
        assert_eq!((logs.pending_rows("series"), logs.pending_rows("points")), (0, 0), "no metric rows");
        assert!(logs.stale_rows().is_empty(), "no staleness markers either: they belong to the metrics");
        assert_eq!(logs.pending_rows("logs"), 1);
        let metrics = sink(false);
        rows(&metrics);
        assert_eq!((metrics.pending_rows("series"), metrics.pending_rows("points")), (1, 1));
        assert_eq!(metrics.stale_rows().len(), 1);
        assert_eq!(metrics.pending_rows("logs"), 0, "no log lines");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn staleness_markers_that_cannot_be_sent_are_dropped_not_spooled() {
        // A schema without the stale table: ClickHouse answers 404
        let dir = spool_dir("stale");
        let sink = Sink::new(answering(404).await, "u".into(), test_password(), "atalaya".into(), dir.clone(), 1 << 20).unwrap();
        sink.push("points", json!({"Metric": "m"}));
        sink.push("stale", json!({"Metric": "m"}));
        sink.flush().await;
        assert!(sink.stale_rows().is_empty(), "not kept for later");
        let batches = spooled(&dir);
        assert_eq!(batches.len(), 1, "only the points wait in the spool");
        assert!(batches[0].contains(r#""Metric":"m""#));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names[0].ends_with("-points.json.gz"), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rows_that_cannot_be_spooled_stay_queued_and_are_not_durable() {
        let dir = spool_dir("unwritable");
        let sink = Sink::new(
            "http://127.0.0.1:1/".into(),
            "u".into(),
            test_password(),
            "atalaya".into(),
            dir.clone(),
            1 << 20,
        )
        .unwrap();
        // ClickHouse is unreachable and the spool path is no longer a directory:
        // the batch reaches neither
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"").unwrap();
        sink.push("logs", json!({"Body": "a"}));
        let m = sink.mark();
        sink.flush().await;
        assert!(!sink.is_durable(m), "nothing was stored, so the log positions must not move");
        // The spool is back: the next flush stores the same row
        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        sink.flush().await;
        assert!(sink.is_durable(m));
        let batches = spooled(&dir);
        assert_eq!(batches.len(), 1, "{batches:?}");
        assert!(batches[0].contains(r#""Body":"a""#), "{batches:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn credential_and_missing_table_errors_wait_in_the_spool_but_bad_batches_are_dropped() {
        for (status, kept) in [(401, true), (403, true), (404, true), (400, false)] {
            let dir = spool_dir(&format!("status-{status}"));
            let sink = Sink::new(answering(status).await, "u".into(), test_password(), "atalaya".into(), dir.clone(), 1 << 20).unwrap();
            sink.push("logs", json!({"Body": "a"}));
            let m = sink.mark();
            sink.flush().await;
            assert_eq!(spooled(&dir).len(), usize::from(kept), "{status}: batches in the spool");
            assert_eq!(
                sink.stats.dropped_batches.load(Ordering::Relaxed),
                u64::from(!kept),
                "{status}: dropped batches"
            );
            assert!(sink.is_durable(m), "{status}: the batch is settled either way");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// An HTTP server that answers 204 and keeps every request: its path with the query, and its
    /// body, gunzipped
    pub(super) async fn capturing() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        use std::io::Read;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let keep = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let keep = keep.clone();
                tokio::spawn(async move {
                    loop {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 8192];
                        let (end, len) = loop {
                            let Ok(n) = s.read(&mut chunk).await else { return };
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                                let len = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if buf.len() >= end + 4 + len {
                                    break (end, len);
                                }
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let target = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
                        let mut body = String::new();
                        flate2::read::GzDecoder::new(&buf[end + 4..end + 4 + len]).read_to_string(&mut body).unwrap();
                        keep.lock().unwrap().push((target, body));
                        if s.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn sent_to(seen: &Mutex<Vec<(String, String)>>, path: &str) -> Vec<Value> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t.starts_with(path))
            .flat_map(|(_, b)| b.lines().map(|l| serde_json::from_str(l).unwrap()).collect::<Vec<Value>>())
            .collect()
    }

    /// Same case as the server's resource_keys_become_labels_without_overriding_the_series_own:
    /// Victoria has to see exactly the labels a dashboard sees in atalaya's PromQL
    #[test]
    fn victoria_labels_are_the_servers_promql_labels() {
        let resource = json!({"host.name": "node-1", "nomad.service": "web-metrics", "nomad.group": "api",
                              "nomad.alloc_index": "2", "instance": "0"});
        let labels = json!({"path": "/", "nomad_group": "from-exporter", "job": "exporter-job", "empty": ""});
        let l = label_view("web", resource.as_object().unwrap(), labels.as_object().unwrap());
        assert_eq!(
            Value::Object(l),
            json!({"instance": "node-1", "job": "web", "exported_job": "exporter-job", "nomad_alloc_index": "2",
                   "nomad_group": "from-exporter", "nomad_service": "web-metrics", "path": "/"})
        );
        assert_eq!(label_name("9.weird-key"), "_9_weird_key");
        assert_eq!(
            (fmt_le(0.005), fmt_le(1.0), fmt_le(10.0), fmt_le(f64::INFINITY)),
            ("0.005".into(), "1".into(), "10".into(), "+Inf".into())
        );
    }

    #[tokio::test]
    async fn victoria_gets_points_with_labels_histograms_as_buckets_and_log_lines_with_their_fields() {
        let dir = spool_dir("victoria");
        let (url, seen) = capturing().await;
        let sink = Sink::victoria(Some(url.clone()), Some(url), None, dir.clone(), 1 << 20).unwrap();
        let res = json!({"host.name": "thor", "nomad.group": "web"});
        sink.push(
            "series",
            json!({"SeriesId": 1, "Metric": "up", "Service": "api", "Resource": res, "Labels": {}, "Bounds": []}),
        );
        sink.push("points", json!({"Metric": "up", "SeriesId": 1, "Time": 1000, "Value": 1.0, "Counter": false}));
        sink.push(
            "series",
            json!({"SeriesId": 2, "Metric": "lat", "Service": "api", "Resource": res, "Labels": {"path": "/x"}, "Bounds": [0.5, 1.0]}),
        );
        sink.push(
            "dist_points",
            json!({"Metric": "lat", "SeriesId": 2, "Time": 1000, "Count": 7, "Sum": 3.5, "Buckets": [2, 5, 7]}),
        );
        sink.push(
            "logs",
            json!({"Time": "2026-11-24 22:47:06.761", "Service": "api", "Host": "thor", "Stream": "stderr", "Level": "ERROR",
                                 "Body": "boom", "Resource": {"nomad.job": "api", "nomad.task": "web", "host": "x"},
                                 "Attributes": {"level": "error", "nomad.job": "other", "status": "500", "blank": ""}}),
        );
        // Staleness markers and descriptors are ClickHouse's: nothing to send
        sink.push("stale", json!({"Metric": "up", "SeriesId": 1, "Time": 1000}));
        sink.push("metric_descriptors", json!({"Metric": "up", "Kind": "gauge"}));
        let m = sink.mark();
        sink.flush().await;
        assert!(sink.is_durable(m));

        let mut points: Vec<(String, Value)> = sent_to(&seen, "/api/v1/import").into_iter().map(|l| (l["metric"].to_string(), l)).collect();
        points.sort_by(|a, b| a.0.cmp(&b.0));
        let lines: Vec<Value> = points.into_iter().map(|(_, l)| l).collect();
        let base = json!({"instance": "thor", "job": "api", "nomad_group": "web"});
        let with = |extra: Value| {
            let mut m = base.as_object().unwrap().clone();
            m.extend(extra.as_object().unwrap().clone());
            Value::Object(m)
        };
        let expected = [
            (with(json!({"__name__": "lat_bucket", "le": "+Inf", "path": "/x"})), 7.0),
            (with(json!({"__name__": "lat_bucket", "le": "0.5", "path": "/x"})), 2.0),
            (with(json!({"__name__": "lat_bucket", "le": "1", "path": "/x"})), 5.0),
            (with(json!({"__name__": "lat_count", "path": "/x"})), 7.0),
            (with(json!({"__name__": "lat_sum", "path": "/x"})), 3.5),
            (with(json!({"__name__": "up"})), 1.0),
        ];
        assert_eq!(lines.len(), expected.len(), "{lines:?}");
        for (line, (metric, v)) in lines.iter().zip(expected) {
            assert_eq!(line["metric"], metric);
            assert_eq!(line["values"], json!([v]));
            assert_eq!(line["timestamps"], json!([1_000_000]), "milliseconds");
        }

        let logs = sent_to(&seen, "/insert/jsonline");
        assert_eq!(
            logs,
            vec![
                json!({"_time": "2026-11-24T22:47:06.761Z", "_msg": "boom", "job": "api", "host": "thor", "stream": "stderr",
                        "level": "ERROR", "nomad.job": "api", "nomad.task": "web", "attr.level": "error",
                        "attr.nomad.job": "other", "status": "500"})
            ]
        );
        let target = seen.lock().unwrap().iter().find(|(t, _)| t.starts_with("/insert/jsonline")).unwrap().0.clone();
        assert!(target.contains("_stream_fields=job%2Chost%2Cnomad.group%2Cnomad.task%2Cstream"), "{target}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Authorization header of each request a test server gets
    async fn auth_headers() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let keep = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let keep = keep.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let Ok(n) = s.read(&mut chunk).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned()))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() < end + 4 + len {
                            continue;
                        }
                        let auth = head.lines().find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                                .map(|(_, v)| v.trim().to_owned())
                        });
                        keep.lock().unwrap().push(auth);
                        buf.drain(..end + 4 + len);
                        if s.write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn victoria_sends_basic_auth_only_when_it_has_a_user() {
        for (auth, want) in [
            (Some(("writer".to_owned(), "s3cret".to_owned())), Some("Basic d3JpdGVyOnMzY3JldA==".to_owned())),
            (None, None),
        ] {
            let dir = spool_dir(&format!("victoria-auth-{}", want.is_some()));
            let (url, seen) = auth_headers().await;
            let sink = Sink::victoria(Some(url), None, auth, dir.clone(), 1 << 20).unwrap();
            sink.push(
                "series",
                json!({"SeriesId": 1, "Metric": "up", "Service": "api", "Resource": {"host.name": "h"}, "Labels": {}, "Bounds": []}),
            );
            sink.push("points", json!({"Metric": "up", "SeriesId": 1, "Time": 1000, "Value": 1.0, "Counter": false}));
            let m = sink.mark();
            sink.flush().await;
            assert!(sink.is_durable(m));
            let got = seen.lock().unwrap().clone();
            assert!(!got.is_empty(), "nothing reached VictoriaMetrics");
            assert!(got.iter().all(|a| *a == want), "{got:?}, want {want:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn a_point_whose_series_victoria_never_saw_is_dropped_and_counted() {
        let dir = spool_dir("noseries");
        let (url, seen) = capturing().await;
        let sink = Sink::victoria(Some(url), None, None, dir.clone(), 1 << 20).unwrap();
        sink.push("points", json!({"Metric": "up", "SeriesId": 9, "Time": 1, "Value": 1.0}));
        sink.flush().await;
        assert!(sent_to(&seen, "/api/v1/import").is_empty());
        assert_eq!(sink.stats.dropped_rows.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn with_two_destinations_a_row_is_durable_only_once_both_have_it() {
        let ch_dir = spool_dir("both-ch");
        let v_dir = spool_dir("both-v");
        let sink = Sink::new(answering(200).await, "u".into(), test_password(), "atalaya".into(), ch_dir.clone(), 1 << 20)
            .unwrap()
            .and_victoria(None, Some("http://127.0.0.1:1".into()), None, v_dir.clone(), 1 << 20)
            .unwrap();
        assert!(sink.victoria_stats.is_some());
        // Victoria is unreachable and its logs spool is not a directory any more
        let v_logs = v_dir.join("logs");
        std::fs::remove_dir_all(&v_logs).unwrap();
        std::fs::write(&v_logs, b"").unwrap();
        sink.push("logs", json!({"Time": "2026-11-24 22:00:00.000", "Body": "a"}));
        let m = sink.mark();
        sink.flush().await;
        assert!(!sink.is_durable(m), "ClickHouse has it, Victoria does not");
        assert_eq!(sink.pending_rows("logs"), 1, "kept for Victoria's next flush");
        // Victoria's spool is back: the next flush stores the line there and the row is durable
        std::fs::remove_file(&v_logs).unwrap();
        std::fs::create_dir_all(&v_logs).unwrap();
        sink.flush().await;
        assert!(sink.is_durable(m));
        let batches = spooled(&v_logs);
        assert_eq!(batches.len(), 1, "{batches:?}");
        assert!(batches[0].contains(r#""_msg":"a""#), "{batches:?}");
        let _ = std::fs::remove_dir_all(&ch_dir);
        let _ = std::fs::remove_dir_all(&v_dir);
    }

    #[tokio::test]
    async fn a_spooled_batch_for_a_destination_no_longer_configured_is_set_aside() {
        let dir = spool_dir("gone");
        let (url, _seen) = capturing().await;
        let sink = Sink::victoria(Some(url), None, None, dir.clone(), 1 << 20).unwrap();
        let v = sink.victoria.as_ref().unwrap();
        let metrics = v.metrics.as_ref().unwrap();
        assert!(metrics.to_spool("vl", &gzip(&[r#"{"_msg":"x"}"#.into()]).unwrap()));
        sink.flush().await;
        assert!(spooled(&dir.join("metrics")).is_empty());
        assert!(dir.join("metrics/rejected").read_dir().unwrap().next().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod victoria_outage_tests {
    use super::tests as tests_support;
    use super::*;

    #[tokio::test]
    async fn victorialogs_down_does_not_hold_up_victoriametrics() {
        let dir = std::env::temp_dir().join(format!("atalaya-sink-vl-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (url, seen) = tests_support::capturing().await;
        let sink = Sink::victoria(Some(url), Some("http://127.0.0.1:1".into()), None, dir.clone(), 1 << 20).unwrap();
        sink.push(
            "series",
            json!({"SeriesId": 1, "Metric": "up", "Service": "a", "Resource": {}, "Labels": {}, "Bounds": []}),
        );
        for t in [1, 2] {
            sink.push("points", json!({"Metric": "up", "SeriesId": 1, "Time": t, "Value": 1.0}));
            sink.push("logs", json!({"Time": "2026-11-24 22:00:00.000", "Body": "x"}));
            sink.flush().await;
        }
        let points: usize = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t.starts_with("/api/v1/import"))
            .map(|(_, b)| b.lines().count())
            .sum();
        assert_eq!(points, 2, "both points reached VictoriaMetrics");
        let logs_spool = std::fs::read_dir(dir.join("logs")).unwrap().flatten().filter(|e| e.path().is_file()).count();
        assert_eq!(logs_spool, 2, "the log lines wait in their own spool");
        assert!(std::fs::read_dir(dir.join("metrics")).unwrap().flatten().all(|e| !e.path().is_file()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
