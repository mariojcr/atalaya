// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Tails the logs of this node's Nomad allocations
//! (<alloc_dir>/<alloc>/alloc/logs/<task>.<stdout|stderr>.<N>).
//!
//! Nomad numbers the files (it does not rename them): the highest index is
//! followed and, on reaching its end with a later one available, reading moves
//! to the next. Offsets are saved to disk. When starting without state, files
//! that already existed are read from the end (the past is not re-sent); new
//! allocations are read from the beginning.
//!
//! The podman driver writes each line as `<date> <stream> <F|P> <text>`;
//! split lines (P…P F) are joined before sending.
//!
//! `alloc/` is shared with the tasks, which can write to it, and the collector
//! runs as root: nothing a task puts there may make it read another file of the
//! host into the task's logs. The `logs` directory is opened without following
//! a symlink, and each file is looked up through that open directory (never
//! by its path again, so swapping the directory afterwards changes nothing),
//! again without following one. Only a regular file with a single link is
//! read: a hard link would be some other file under a log's name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::nomad::Nomad;
use crate::sink::Sink;

pub(crate) const MAX_READ: u64 = 4 << 20;
const MAX_BODY: usize = 64 << 10;
/// Log patterns only see the start of a line: enough for a structured line,
/// and the work per line stays bounded
const PATTERN_INPUT: usize = 4 << 10;
/// Time lines may spend matching patterns in one pass: a job's (with its
/// dispatched or periodic children) and the node's. No pattern is tried past
/// it, and the rest of the pass's lines go without pattern attributes, so
/// costly patterns cannot hold up every other allocation's logs.
const JOB_PATTERN_TIME: Duration = Duration::from_millis(50);
const NODE_PATTERN_TIME: Duration = Duration::from_millis(200);

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// group (alloc/task/stream) → (file index, offset)
    files: BTreeMap<String, (u64, u64)>,
}

pub struct Tailer {
    alloc_dir: PathBuf,
    state_path: PathBuf,
    state: State,
    /// Groups seen on the first pass: without state they are read from the end
    initial: Option<HashSet<String>>,
    partial: HashMap<String, String>,
    /// Groups in the middle of a line longer than MAX_READ: discard up to the
    /// next newline
    skipping: HashSet<String>,
    /// Groups already reported for running out of pattern time
    over_budget: HashSet<String>,
    /// Passes so far: each starts at a different group, so the node's pattern
    /// time does not always go to the same allocations
    passes: usize,
    host: String,
    pub lines: u64,
    /// Lines truncated or dropped for being too long
    pub truncated: u64,
}

struct Group {
    alloc: String,
    task: String,
    stream: String,
    files: Vec<(u64, PathBuf)>,
}

fn file_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?P<task>.+)\.(?P<stream>stdout|stderr)\.(?P<n>\d+)$").unwrap())
}

fn line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?P<ts>\S+) (?P<stream>stdout|stderr) (?P<p>[FP]) ?(?P<text>.*)$").unwrap())
}

fn level_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:level=|\[|\b)(trace|debug|dbg|info|inf|notice|warn|warning|wrn|error|err|fatal|ftl|crit|critical)(?:\]|\b)").unwrap())
}

fn normalize_level(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "trace" => "TRACE",
        "debug" | "dbg" => "DEBUG",
        "info" | "inf" | "notice" | "information" => "INFO",
        "warn" | "warning" | "wrn" => "WARN",
        "error" | "err" => "ERROR",
        "fatal" | "ftl" | "crit" | "critical" | "panic" => "FATAL",
        _ => "",
    }
}

