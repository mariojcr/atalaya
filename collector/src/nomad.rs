// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Nomad API queries: this node's services that expose metrics and the details
//! of each allocation (job, group, index, log patterns), cached.
//!
//! A service registration's address and port are the job's to choose, and the
//! collector scrapes from the host's network: a target is only scraped at an
//! address of its own allocation (its address in its network namespace, or a
//! host port it was given), never wherever a job points it.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Target {
    /// Job name: the series' `Service`
    pub job: String,
    pub service: String,
    pub url: String,
    pub alloc: String,
}

#[derive(Debug, Clone, Default)]
pub struct AllocInfo {
    pub job: String,
    pub group: String,
    pub namespace: String,
    /// Allocation index (job.group[N]): tells replicas apart without changing on every deployment
    pub index: String,
    /// Where the allocation answers
    pub reach: Reach,
    /// The patterns each task's plain-text log lines are parsed with
    pub log_regex: Arc<HashMap<String, Vec<Arc<Regex>>>>,
}

/// Meta key (task, group or job) with the patterns that turn a task's
/// plain-text log lines into attributes: one regular expression per line
const LOG_REGEX: &str = "atalaya.log_regex";
/// The patterns come from the job, so they are bounded: at most this many per
/// task, this many in use on the node (the same text is compiled once for
/// every allocation), and each compiled, and its lazy DFA cache, up to this size
const MAX_PATTERNS: usize = 16;
const MAX_NODE_PATTERNS: usize = 32;
const PATTERN_SIZE: usize = 64 << 10;

/// Where an allocation answers: its own address in its network namespace (any
/// port there is its own) and the host addresses and ports it was given
#[derive(Debug, Clone, Default)]
pub struct Reach {
    own: Option<IpAddr>,
    ports: Vec<(IpAddr, u16)>,
}

impl Reach {
    fn allows(&self, ip: IpAddr, port: u16) -> bool {
        self.own == Some(ip) || self.ports.contains(&(ip, port))
    }
}

#[derive(Deserialize)]
struct ServiceList {
    #[serde(rename = "Services")]
    services: Vec<ServiceStub>,
}

#[derive(Deserialize)]
struct ServiceStub {
    #[serde(rename = "ServiceName")]
    name: String,
}

#[derive(Deserialize)]
struct Registration {
    #[serde(rename = "NodeID")]
    node_id: String,
    #[serde(rename = "Address")]
    address: String,
    #[serde(rename = "Port")]
    port: u16,
    #[serde(rename = "Tags", default)]
    tags: Vec<String>,
    #[serde(rename = "JobID")]
    job_id: String,
    #[serde(rename = "AllocID")]
    alloc_id: String,
    #[serde(rename = "ServiceName")]
    service: String,
}

#[derive(Deserialize)]
struct Allocation {
    #[serde(rename = "JobID")]
    job_id: String,
    #[serde(rename = "TaskGroup")]
    task_group: String,
    #[serde(rename = "Namespace")]
    namespace: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "ClientStatus", default)]
    client_status: String,
    #[serde(rename = "NetworkStatus", default)]
    network_status: Option<NetworkStatus>,
    #[serde(rename = "AllocatedResources", default)]
    resources: Option<AllocatedResources>,
    #[serde(rename = "Job", default)]
    job: Option<JobSpec>,
}

/// The job the allocation runs: only its meta
#[derive(Deserialize)]
struct JobSpec {
    #[serde(rename = "Meta", default)]
    meta: Option<HashMap<String, String>>,
    #[serde(rename = "TaskGroups", default)]
    groups: Option<Vec<GroupSpec>>,
}

#[derive(Deserialize)]
struct GroupSpec {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Meta", default)]
    meta: Option<HashMap<String, String>>,
    #[serde(rename = "Tasks", default)]
    tasks: Option<Vec<TaskSpec>>,
}

#[derive(Deserialize)]
struct TaskSpec {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Meta", default)]
    meta: Option<HashMap<String, String>>,
}

