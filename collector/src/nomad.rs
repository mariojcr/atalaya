// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Nomad API queries: this node's services that expose metrics and the details
//! of each allocation (job, group, index), cached.
//!
//! A service registration's address and port are the job's to choose, and the
//! collector scrapes from the host's network: a target is only scraped at an
//! address of its own allocation (its address in its network namespace, or a
//! host port it was given), never wherever a job points it.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
}

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
    #[serde(rename = "NetworkStatus", default)]
    network_status: Option<NetworkStatus>,
    #[serde(rename = "AllocatedResources", default)]
    resources: Option<AllocatedResources>,
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

pub struct Nomad {
    http: reqwest::Client,
    addr: String,
    node_id: String,
    /// Allocation details by id, with when they were last asked for
    allocs: Mutex<HashMap<String, (AllocInfo, Instant)>>,
    /// Registrations already reported as not scraped: once each
    refused: Mutex<HashSet<String>>,
}

/// An allocation not asked about for this long is forgotten: it has stopped
const ALLOC_TTL: Duration = Duration::from_secs(3600);

/// Stores an allocation's details and forgets those nobody asked about within
/// `ALLOC_TTL`; without this every allocation the node ever ran stays in memory
fn remember(allocs: &mut HashMap<String, (AllocInfo, Instant)>, id: &str, info: AllocInfo, now: Instant) {
    allocs.retain(|_, (_, used)| now.duration_since(*used) <= ALLOC_TTL);
    allocs.insert(id.to_owned(), (info, now));
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
        if let Some((a, used)) = self.allocs.lock().unwrap().get_mut(id) {
            *used = Instant::now();
            return Some(a.clone());
        }
        let a: Allocation = self.get(&format!("/v1/allocation/{id}")).await.ok()?;
        let index = a.name.rsplit_once('[').and_then(|(_, i)| i.strip_suffix(']')).unwrap_or("0").to_owned();
        let reach = a.reach();
        let info = AllocInfo {
            job: a.job_id,
            group: a.task_group,
            namespace: a.namespace,
            index,
            reach,
        };
        remember(&mut self.allocs.lock().unwrap(), id, info.clone(), Instant::now());
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
            reach: Reach::default(),
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

    #[test]
    fn allocations_nobody_asks_about_are_forgotten() {
        let start = Instant::now();
        let mut allocs = HashMap::new();
        remember(&mut allocs, "a", info("old"), start);
        remember(&mut allocs, "b", info("live"), start + ALLOC_TTL);
        assert_eq!(allocs.len(), 2, "both within the TTL");
        remember(&mut allocs, "c", info("new"), start + ALLOC_TTL + Duration::from_secs(1));
        let mut ids: Vec<&str> = allocs.keys().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, ["b", "c"]);
    }
}