impl Tailer {
    pub fn new(alloc_dir: PathBuf, state_dir: &Path, host: String) -> Self {
        let state_path = state_dir.join("log-offsets.json");
        let (state, initial) = match std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            Some(s) => (s, Some(HashSet::new())),
            None => (State::default(), None),
        };
        Self {
            alloc_dir,
            state_path,
            state,
            initial,
            partial: HashMap::new(),
            skipping: HashSet::new(),
            over_budget: HashSet::new(),
            passes: 0,
            host,
            lines: 0,
            truncated: 0,
        }
    }

    fn scan(&self) -> BTreeMap<String, Group> {
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        let Ok(allocs) = std::fs::read_dir(&self.alloc_dir) else { return groups };
        for a in allocs.flatten() {
            let alloc = a.file_name().to_string_lossy().to_string();
            let dir = a.path().join("alloc/logs");
            let Some(d) = open_dir(&dir) else { continue };
            let Ok(files) = std::fs::read_dir(fd_path(&d)) else { continue };
            for f in files.flatten() {
                if !f.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let name = f.file_name().to_string_lossy().to_string();
                let Some(c) = file_re().captures(&name) else { continue };
                let (task, stream) = (c["task"].to_owned(), c["stream"].to_owned());
                let n: u64 = c["n"].parse().unwrap_or(0);
                let key = format!("{alloc}/{task}/{stream}");
                groups
                    .entry(key)
                    .or_insert_with(|| Group {
                        alloc: alloc.clone(),
                        task,
                        stream,
                        files: Vec::new(),
                    })
                    .files
                    .push((n, dir.join(f.file_name())));
            }
        }
        for g in groups.values_mut() {
            g.files.sort();
        }
        groups
    }

    /// Reads what is new in every file and sends it to the sink.
    pub async fn poll(&mut self, nomad: &Nomad, sink: &Sink) {
        let groups = self.scan();
        // First pass without state: everything that exists starts at the end
        if self.initial.is_none() {
            for (key, g) in &groups {
                if let Some((n, path)) = g.files.last() {
                    let len = std::fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0);
                    self.state.files.insert(key.clone(), (*n, len));
                }
            }
            self.initial = Some(groups.keys().cloned().collect());
        }
        // Allocations garbage-collected by Nomad: drop them from the state (and their partial lines)
        self.state.files.retain(|k, _| groups.contains_key(k));
        self.partial.retain(|k, _| groups.contains_key(k));
        self.skipping.retain(|k| groups.contains_key(k));
        self.over_budget.retain(|k| groups.contains_key(k));

        let mut budget = Budget::new();
        let first = self.passes % groups.len().max(1);
        self.passes = self.passes.wrapping_add(1);
        for (key, g) in groups.iter().cycle().skip(first).take(groups.len()) {
            let (mut idx, mut pos) = self.state.files.get(key).copied().unwrap_or((g.files[0].0, 0));
            let info = nomad.alloc(&g.alloc).await.unwrap_or_default();
            loop {
                let Some((_, path)) = g.files.iter().find(|(n, _)| *n == idx) else {
                    // The file is gone: move to the next available one
                    match g.files.iter().find(|(n, _)| *n > idx) {
                        Some((n, _)) => {
                            idx = *n;
                            pos = 0;
                            continue;
                        }
                        None => break,
                    }
                };
                let (mut buf, start) = read_from(path, pos);
                pos = start;
                let has_next = g.files.iter().any(|(n, _)| *n > idx);
                // Rest of a huge line: discard up to its newline
                if self.skipping.contains(key.as_str()) {
                    match buf.iter().position(|&b| b == b'\n') {
                        Some(i) => {
                            self.skipping.remove(key.as_str());
                            pos += i as u64 + 1;
                            buf.drain(..=i);
                        }
                        None => {
                            pos += buf.len() as u64;
                            if (buf.len() as u64) < MAX_READ {
                                break;
                            }
                            continue;
                        }
                    }
                }
                // A line that does not fit in MAX_READ: emit it truncated and skip
                // the rest (otherwise the same block would be re-read every pass)
                if buf.len() as u64 >= MAX_READ && !buf.contains(&b'\n') && !has_next {
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    self.emit(key, g, &info, &head, sink, &mut budget);
                    self.truncated += 1;
                    self.skipping.insert(key.clone());
                    pos += buf.len() as u64;
                    continue;
                }
                // Complete lines only (the newline is searched in bytes so the
                // offset does not drift with invalid UTF-8); when moving to the
                // next file, the last one too.
                let (complete, consumed): (&[u8], u64) = match buf.iter().rposition(|&b| b == b'\n') {
                    Some(i) => (&buf[..=i], i as u64 + 1),
                    None if has_next => (&buf[..], buf.len() as u64),
                    None => (&[][..], 0),
                };
                for line in String::from_utf8_lossy(complete).lines() {
                    self.emit(key, g, &info, line, sink, &mut budget);
                }
                pos += consumed;
                if consumed == 0 && has_next {
                    // End of this file: move to the next
                    idx = g.files.iter().find(|(n, _)| *n > idx).map(|(n, _)| *n).unwrap_or(idx);
                    pos = 0;
                    continue;
                }
                if (buf.len() as u64) < MAX_READ || consumed == 0 {
                    break;
                }
            }
            self.state.files.insert(key.clone(), (idx, pos));
        }
    }

    /// Current offsets, to be saved once what was read is safe
    pub fn offsets(&self) -> State {
        self.state.clone()
    }

    pub fn save(&self) {
        self.save_offsets(&self.state);
    }

    pub fn save_offsets(&self, state: &State) {
        if let Ok(b) = serde_json::to_vec(state) {
            let tmp = self.state_path.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, &self.state_path);
            }
        }
    }

    fn emit(&mut self, key: &str, g: &Group, info: &crate::nomad::AllocInfo, line: &str, sink: &Sink, budget: &mut Budget) {
        let (time, stream, text) = match line_re().captures(line) {
            Some(c) => {
                let text = c["text"].to_owned();
                if &c["p"] == "P" {
                    // Chunks of a long line (podman format): capped, since the
                    // body is truncated to MAX_BODY anyway
                    let p = self.partial.entry(key.to_owned()).or_default();
                    if p.len() < MAX_BODY {
                        p.push_str(&text);
                    } else {
                        self.truncated += 1;
                    }
                    return;
                }
                let full = match self.partial.remove(key) {
                    Some(mut p) => {
                        p.push_str(&text);
                        p
                    }
                    None => text,
                };
                let time = DateTime::parse_from_rfc3339(&c["ts"])
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                (time, c["stream"].to_owned(), full)
            }
            None => (Utc::now(), g.stream.clone(), line.to_owned()),
        };
        let service = if info.job.is_empty() { g.task.clone() } else { info.job.clone() };
        let resource = json!({
            "nomad.job": info.job,
            "nomad.group": info.group,
            "nomad.task": g.task,
            "nomad.alloc": g.alloc,
            "nomad.namespace": info.namespace,
        });
        let list = info.log_regex.get(&g.task).map_or(&[][..], Vec::as_slice);
        let left = if list.is_empty() { Duration::ZERO } else { budget.left(&info.owner) };
        let list = if left.is_zero() { &[] } else { list };
        let started = Instant::now();
        let row = build_row(
            &self.host,
            &service,
            &stream,
            time,
            text,
            resource,
            LinePatterns { list, until: started + left },
        );
        if !list.is_empty() && budget.spend(&info.owner, started.elapsed()) && self.over_budget.insert(key.to_owned()) {
            tracing::warn!(
                "logs: {key}: out of log pattern time in one pass ({JOB_PATTERN_TIME:?} per job, {NODE_PATTERN_TIME:?} per node); the rest of its lines in that pass went without their attributes"
            );
        }
        if let Some(row) = row {
            sink.push("logs", row);
            self.lines += 1;
        }
    }
}