/// The group network's status (bridge or CNI): the allocation's own address
#[derive(Deserialize)]
struct NetworkStatus {
    #[serde(rename = "Address", default)]
    address: String,
}

#[derive(Deserialize)]
struct AllocatedResources {
    #[serde(rename = "Shared", default)]
    shared: Option<SharedResources>,
    /// Task-level networks (older jobs)
    #[serde(rename = "Tasks", default)]
    tasks: Option<HashMap<String, TaskResources>>,
}

#[derive(Deserialize)]
struct SharedResources {
    #[serde(rename = "Networks", default)]
    networks: Option<Vec<Network>>,
    #[serde(rename = "Ports", default)]
    ports: Option<Vec<PortMapping>>,
}

#[derive(Deserialize)]
struct TaskResources {
    #[serde(rename = "Networks", default)]
    networks: Option<Vec<Network>>,
}

#[derive(Deserialize)]
struct Network {
    #[serde(rename = "IP", default)]
    ip: String,
    #[serde(rename = "ReservedPorts", default)]
    reserved: Option<Vec<Port>>,
    #[serde(rename = "DynamicPorts", default)]
    dynamic: Option<Vec<Port>>,
}

#[derive(Deserialize)]
struct Port {
    #[serde(rename = "Value")]
    value: u16,
}

#[derive(Deserialize)]
struct PortMapping {
    #[serde(rename = "HostIP", default)]
    host_ip: String,
    #[serde(rename = "Value")]
    value: u16,
}

impl Allocation {
    fn reach(&self) -> Reach {
        let own = self.network_status.as_ref().and_then(|n| n.address.parse().ok());
        let mut ports = Vec::new();
        if let Some(r) = &self.resources {
            let shared = r.shared.iter().flat_map(|s| s.networks.iter().flatten());
            let tasks = r.tasks.iter().flat_map(|t| t.values()).flat_map(|t| t.networks.iter().flatten());
            for n in shared.chain(tasks) {
                if let Ok(ip) = n.ip.parse() {
                    ports.extend(n.reserved.iter().chain(&n.dynamic).flatten().map(|p| (ip, p.value)));
                }
            }
            for m in r.shared.iter().flat_map(|s| s.ports.iter().flatten()) {
                if let Ok(ip) = m.host_ip.parse() {
                    ports.push((ip, m.value));
                }
            }
        }
        Reach { own, ports }
    }

    /// Each task's log patterns: its `atalaya.log_regex`, or its group's, or
    /// its job's, compiled once per node (`compiled`)
    fn log_regex(&self, compiled: &mut HashMap<String, Arc<Regex>>) -> HashMap<String, Vec<Arc<Regex>>> {
        let mut out = HashMap::new();
        let Some(job) = &self.job else { return out };
        let Some(group) = job.groups.iter().flatten().find(|g| g.name == self.task_group) else {
            return out;
        };
        for task in group.tasks.iter().flatten() {
            let Some(src) = [&task.meta, &group.meta, &job.meta].into_iter().flatten().find_map(|m| m.get(LOG_REGEX)) else {
                continue;
            };
            let who = format!("{}/{}", self.job_id, task.name);
            let lines = src.lines().map(str::trim).filter(|l| !l.is_empty());
            let patterns: Vec<_> = lines.take(MAX_PATTERNS).filter_map(|l| pattern(compiled, l, &who)).collect();
            if !patterns.is_empty() {
                out.insert(task.name.clone(), patterns);
            }
        }
        out
    }
}

