// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Sending to ClickHouse: one gzip-compressed batch per table every flush
//! interval (JSONEachRow). If ClickHouse does not respond, the batch goes to a
//! disk spool that is retried in order; when the spool is full the oldest
//! batches are dropped.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::{Compression, write::GzEncoder};
use serde_json::Value;

pub const TABLES: &[&str] = &["metric_descriptors", "series", "points", "dist_points", "logs"];

/// Cap on in-memory rows per table between two sends. Normally there are a few
/// thousand; if sending gets stuck, the oldest are dropped (counted in
/// dropped_rows) rather than exhausting the node's memory.
const MAX_PENDING_ROWS: usize = 500_000;

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

pub struct Sink {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
    database: String,
    spool: PathBuf,
    max_spool_bytes: u64,
    pending: Mutex<Vec<Vec<String>>>,
    /// Flushes that have taken the pending rows / the last flush whose rows all
    /// reached ClickHouse or the disk spool. Rows queued before `taken` reaches k
    /// are durable once `done` reaches k (see `mark` and `is_durable`). A flush
    /// that cannot save a batch puts its rows back and leaves `done` where it was.
    taken: AtomicU64,
    done: AtomicU64,
    pub stats: Stats,
}

/// Failure sending a batch: if ClickHouse rejects it for its content, retrying
/// will not fix it
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

impl Sink {
    pub fn new(url: String, user: String, password: String, database: String, spool: PathBuf, max_spool_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&spool).with_context(|| format!("creating {}", spool.display()))?;
        Ok(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?,
            url,
            user,
            password,
            database,
            spool,
            max_spool_bytes,
            pending: Mutex::new(vec![Vec::new(); TABLES.len()]),
            taken: AtomicU64::new(0),
            done: AtomicU64::new(0),
            stats: Stats::default(),
        })
    }

    pub fn push(&self, table: &str, row: Value) {
        let i = TABLES.iter().position(|t| *t == table).expect("unknown table");
        let line = row.to_string();
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

    fn gzip(lines: &[String]) -> Result<Vec<u8>> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        for l in lines {
            enc.write_all(l.as_bytes())?;
            enc.write_all(b"\n")?;
        }
        Ok(enc.finish()?)
    }

    /// Everything queued so far is safe once `is_durable(mark)` is true
    pub fn mark(&self) -> u64 {
        self.taken.load(Ordering::SeqCst) + 1
    }

    pub fn is_durable(&self, mark: u64) -> bool {
        self.done.load(Ordering::SeqCst) >= mark
    }

    async fn send(&self, table: &str, body: Vec<u8>) -> std::result::Result<(), SendError> {
        let query = format!("INSERT INTO {}.{} FORMAT JSONEachRow", self.database, table);
        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            // No async insert: the collector already sends large batches, and
            // waiting for the server-side flush (up to 5 s per insert) slows it down.
            .query(&[("query", query.as_str()), ("input_format_skip_unknown_fields", "1"), ("async_insert", "0")])
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

    /// Sends what is pending; whatever fails goes to the disk spool.
    pub async fn flush(&self) {
        let (round, batches): (u64, Vec<Vec<String>>) = {
            let mut p = self.pending.lock().unwrap();
            let b = p.iter_mut().map(std::mem::take).collect();
            (self.taken.fetch_add(1, Ordering::SeqCst) + 1, b)
        };
        // What was waiting on disk goes first, to keep the order
        let spool_ok = self.retry_spool().await;
        // Tables in parallel
        let sends = batches.into_iter().enumerate().filter(|(_, l)| !l.is_empty()).map(|(i, lines)| async move {
            let table = TABLES[i];
            let n = lines.len() as u64;
            let body = match Self::gzip(&lines) {
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
                        tracing::error!("ClickHouse rejected {n} rows of {table}: {m}");
                        return None;
                    }
                    Err(e) => {
                        self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("sending {n} rows to {table}: {e}; spooling to disk");
                    }
                }
            }
            if self.to_spool(table, &body) { None } else { Some((i, lines)) }
        });
        // Rows that reached neither ClickHouse nor the disk go back to the front of
        // the queue for the next flush. Counting this flush as done would let the
        // log tailers save positions past lines that were never stored.
        let unsaved: Vec<(usize, Vec<String>)> = futures::future::join_all(sends).await.into_iter().flatten().collect();
        self.trim_spool();
        if unsaved.is_empty() {
            self.done.fetch_max(round, Ordering::SeqCst);
            return;
        }
        let mut p = self.pending.lock().unwrap();
        for (i, mut lines) in unsaved {
            tracing::warn!("{} rows of {} kept in memory for the next flush", lines.len(), TABLES[i]);
            lines.append(&mut p[i]);
            p[i] = lines;
        }
    }

    /// Writes a batch to the disk spool; false if it could not be written
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

    /// Retries the spool in order. Returns false if something is left unsent.
    async fn retry_spool(&self) -> bool {
        for (path, _) in self.spool_files() {
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
                    tracing::error!("ClickHouse rejected batch {name} ({m}); moved to {}", bad.display());
                }
                Err(e) => {
                    self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("disk spool still cannot be sent: {e}");
                    return false;
                }
            }
        }
        true
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

    /// The batches in the spool, decompressed
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
}