/// What is left of a pass's pattern time, per job (`AllocInfo::owner`) and for
/// the node
struct Budget {
    node: Duration,
    jobs: HashMap<String, Duration>,
}

impl Budget {
    fn new() -> Self {
        Self {
            node: NODE_PATTERN_TIME,
            jobs: HashMap::new(),
        }
    }

    /// Time left for a job's lines
    fn left(&self, job: &str) -> Duration {
        self.node.min(self.jobs.get(job).copied().unwrap_or(JOB_PATTERN_TIME))
    }

    /// Takes the time a job's line took; true if that leaves the job none
    fn spend(&mut self, job: &str, took: Duration) -> bool {
        self.node = self.node.saturating_sub(took);
        let left = self.jobs.entry(job.to_owned()).or_insert(JOB_PATTERN_TIME);
        *left = left.saturating_sub(took);
        self.left(job).is_zero()
    }
}

/// The patterns a plain-text line is matched against, in order; none is tried
/// once `until` has passed
#[derive(Clone, Copy)]
pub(crate) struct LinePatterns<'a> {
    list: &'a [Arc<Regex>],
    until: Instant,
}

impl<'a> LinePatterns<'a> {
    pub(crate) fn none() -> Self {
        Self {
            list: &[],
            until: Instant::now(),
        }
    }