/// A compiled log pattern, shared with every allocation that uses the same
/// text. None (and a warning) if it does not compile or the node already has
/// `MAX_NODE_PATTERNS` others in use.
fn pattern(compiled: &mut HashMap<String, Arc<Regex>>, src: &str, task: &str) -> Option<Arc<Regex>> {
    if let Some(re) = compiled.get(src) {
        return Some(re.clone());
    }
    // Those no allocation holds any more are forgotten
    compiled.retain(|_, re| Arc::strong_count(re) > 1);
    if compiled.len() >= MAX_NODE_PATTERNS {
        tracing::warn!("nomad: {task}: ignoring a {LOG_REGEX} pattern: {MAX_NODE_PATTERNS} already in use on this node");
        return None;
    }
    match RegexBuilder::new(src).size_limit(PATTERN_SIZE).dfa_size_limit(PATTERN_SIZE).build() {
        Ok(re) => {
            let re = Arc::new(re);
            compiled.insert(src.to_owned(), re.clone());
            Some(re)
        }
        Err(e) => {
            tracing::warn!("nomad: {task}: ignoring a {LOG_REGEX} pattern: {e}");
            None
        }
    }
}

/// The target a registration describes, if it points at its own allocation
fn target(r: Registration, reach: &Reach) -> std::result::Result<Target, String> {
    let path = r.tags.iter().find_map(|t| t.strip_prefix("metrics_path=")).unwrap_or("/metrics");
    // A path, never an authority: `@host/` after the port would make the
    // address a user name and send the scrape to `host`
    if !path.starts_with('/') {
        return Err(format!("metrics_path {path:?} does not start with /"));
    }
    let ip: IpAddr = r.address.parse().map_err(|_| format!("address {:?} is not an IP", r.address))?;
    if !reach.allows(ip, r.port) {
        return Err(format!("{} is not an address of its allocation", SocketAddr::new(ip, r.port)));
    }
    Ok(Target {
        job: r.job_id,
        service: r.service,
        url: format!("http://{}{path}", SocketAddr::new(ip, r.port)),
        alloc: r.alloc_id,
    })
}

/// Allocation details by id, with when they were last asked for and, for one
/// still starting, when to read them again
type Cache = HashMap<String, (AllocInfo, Instant, Option<Instant>)>;

pub struct Nomad {
    http: reqwest::Client,
    addr: String,
    node_id: String,
    allocs: Mutex<Cache>,
    /// Registrations already reported as not scraped: once each
    refused: Mutex<HashSet<String>>,
    /// Compiled log patterns by their text, for every allocation on the node
    patterns: Mutex<HashMap<String, Arc<Regex>>>,
}

/// An allocation not asked about for this long is forgotten: it has stopped
const ALLOC_TTL: Duration = Duration::from_secs(3600);

/// A starting allocation may not have reported its network yet: its details
/// are read again after this long, until it runs
const PENDING_TTL: Duration = Duration::from_secs(10);

/// Stores an allocation's details and forgets those nobody asked about within
/// `ALLOC_TTL`; without this every allocation the node ever ran stays in memory
fn remember(allocs: &mut Cache, id: &str, info: AllocInfo, until: Option<Instant>, now: Instant) {
    allocs.retain(|_, (_, used, _)| now.duration_since(*used) <= ALLOC_TTL);
    allocs.insert(id.to_owned(), (info, now, until));
}

