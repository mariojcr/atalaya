// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Docker container logs (json-file driver), for using the collector without
//! Nomad. Reads `<docker_dir>/<id>/<id>-json.log`, one JSON line per message
//! ({"log": "...", "stream": "stdout", "time": "..."}), and takes the service
//! name from `config.v2.json` (Compose label or container name).
//!
//! Same as the Nomad logs: the first pass starts at the end of what already
//! exists, offsets are only saved once what was read is safe (see main) and a
//! line longer than MAX_READ does not stall reading.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::logs::{MAX_READ, build_row, read_from};
use crate::sink::Sink;

/// Read offset of each container (id → bytes)
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    files: BTreeMap<String, u64>,
}

pub struct DockerTailer {
    dir: PathBuf,
    state_path: PathBuf,
    state: State,
    first: bool,
    /// Docker splits long lines into 16 KiB chunks without a trailing "\n"
    partial: HashMap<String, String>,
    /// id → (service, container name, image)
    names: HashMap<String, (String, String, String)>,
    host: String,
    pub lines: u64,
}

#[derive(Deserialize)]
struct Entry {
    log: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    time: String,
}

impl DockerTailer {
    pub fn new(dir: PathBuf, state_dir: &Path, host: String) -> Self {
        let state_path = state_dir.join("docker-offsets.json");
        let saved: Option<State> = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        Self {
            dir,
            state_path,
            first: saved.is_none(),
            state: saved.unwrap_or_default(),
            partial: HashMap::new(),
            names: HashMap::new(),
            host,
            lines: 0,
        }
    }

    /// Service name: the Compose label if present, otherwise the container
    /// name without its leading slash
    fn name(&mut self, id: &str) -> (String, String, String) {
        if let Some(n) = self.names.get(id) {
            return n.clone();
        }
        let cfg: Value = std::fs::read(self.dir.join(id).join("config.v2.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let container = cfg["Name"].as_str().unwrap_or(id).trim_start_matches('/').to_owned();
        let service = cfg["Config"]["Labels"]["com.docker.compose.service"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| container.clone());
        let image = cfg["Config"]["Image"].as_str().unwrap_or_default().to_owned();
        let n = (service, container, image);
        self.names.insert(id.to_owned(), n.clone());
        n
    }

    pub async fn poll(&mut self, sink: &Sink) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        let mut ids: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        // Removed containers: drop them from the state
        self.state.files.retain(|id, _| ids.contains(id));
        self.partial.retain(|id, _| ids.contains(id));
        self.names.retain(|id, _| ids.contains(id));
        for id in ids {
            let path = self.dir.join(&id).join(format!("{id}-json.log"));
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let pos = match self.state.files.get(&id) {
                Some(p) => *p,
                None if self.first => meta.len(),
                None => 0,
            };
            let (buf, start) = read_from(&path, pos);
            let (complete, consumed) = match buf.iter().rposition(|&b| b == b'\n') {
                Some(i) => (&buf[..=i], i as u64 + 1),
                // A line that does not fit: discard it whole
                None if buf.len() as u64 >= MAX_READ => (&[][..], buf.len() as u64),
                None => (&[][..], 0),
            };
            let (service, container, image) = self.name(&id);
            for line in String::from_utf8_lossy(complete).lines() {
                let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
                // Chunk of a long line (no "\n"): accumulate until its end
                if !e.log.ends_with('\n') {
                    let p = self.partial.entry(id.clone()).or_default();
                    if p.len() < 64 << 10 {
                        p.push_str(&e.log);
                    }
                    continue;
                }
                let mut text = self.partial.remove(&id).unwrap_or_default();
                text.push_str(e.log.trim_end_matches(['\n', '\r']));
                let time = DateTime::parse_from_rfc3339(&e.time)
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let resource = json!({ "container.id": id, "container.name": container, "container.image": image });
                if let Some(row) = build_row(&self.host, &service, &e.stream, time, text, resource) {
                    sink.push("logs", row);
                    self.lines += 1;
                }
            }
            self.state.files.insert(id, start + consumed);
        }
        self.first = false;
    }

    pub fn offsets(&self) -> State {
        self.state.clone()
    }

    pub fn save_offsets(&self, state: &State) {
        if let Ok(b) = serde_json::to_vec(state) {
            let tmp = self.state_path.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, &self.state_path);
            }
        }
    }

    pub fn save(&self) {
        self.save_offsets(&self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn reads_new_lines_joins_partials_and_names_compose_services() {
        let root = std::env::temp_dir().join(format!("atalaya-docker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let id = "abc123";
        let c = root.join("containers").join(id);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(
            c.join("config.v2.json"),
            r#"{"Name":"/example-web-1","Config":{"Image":"nginx:1","Labels":{"com.docker.compose.service":"web"}}}"#,
        )
        .unwrap();
        let log = c.join(format!("{id}-json.log"));
        std::fs::write(&log, "{\"log\":\"old\\n\",\"stream\":\"stdout\",\"time\":\"2026-10-05T10:00:00Z\"}\n").unwrap();
        let sink = Sink::new(
            "http://127.0.0.1:1/".into(),
            "u".into(),
            crate::sink::test_password(),
            "atalaya".into(),
            root.join("spool"),
            1 << 20,
        )
        .unwrap();
        let mut t = DockerTailer::new(root.join("containers"), &root, "host-1".into());
        t.poll(&sink).await;
        assert_eq!(t.lines, 0, "existing content is not re-read");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "{{\"log\":\"level=error part \",\"stream\":\"stderr\",\"time\":\"2026-10-05T10:00:01Z\"}}").unwrap();
        writeln!(f, "{{\"log\":\"final\\n\",\"stream\":\"stderr\",\"time\":\"2026-10-05T10:00:01Z\"}}").unwrap();
        writeln!(f, "{{\"log\":\"hello\\n\",\"stream\":\"stdout\",\"time\":\"2026-10-05T10:00:02Z\"}}").unwrap();
        t.poll(&sink).await;
        assert_eq!(t.lines, 2, "the chunk and its end are one line");
        assert_eq!(t.offsets().files[id], std::fs::metadata(&log).unwrap().len());
        assert_eq!(t.name(id).0, "web");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_row_takes_the_level_from_text() {
        let row = build_row("h", "web", "stderr", Utc::now(), "level=error part final".into(), json!({})).unwrap();
        assert_eq!(row["Level"], "ERROR");
        assert_eq!(row["Service"], "web");
    }
}