    /// The first pattern that matches the text, with its captures
    fn first_match<'t>(&self, text: &'t str) -> Option<(&'a Regex, regex::Captures<'t>)> {
        let mut tried = self.list.iter().take_while(|_| Instant::now() < self.until);
        tried.find_map(|re| re.captures(text).map(|c| (&**re, c)))
    }
}

/// Nested JSON objects become dotted keys down to this depth, and only under a
/// key of at most this length: each child repeats it, so a long one would
/// multiply a line's size (the server does not query longer field names)
const MAX_JSON_DEPTH: usize = 4;
const MAX_JSON_PREFIX: usize = 128;

/// A JSON line's fields as attributes, at most 64: nested objects become dotted
/// keys (`question.name`); arrays, and objects past those limits, stay as JSON
/// text; nulls are left out
fn json_attributes(obj: Map<String, Value>) -> Vec<(String, String)> {
    fn walk(prefix: Option<&str>, obj: Map<String, Value>, depth: usize, out: &mut Vec<(String, String)>) {
        for (k, v) in obj {
            if out.len() == 64 {
                return;
            }
            let key = match prefix {
                Some(p) => format!("{p}.{k}"),
                None => k,
            };
            match v {
                Value::Null => {}
                Value::String(s) => out.push((key, s)),
                Value::Object(o) if depth < MAX_JSON_DEPTH && key.len() <= MAX_JSON_PREFIX => walk(Some(&key), o, depth + 1, out),
                other => out.push((key, other.to_string())),
            }
        }
    }
    let mut out = Vec::new();
    walk(None, obj, 1, &mut out);
    out
}

/// Keys whose value is the line's level
fn is_level_key(k: &str) -> bool {
    matches!(k, "level" | "lvl" | "severity" | "log.level" | "levelname")
}

/// A log line in the logs table format: level (from JSON or from the text),
/// attributes from JSON or from the named groups of the first pattern a
/// plain-text line matches, and the body truncated to MAX_BODY
pub(crate) fn build_row(host: &str, service: &str, stream: &str, time: DateTime<Utc>, text: String, resource: Value, patterns: LinePatterns) -> Option<Value> {
    if text.trim().is_empty() {
        return None;
    }

    let mut attributes = Map::new();
    let mut level = String::new();
    if text.starts_with('{')
        && let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text)
    {
        for (k, s) in json_attributes(obj) {
            if level.is_empty() && is_level_key(&k) {
                level = normalize_level(&s).to_owned();
            }
            attributes.insert(k, Value::String(s));
        }
    } else if let Some((re, c)) = patterns.first_match(head(&text, PATTERN_INPUT)) {
        for (name, m) in re.capture_names().zip(c.iter()) {
            if let (Some(k), Some(m)) = (name, m)
                && attributes.len() < 64
            {
                if level.is_empty() && is_level_key(k) {
                    level = normalize_level(m.as_str()).to_owned();
                }
                attributes.insert(k.to_owned(), Value::String(m.as_str().to_owned()));
            }
        }
    }
    if level.is_empty() {
        let head: String = text.chars().take(200).collect();
        if let Some(c) = level_re().captures(&head) {
            level = normalize_level(&c[1]).to_owned();
        }
    }
    let mut body = text;
    body.truncate(head(&body, MAX_BODY).len());
    Some(json!({
        "Time": time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        "Service": service,
        "Host": host,
        "Stream": stream,
        "Level": level,
        "Body": body,
        "Resource": resource,
        "Attributes": attributes,
    }))
}