impl Nomad {
    /// With the cluster's CA (an https addr) and an ACL token, if any
    pub fn new(addr: String, node_id: String, ca: Option<reqwest::Certificate>, token: Option<&str>) -> Result<Self> {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if let Some(ca) = ca {
            b = b.add_root_certificate(ca);
        }
        if let Some(t) = token {
            let mut h = reqwest::header::HeaderMap::new();
            let mut v = reqwest::header::HeaderValue::from_str(t.trim())?;
            v.set_sensitive(true);
            h.insert("X-Nomad-Token", v);
            b = b.default_headers(h);
        }
        Ok(Self {
            http: b.build()?,
            addr: addr.trim_end_matches('/').to_owned(),
            node_id,
            allocs: Mutex::new(HashMap::new()),
            refused: Mutex::new(HashSet::new()),
            patterns: Mutex::new(HashMap::new()),
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let r = self.http.get(format!("{}{path}", self.addr)).send().await?.error_for_status()?;
        Ok(r.json().await?)
    }

    /// This node's services tagged `metrics=true`, at an address of their own
    /// allocation. The path can be changed with the tag `metrics_path=/other`.
    pub async fn targets(&self) -> Result<Vec<Target>> {
        let lists: Vec<ServiceList> = self.get("/v1/services").await?;
        let mut out = Vec::new();
        // Collect the names first: an iterator with closures alive across awaits
        // makes the future non-Send for tokio::spawn.
        let names: Vec<String> = lists.into_iter().flat_map(|l| l.services).map(|s| s.name).collect();
        for name in names {
            let regs: Vec<Registration> = self.get(&format!("/v1/service/{name}")).await?;
            for r in regs {
                if r.node_id != self.node_id || !r.tags.iter().any(|t| t == "metrics=true") {
                    continue;
                }
                let key = format!("{}/{}", r.alloc_id, r.service);
                let found = match self.alloc(&r.alloc_id).await {
                    Some(info) => target(r, &info.reach),
                    None => Err("its allocation could not be read".to_owned()),
                };
                match found {
                    Ok(t) => out.push(t),
                    Err(why) => self.refuse(key, &why),
                }
            }
        }
        Ok(out)
    }

    /// Reports a registration that is not scraped, once (the set starts over
    /// past a thousand, so stopped allocations do not pile up)
    fn refuse(&self, key: String, why: &str) {
        let mut refused = self.refused.lock().unwrap();
        if refused.len() > 1000 {
            refused.clear();
        }
        if refused.insert(key.clone()) {
            tracing::warn!("nomad: not scraping {key}: {why}");
        }
    }

    pub async fn alloc(&self, id: &str) -> Option<AllocInfo> {
        self.alloc_at(id, Instant::now()).await
    }

    async fn alloc_at(&self, id: &str, now: Instant) -> Option<AllocInfo> {
        if let Some((a, used, until)) = self.allocs.lock().unwrap().get_mut(id)
            && until.is_none_or(|u| now < u)
        {
            *used = now;
            return Some(a.clone());
        }
        let a: Allocation = self.get(&format!("/v1/allocation/{id}")).await.ok()?;
        let index = a.name.rsplit_once('[').and_then(|(_, i)| i.strip_suffix(']')).unwrap_or("0").to_owned();
        let reach = a.reach();
        let until = (a.client_status == "pending").then(|| now + PENDING_TTL);
        let log_regex = Arc::new(a.log_regex(&mut self.patterns.lock().unwrap()));
        let info = AllocInfo {
            job: a.job_id,
            group: a.task_group,
            namespace: a.namespace,
            index,
            reach,
            log_regex,
        };
        remember(&mut self.allocs.lock().unwrap(), id, info.clone(), until, now);
        Some(info)
    }
}

/// A PEM CA certificate from a file
pub fn ca_cert(path: &std::path::Path) -> Result<reqwest::Certificate> {
    let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(reqwest::Certificate::from_pem(&pem)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(job: &str) -> AllocInfo {
        AllocInfo {
            job: job.into(),
            group: "g".into(),
            namespace: "default".into(),
            index: "0".into(),
            ..Default::default()
        }
    }

    fn registration(address: &str, port: u16, tags: &[&str]) -> Registration {
        Registration {
            node_id: "n".into(),
            address: address.into(),
            port,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            job_id: "job".into(),
            alloc_id: "a1".into(),
            service: "svc".into(),
        }
    }

    #[test]
    fn only_an_address_of_its_own_allocation_is_scraped() {
        // As the API returns them: a CNI allocation (address_mode = alloc) and
        // one on the host network with a reserved port
        let cni: Allocation = serde_json::from_str(
            r#"{"JobID":"web","TaskGroup":"web","Namespace":"default","Name":"web.web[0]",
                "NetworkStatus":{"InterfaceName":"eth0","Address":"10.10.100.2"},
                "AllocatedResources":{"Shared":{"Networks":[{"Mode":"cni/containers","IP":"192.168.0.201","ReservedPorts":null,"DynamicPorts":null}],"Ports":null},"Tasks":{}}}"#,
        )
        .unwrap();
        let host: Allocation = serde_json::from_str(
            r#"{"JobID":"node-exporter","TaskGroup":"ne","Namespace":"default","Name":"node-exporter.ne[0]",
                "NetworkStatus":null,
                "AllocatedResources":{"Shared":{"Networks":[{"Mode":"host","IP":"192.168.0.203","ReservedPorts":[{"Label":"metrics","Value":9100,"To":0}],"DynamicPorts":null}],
                    "Ports":[{"Label":"metrics","Value":9100,"To":0,"HostIP":"192.168.0.203"}]}}}"#,
        )
        .unwrap();
        let (cni, host) = (cni.reach(), host.reach());

        let t = target(registration("10.10.100.2", 8090, &["metrics=true"]), &cni).unwrap();
        assert_eq!(t.url, "http://10.10.100.2:8090/metrics", "any port of its own address");
        let t = target(registration("192.168.0.203", 9100, &["metrics=true", "metrics_path=/m"]), &host).unwrap();
        assert_eq!(t.url, "http://192.168.0.203:9100/m");

        // Somewhere else: another host, the host itself on a port it was not
        // given, or the node's address for a CNI allocation
        for (addr, port, reach) in [("169.254.169.254", 80, &cni), ("192.168.0.203", 4646, &host), ("192.168.0.201", 8090, &cni)] {
            assert!(target(registration(addr, port, &[]), reach).is_err(), "{addr}:{port}");
        }
        // A metrics_path that would move the scrape to another host, or a name for an address
        let err = target(registration("10.10.100.2", 8090, &["metrics_path=@evil.example/x"]), &cni).unwrap_err();
        assert!(err.contains("metrics_path"), "{err}");
        assert!(target(registration("localhost", 8090, &[]), &cni).is_err());
        // IPv6 gets its brackets
        let v6 = Reach {
            own: Some("fd00::2".parse().unwrap()),
            ports: Vec::new(),
        };
        assert_eq!(target(registration("fd00::2", 9100, &[]), &v6).unwrap().url, "http://[fd00::2]:9100/metrics");
    }

    #[tokio::test]
    async fn a_starting_allocation_is_read_again_until_it_runs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Nomad answers the first read before the allocation reported its network
        let answers = [
            r#"{"JobID":"dns","TaskGroup":"g","Namespace":"default","Name":"dns.g[0]","ClientStatus":"pending","NetworkStatus":null}"#,
            r#"{"JobID":"dns","TaskGroup":"g","Namespace":"default","Name":"dns.g[0]","ClientStatus":"running","NetworkStatus":{"Address":"10.10.100.53"}}"#,
        ];
        let reads = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = reads.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let body = answers[served.fetch_add(1, Ordering::SeqCst).min(1)];
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(body.as_bytes()).await;
            }
        });
        let nomad = Nomad::new(format!("http://{addr}"), "n".into(), None, None).unwrap();
        let (ip, start) = ("10.10.100.53".parse().unwrap(), Instant::now());
        let reach = async |secs| nomad.alloc_at("a1", start + Duration::from_secs(secs)).await.unwrap().reach.allows(ip, 9167);

        assert!(!reach(0).await, "no address yet");
        assert!(!reach(1).await, "not read again right away");
        assert!(reach(PENDING_TTL.as_secs() + 1).await, "read again once running");
        assert!(reach(3600).await);
        assert_eq!(reads.load(Ordering::SeqCst), 2, "a running allocation stays cached");
    }

    #[test]
    fn log_patterns_come_from_the_task_its_group_or_its_job() {
        let mut compiled = HashMap::new();
        let a: Allocation = serde_json::from_str(
            r#"{"JobID":"dns","TaskGroup":"unbound","Namespace":"default","Name":"dns.unbound[0]",
                "Job":{"Meta":{"atalaya.log_regex":"job: (?P<j>.*)"},"TaskGroups":[
                  {"Name":"other","Meta":{"atalaya.log_regex":"other"},"Tasks":[{"Name":"unbound","Meta":null}]},
                  {"Name":"unbound","Meta":{"atalaya.log_regex":"group: (?P<g>.*)"},"Tasks":[
                    {"Name":"unbound","Meta":{"atalaya.log_regex":"reply: (?P<client>\\S+)\n\n  rpz: (?P<rpz>\\S+)  \n(unclosed"}},
                    {"Name":"exporter","Meta":null}]}]}}"#,
        )
        .unwrap();
        let got = a.log_regex(&mut compiled);
        let src = |task: &str| got[task].iter().map(|re| re.as_str()).collect::<Vec<_>>();
        assert_eq!(
            src("unbound"),
            [r"reply: (?P<client>\S+)", r"rpz: (?P<rpz>\S+)"],
            "its own, without the one that does not compile"
        );
        assert_eq!(src("exporter"), ["group: (?P<g>.*)"], "its group's");
        assert_eq!(got.len(), 2);

        let job: Allocation = serde_json::from_str(
            r#"{"JobID":"dns","TaskGroup":"plain","Namespace":"default","Name":"dns.plain[0]",
                "Job":{"Meta":{"atalaya.log_regex":"job: (?P<j>.*)"},"TaskGroups":[{"Name":"plain","Tasks":[{"Name":"t"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(job.log_regex(&mut compiled)["t"][0].as_str(), "job: (?P<j>.*)", "its job's");
        let none: Allocation = serde_json::from_str(r#"{"JobID":"web","TaskGroup":"web","Namespace":"default","Name":"web.web[0]"}"#).unwrap();
        assert!(none.log_regex(&mut compiled).is_empty());
    }

    #[test]
    fn log_patterns_are_bounded_per_task_and_per_node() {
        let alloc = |from: usize, n: usize| -> Allocation {
            let src = (from..from + n).map(|i| format!("p{i}")).collect::<Vec<_>>().join("\n");
            serde_json::from_value(serde_json::json!({
                "JobID": "j", "TaskGroup": "g", "Namespace": "default", "Name": "j.g[0]",
                "Job": {"TaskGroups": [{"Name": "g", "Tasks": [{"Name": "t", "Meta": {"atalaya.log_regex": src}}]}]},
            }))
            .unwrap()
        };
        let mut compiled = HashMap::new();
        let a = alloc(0, MAX_PATTERNS + 2).log_regex(&mut compiled);
        assert_eq!(a["t"].len(), MAX_PATTERNS, "per task");
        let again = alloc(0, 1).log_regex(&mut compiled);
        assert!(Arc::ptr_eq(&a["t"][0], &again["t"][0]), "the same text is compiled once");
        let b = alloc(100, MAX_NODE_PATTERNS - MAX_PATTERNS).log_regex(&mut compiled);
        assert_eq!(b["t"].len(), MAX_NODE_PATTERNS - MAX_PATTERNS);
        assert!(alloc(200, 1).log_regex(&mut compiled).is_empty(), "the node is full");
        drop((a, again));
        assert_eq!(alloc(200, 1).log_regex(&mut compiled)["t"].len(), 1, "patterns nobody holds are forgotten");
    }

    #[test]
    fn allocations_nobody_asks_about_are_forgotten() {
        let start = Instant::now();
        let mut allocs = HashMap::new();
        remember(&mut allocs, "a", info("old"), None, start);
        remember(&mut allocs, "b", info("live"), None, start + ALLOC_TTL);
        assert_eq!(allocs.len(), 2, "both within the TTL");
        remember(&mut allocs, "c", info("new"), None, start + ALLOC_TTL + Duration::from_secs(1));
        let mut ids: Vec<&str> = allocs.keys().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, ["b", "c"]);
    }
}