/// The first `max` bytes of a text, or fewer to end at a character boundary
fn head(text: &str, max: usize) -> &str {
    let mut cut = text.len().min(max);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// A directory opened without following a symlink in its last component
fn open_dir(path: &Path) -> Option<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

/// The path of an open directory through its descriptor: names under it are
/// looked up in that directory, like `openat`, whatever its path points to now
fn fd_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

/// Reads at most MAX_READ bytes from `pos`. Returns the bytes and the offset
/// they were read from (0 if the file was truncated). The file is looked up in
/// its directory, opened without following a symlink, and only read if it is a
/// regular file with a single link (a FIFO swapped in does not block the open).
pub(crate) fn read_from(path: &Path, pos: u64) -> (Vec<u8>, u64) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return (Vec::new(), pos);
    };
    let Some(d) = open_dir(dir) else { return (Vec::new(), pos) };
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(fd_path(&d).join(name));
    let Ok(mut f) = opened else { return (Vec::new(), pos) };
    let Ok(meta) = f.metadata() else { return (Vec::new(), pos) };
    if !meta.is_file() || meta.nlink() != 1 {
        return (Vec::new(), pos);
    }
    let len = meta.len();
    // File truncated or replaced: start from the beginning
    let pos = if pos > len { 0 } else { pos };
    if f.seek(SeekFrom::Start(pos)).is_err() {
        return (Vec::new(), pos);
    }
    let mut buf = Vec::new();
    let _ = f.take(MAX_READ).read_to_end(&mut buf);
    (buf, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn a_line_longer_than_max_read_does_not_stall_the_tailer() {
        let root = std::env::temp_dir().join(format!("atalaya-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let logs = root.join("allocs/abc/alloc/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let file = logs.join("web.stdout.0");
        std::fs::write(&file, b"").unwrap();
        let spool = root.join("spool");
        let sink = Sink::new(
            "http://127.0.0.1:1/".into(),
            "u".into(),
            crate::sink::test_password(),
            "atalaya".into(),
            spool,
            1 << 30,
        )
        .unwrap();
        let nomad = Nomad::new("http://127.0.0.1:1".into(), "n".into(), None, None).unwrap();
        let mut t = Tailer::new(root.join("allocs"), &root, "host-1".into());
        t.poll(&nomad, &sink).await; // first pass: starts at the end
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"before\n").unwrap();
        f.write_all(&vec![b'x'; (MAX_READ + (1 << 20)) as usize]).unwrap();
        t.poll(&nomad, &sink).await;
        assert_eq!(t.lines, 2, "the normal line and the truncated start of the huge one");
        assert_eq!(t.truncated, 1);
        // The end of the huge line arrives, then a new line
        f.write_all(b"xxx\nafter\n").unwrap();
        t.poll(&nomad, &sink).await;
        assert_eq!(t.lines, 3, "only the new one; the rest of the huge line is dropped");
        let pos = t.offsets().files.values().next().unwrap().1;
        assert_eq!(pos, std::fs::metadata(&file).unwrap().len(), "read to the end");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pattern_time_is_shared_by_a_job_and_by_the_node() {
        let mut b = Budget::new();
        assert!(b.spend("ns/a", JOB_PATTERN_TIME), "job a is out of time");
        assert_eq!(b.left("ns/a"), Duration::ZERO);
        assert_eq!(b.left("ns/b"), JOB_PATTERN_TIME, "b still has its own");
        for job in ["ns/b", "ns/c", "ns/d"] {
            b.spend(job, JOB_PATTERN_TIME - Duration::from_millis(1));
        }
        assert_eq!(
            b.left("ns/e"),
            NODE_PATTERN_TIME - 4 * JOB_PATTERN_TIME + Duration::from_millis(3),
            "the node's time runs out first"
        );
        assert!(b.spend("ns/e", Duration::from_millis(3)), "none left for anyone");
        assert_eq!(b.left("ns/f"), Duration::ZERO);
    }

    #[test]
    fn nested_json_objects_become_dotted_attributes() {
        let row = |text: &str| build_row("h", "dns", "stdout", Utc::now(), text.into(), json!({}), LinePatterns::none()).unwrap();
        let r = row(
            r#"{"question":{"questionName":"example.com","questionType":"A"},"answers":[{"recordData":"192.0.2.1"}],"responseType":"Cached","rtt":null,"log":{"level":"warn"},"a":{"b":{"c":{"d":{"e":1}}}}}"#,
        );
        assert_eq!(
            r["Attributes"],
            json!({
                "question.questionName": "example.com",
                "question.questionType": "A",
                "answers": r#"[{"recordData":"192.0.2.1"}]"#,
                "responseType": "Cached",
                "log.level": "warn",
                "a.b.c.d": r#"{"e":1}"#,
            }),
            "arrays and objects past MAX_JSON_DEPTH stay as JSON text"
        );
        assert_eq!(r["Level"], "WARN", "a nested log.level sets it");
        // At most 64 attributes, nested or not
        let wide = format!("{{\"o\":{{{}}}}}", (0..100).map(|i| format!("\"k{i}\":{i}")).collect::<Vec<_>>().join(","));
        assert_eq!(row(&wide)["Attributes"].as_object().unwrap().len(), 64);
        // A long key is not repeated in front of each child
        let long = "k".repeat(MAX_JSON_PREFIX + 1);
        let r = row(&format!("{{\"{long}\":{{\"a\":1,\"b\":2}}}}"));
        assert_eq!(r["Attributes"], json!({ long: r#"{"a":1,"b":2}"# }));
    }

    #[test]
    fn no_pattern_is_tried_past_its_deadline() {
        let list = [Arc::new(Regex::new("(?P<x>.)").unwrap())];
        let row = |until| build_row("h", "web", "stdout", Utc::now(), "text".into(), json!({}), LinePatterns { list: &list, until }).unwrap();
        assert_eq!(row(Instant::now() + JOB_PATTERN_TIME)["Attributes"], json!({"x": "t"}));
        assert_eq!(row(Instant::now())["Attributes"], json!({}), "out of time: the line goes without them");
    }

    #[test]
    fn a_plain_text_line_takes_the_named_groups_of_the_first_pattern_it_matches() {
        let patterns = [
            r"reply: (?P<client>\S+) (?P<qname>\S+) (?P<qtype>\S+) \S+ (?P<rcode>\S+)",
            r"(?P<level>info): rpz: applied \[(?P<rpz>[^\]]+)\] \S+ (?P<action>\S+) (?P<client>[^@ ]+)@\d+ (?P<qname>\S+)",
            r"(?P<qname>\S+)",
        ]
        .map(|p| Arc::new(Regex::new(p).unwrap()));
        let all = |n: usize| LinePatterns {
            list: &patterns[..n],
            until: Instant::now() + JOB_PATTERN_TIME,
        };
        let row = |text: &str| build_row("h", "dns", "stderr", Utc::now(), text.into(), json!({}), all(3)).unwrap();

        let r = row("[1792868141] unbound[1:0] reply: 10.10.0.5 example.com. A IN NOERROR 0.012000 0 56");
        assert_eq!(
            r["Attributes"],
            json!({"client": "10.10.0.5", "qname": "example.com.", "qtype": "A", "rcode": "NOERROR"})
        );
        let r = row("[1792868141] unbound[1:0] info: rpz: applied [hagezi] ads.example. nxdomain 10.10.0.5@53412 x.ads.example. A IN");
        assert_eq!(r["Attributes"]["rpz"], "hagezi");
        assert_eq!(r["Attributes"]["qname"], "x.ads.example.");
        assert_eq!(r["Level"], "INFO", "a group named level sets it");
        // JSON keeps its own keys; the patterns are only for plain text
        let r = row(r#"{"msg":"hi","level":"warn"}"#);
        assert_eq!(r["Attributes"], json!({"msg": "hi", "level": "warn"}));
        let r = build_row("h", "dns", "stderr", Utc::now(), "no patterns".into(), json!({}), LinePatterns::none()).unwrap();
        assert_eq!(r["Attributes"], json!({}));
        // Only the start of a line is matched
        let late = format!("{} reply: 10.10.0.5 example.com. A IN NOERROR", " ".repeat(PATTERN_INPUT));
        let r = build_row("h", "dns", "stderr", Utc::now(), late, json!({}), all(1)).unwrap();
        assert_eq!(r["Attributes"], json!({}));
    }

    #[test]
    fn a_symlink_in_the_shared_alloc_dir_is_not_followed() {
        let root = std::env::temp_dir().join(format!("atalaya-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let secret = root.join("secret");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&secret, b"root only\n").unwrap();
        // A task links a file of the host as one of its logs...
        let logs = root.join("allocs/abc/alloc/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::os::unix::fs::symlink(&secret, logs.join("web.stdout.0")).unwrap();
        std::fs::write(logs.join("web.stderr.0"), b"a real log\n").unwrap();
        // ...or replaces its logs directory with a link to a directory of the host
        std::fs::create_dir_all(root.join("allocs/def/alloc")).unwrap();
        std::fs::create_dir_all(root.join("host-dir")).unwrap();
        std::fs::write(root.join("host-dir/web.stdout.0"), b"not a log\n").unwrap();
        std::os::unix::fs::symlink(root.join("host-dir"), root.join("allocs/def/alloc/logs")).unwrap();

        let t = Tailer::new(root.join("allocs"), &root, "host-1".into());
        let groups = t.scan();
        assert_eq!(groups.keys().collect::<Vec<_>>(), ["abc/web/stderr"], "only the regular file");
        // A file swapped for a symlink after the scan is not read either
        assert!(read_from(&logs.join("web.stdout.0"), 0).0.is_empty());
        assert_eq!(read_from(&logs.join("web.stderr.0"), 0).0, b"a real log\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hard_link_or_a_logs_directory_swapped_after_the_scan_is_not_read() {
        let root = std::env::temp_dir().join(format!("atalaya-hardlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let logs = root.join("allocs/abc/alloc/logs");
        std::fs::create_dir_all(&logs).unwrap();
        // A task hard-links a file of the host (same filesystem) as one of its logs
        let secret = root.join("secret");
        std::fs::write(&secret, b"root only\n").unwrap();
        std::fs::hard_link(&secret, logs.join("web.stdout.0")).unwrap();
        std::fs::write(logs.join("web.stderr.0"), b"a real log\n").unwrap();
        assert!(read_from(&logs.join("web.stdout.0"), 0).0.is_empty(), "two links: not read");
        assert_eq!(read_from(&logs.join("web.stderr.0"), 0).0, b"a real log\n");

        // After the scan, the task swaps its logs directory for a link to a
        // directory of the host holding a file with a log's name
        let t = Tailer::new(root.join("allocs"), &root, "host-1".into());
        let groups = t.scan();
        assert!(groups.contains_key("abc/web/stderr"));
        std::fs::create_dir_all(root.join("host-dir")).unwrap();
        std::fs::write(root.join("host-dir/web.stderr.0"), b"not a log\n").unwrap();
        std::fs::rename(&logs, root.join("allocs/abc/alloc/old-logs")).unwrap();
        std::os::unix::fs::symlink(root.join("host-dir"), &logs).unwrap();
        assert!(read_from(&logs.join("web.stderr.0"), 0).0.is_empty(), "the directory is a symlink now");
        assert!(t.scan().is_empty(), "and the next scan skips it");
        let _ = std::fs::remove_dir_all(&root);
    }
}
