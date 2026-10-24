// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Disk health via SMART: the collector runs `smartctl --json` (shipped in the
//! image) on every host disk and turns the output into `host` service metrics.
//! Needs a privileged container (SG_IO for ATA, admin ioctl for NVMe).
//!
//! With `-n standby` a sleeping disk is not woken up: that pass only records
//! host_disk_standby = 1. One pass per hour by default (host.smart.interval):
//! queries use last_over_time(…[2h]).
//!
//! Every disk series carries device (sda, nvme0), model, serial (the stable
//! identity: sdX can change between boots) and kind (hdd/ssd/nvme).

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::prom::{Kind, Labels, Scrape};

/// A host disk as seen in sysfs
#[derive(Debug, PartialEq)]
pub struct Disk {
    /// Name used in labels (sda, nvme0)
    pub name: String,
    pub path: String,
    /// queue/rotational (USB bridges usually report 1 even for an SSD)
    pub rotational: Option<bool>,
}

/// Host disks. NVMe drives are read through the controller (nvme0), not the
/// namespace; with native multipath the namespace hangs off the subsystem
/// (nvme-subsys1) and the controller is inside it.
pub fn devices(sys: &Path) -> Vec<Disk> {
    let mut out: Vec<Disk> = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys.join("block")) else { return out };
    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let is_ctrl = |c: &str| c.strip_prefix("nvme").is_some_and(|n| !n.is_empty() && n.chars().all(|x| x.is_ascii_digit()));
    for n in names {
        if ["loop", "zram", "ram", "sr", "dm-", "md", "nbd", "fd", "mmcblk"]
            .iter()
            .any(|p| n.starts_with(p))
        {
            continue;
        }
        let block = sys.join("block").join(&n);
        let dev = if n.starts_with("nvme") {
            let target = std::fs::canonicalize(block.join("device")).ok();
            let base = target
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_ctrl(&base) {
                base
            } else if let Some(ctrl) = target.as_ref().filter(|_| base.starts_with("nvme-subsys")).and_then(|t| {
                let mut c: Vec<String> = std::fs::read_dir(t)
                    .ok()?
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|c| is_ctrl(c))
                    .collect();
                c.sort();
                c.into_iter().next()
            }) {
                ctrl
            } else {
                continue;
            }
        } else {
            n.clone()
        };
        if out.iter().any(|d| d.name == dev) {
            continue;
        }
        let rotational = std::fs::read_to_string(block.join("queue/rotational")).ok().map(|s| s.trim() == "1");
        out.push(Disk {
            path: format!("/dev/{dev}"),
            name: dev,
            rotational,
        });
    }
    out
}

/// Runs smartctl on a disk. None if it could not run or did not return JSON.
/// On ATA the device statistics (devstat) are added: they carry the standard
/// wear indicator of SATA SSDs.
pub async fn run(smartctl: &str, path: &str) -> Option<Value> {
    let mut args = vec!["--json=c", "-a", "-n", "standby"];
    if !path.starts_with("/dev/nvme") {
        args.extend(["-l", "devstat"]);
    }
    args.push(path);
    let fut = tokio::process::Command::new(smartctl).args(&args).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Ok(Ok(o)) => serde_json::from_slice(&o.stdout).ok(),
        Ok(Err(e)) => {
            tracing::warn!("running {smartctl}: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("{smartctl} {path}: no response within 60 s");
            None
        }
    }
}

/// The readable raw value of an ATA attribute: "35 (Min/Max 14/49)" → 35,
/// "309 (Average 283)" → 309; if the string does not start with a clean number
/// ("0/7"), the integer raw value reported by smartctl.
fn ata_raw(raw: &Value) -> Option<f64> {
    let s = raw["string"].as_str().unwrap_or("");
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = &s[digits.len()..];
    if !digits.is_empty() && (rest.is_empty() || rest.starts_with(' ') || rest.starts_with('h')) {
        return digits.parse().ok();
    }
    raw["value"].as_f64()
}

/// Turns one disk's smartctl output into metrics
pub fn parse(device: &str, rotational: Option<bool>, v: &Value, o: &mut Scrape) {
    let standby = v["smartctl"]["messages"].as_array().is_some_and(|m| {
        m.iter()
            .any(|x| x["string"].as_str().is_some_and(|t| t.contains("STANDBY") || t.contains("SLEEP")))
    });
    let base: Labels = [("device".to_owned(), device.to_owned())].into();
    o.put(
        "host_disk_standby",
        Kind::Gauge,
        "1",
        "1 when the disk was asleep (standby) and SMART was not read to avoid waking it.",
        base.clone(),
        if standby { 1.0 } else { 0.0 },
    );
    let nvme = v["device"]["protocol"].as_str() == Some("NVMe");
    let has_data = v["smart_status"].is_object() || v["nvme_smart_health_information_log"].is_object() || v["ata_smart_attributes"].is_object();
    o.put(
        "host_disk_smart_up",
        Kind::Gauge,
        "1",
        "1 when smartctl returned SMART data for the disk.",
        base,
        if has_data { 1.0 } else { 0.0 },
    );
    if standby || !has_data {
        return;
    }
    // rotation_rate: rpm, 0 = SSD; old disks do not report it and then sysfs
    // decides (unreliable over USB: bridges always report rotational)
    let kind = if nvme {
        "nvme"
    } else {
        match v["rotation_rate"].as_f64() {
            Some(r) if r > 0.0 => "hdd",
            Some(_) => "ssd",
            None if rotational == Some(true) => "hdd",
            None => "ssd",
        }
    };
    let labels: Labels = [
        ("device", device),
        ("model", v["model_name"].as_str().unwrap_or("")),
        ("serial", v["serial_number"].as_str().unwrap_or("")),
        ("kind", kind),
    ]
    .iter()
    .map(|(k, x)| ((*k).to_owned(), (*x).to_owned()))
    .collect();
    let with = |extra: &[(&str, String)]| {
        let mut l = labels.clone();
        l.extend(extra.iter().map(|(k, x)| ((*k).to_owned(), x.clone())));
        l
    };
    if let Some(p) = v["smart_status"]["passed"].as_bool() {
        o.put(
            "host_disk_smart_healthy",
            Kind::Gauge,
            "1",
            "SMART overall health: 1 passed, 0 failing.",
            with(&[]),
            if p { 1.0 } else { 0.0 },
        );
    }
    if let Some(t) = v["temperature"]["current"].as_f64() {
        o.put(
            "host_disk_temperature_celsius",
            Kind::Gauge,
            "°C",
            "Disk temperature reported by SMART.",
            with(&[]),
            t,
        );
    }
    if let Some(h) = v["power_on_time"]["hours"].as_f64() {
        o.put(
            "host_disk_power_on_seconds_total",
            Kind::Cumulative,
            "s",
            "Time the disk has been powered on.",
            with(&[]),
            h * 3600.0,
        );
    }
    if let Some(n) = v["power_cycle_count"].as_f64() {
        o.put("host_disk_power_cycles_total", Kind::Cumulative, "1", "Disk power-on cycles.", with(&[]), n);
    }
    if let Some(b) = v["user_capacity"]["bytes"].as_f64().or_else(|| v["nvme_total_capacity"].as_f64()) {
        o.put("host_disk_capacity_bytes", Kind::Gauge, "By", "Disk capacity.", with(&[]), b);
    }
    // Wear: the standard indicator (ATA devstat, NVMe percentage_used) takes
    // precedence over smartctl 7.5's endurance_used, which on some models is
    // derived from misinterpreted vendor attributes (Kingston A400: 71 % vs 28 %)
    let devstat = v["ata_device_statistics"]["pages"].as_array().and_then(|pages| {
        pages
            .iter()
            .flat_map(|p| p["table"].as_array().into_iter().flatten())
            .find(|e| e["name"].as_str() == Some("Percentage Used Endurance Indicator"))?["value"]
            .as_f64()
    });
    let endurance = devstat
        .or_else(|| v["nvme_smart_health_information_log"]["percentage_used"].as_f64())
        .or_else(|| v["endurance_used"]["current_percent"].as_f64());
    if let Some(e) = endurance {
        o.put(
            "host_disk_endurance_used_ratio",
            Kind::Gauge,
            "1",
            "Share of the rated write endurance already used (SSD/NVMe); can exceed 1.",
            with(&[]),
            e / 100.0,
        );
    }
    if let Some(table) = v["ata_smart_attributes"]["table"].as_array() {
        for a in table {
            let (Some(id), Some(name)) = (a["id"].as_u64(), a["name"].as_str()) else {
                continue;
            };
            let l = with(&[("id", id.to_string()), ("name", name.to_owned())]);
            if let Some(r) = ata_raw(&a["raw"]) {
                o.put(
                    "host_disk_ata_attribute_raw",
                    Kind::Gauge,
                    "1",
                    "Raw value of an ATA SMART attribute (vendor units: sectors, hours, counts…).",
                    l.clone(),
                    r,
                );
            }
            // The normalized value only means something against its threshold
            if let (Some(val), Some(th)) = (a["value"].as_f64(), a["thresh"].as_f64())
                && th > 0.0
            {
                o.put(
                    "host_disk_ata_attribute_value",
                    Kind::Gauge,
                    "1",
                    "Normalized value of an ATA SMART attribute (fails at or below its threshold).",
                    l.clone(),
                    val,
                );
                o.put(
                    "host_disk_ata_attribute_threshold",
                    Kind::Gauge,
                    "1",
                    "Failure threshold of an ATA SMART attribute.",
                    l,
                    th,
                );
            }
        }
    }
    let n = &v["nvme_smart_health_information_log"];
    if n.is_object() {
        let g = |o: &mut Scrape, m: &str, unit: &str, help: &str, x: Option<f64>| {
            if let Some(x) = x {
                o.put(m, Kind::Gauge, unit, help, labels.clone(), x);
            }
        };
        let c = |o: &mut Scrape, m: &str, unit: &str, help: &str, x: Option<f64>| {
            if let Some(x) = x {
                o.put(m, Kind::Cumulative, unit, help, labels.clone(), x);
            }
        };
        g(
            o,
            "host_nvme_critical_warning",
            "1",
            "NVMe critical warning bits (0 = none).",
            n["critical_warning"].as_f64(),
        );
        g(
            o,
            "host_nvme_available_spare_ratio",
            "1",
            "NVMe spare capacity left.",
            n["available_spare"].as_f64().map(|x| x / 100.0),
        );
        g(
            o,
            "host_nvme_available_spare_threshold_ratio",
            "1",
            "NVMe spare capacity below which the drive warns.",
            n["available_spare_threshold"].as_f64().map(|x| x / 100.0),
        );
        // Data units of 1000 512-byte sectors
        c(
            o,
            "host_nvme_data_read_bytes_total",
            "By",
            "Bytes read by the host from the NVMe drive.",
            n["data_units_read"].as_f64().map(|x| x * 512_000.0),
        );
        c(
            o,
            "host_nvme_data_written_bytes_total",
            "By",
            "Bytes written by the host to the NVMe drive.",
            n["data_units_written"].as_f64().map(|x| x * 512_000.0),
        );
        c(
            o,
            "host_nvme_unsafe_shutdowns_total",
            "1",
            "NVMe shutdowns without a clean power-off notification.",
            n["unsafe_shutdowns"].as_f64(),
        );
        c(
            o,
            "host_nvme_media_errors_total",
            "1",
            "NVMe unrecovered data integrity errors.",
            n["media_errors"].as_f64(),
        );
        c(
            o,
            "host_nvme_error_log_entries_total",
            "1",
            "NVMe error log entries over the drive's life.",
            n["num_err_log_entries"].as_f64(),
        );
        c(
            o,
            "host_nvme_warning_temperature_seconds_total",
            "s",
            "Time the NVMe drive spent above its warning temperature.",
            n["warning_temp_time"].as_f64().map(|x| x * 60.0),
        );
        c(
            o,
            "host_nvme_critical_temperature_seconds_total",
            "s",
            "Time the NVMe drive spent above its critical temperature.",
            n["critical_comp_time"].as_f64().map(|x| x * 60.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prom::Scalar;

    fn get<'a>(s: &'a Scrape, metric: &str, extra: Option<(&str, &str)>) -> Option<&'a Scalar> {
        s.scalars
            .iter()
            .find(|x| x.metric == metric && extra.is_none_or(|(k, v)| x.labels.get(k).map(String::as_str) == Some(v)))
    }

    #[test]
    fn old_hdd_without_rotation_rate_uses_sysfs() {
        let v = serde_json::json!({"smart_status": {"passed": true}, "temperature": {"current": 35}});
        let mut s = Scrape::default();
        parse("sdc", Some(true), &v, &mut s);
        assert_eq!(get(&s, "host_disk_temperature_celsius", None).unwrap().labels["kind"], "hdd");
    }

    #[test]
    fn standby_disk_is_not_read() {
        let v: Value = serde_json::from_str(r#"{"smartctl":{"exit_status":2,"messages":[{"string":"Device is in STANDBY mode, exit(2)","severity":"information"}]},"device":{"name":"/dev/sdb","protocol":"ATA"}}"#).unwrap();
        let mut s = Scrape::default();
        parse("sdb", Some(true), &v, &mut s);
        assert_eq!(get(&s, "host_disk_standby", None).unwrap().value, 1.0);
        assert_eq!(s.scalars.len(), 2);
    }

    #[test]
    fn lists_disks_from_sysfs() {
        let root = std::env::temp_dir().join(format!("atalaya-smart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let b = root.join("block");
        for d in ["sda", "loop0", "zram0", "sr0", "nvme0n1", "nvme1n1", "nvme1c1n1", "dm-0"] {
            std::fs::create_dir_all(b.join(d)).unwrap();
        }
        std::fs::create_dir_all(b.join("sda/queue")).unwrap();
        std::fs::write(b.join("sda/queue/rotational"), "1\n").unwrap();
        // nvme0 direct; nvme1 with multipath: the namespace hangs off the
        // subsystem, plus the controller's hidden path nvme1c1n1
        std::fs::create_dir_all(root.join("devices/nvme0")).unwrap();
        std::fs::create_dir_all(root.join("devices/nvme1")).unwrap();
        std::fs::create_dir_all(root.join("devices/nvme-subsys1")).unwrap();
        std::os::unix::fs::symlink(root.join("devices/nvme1"), root.join("devices/nvme-subsys1/nvme1")).unwrap();
        std::os::unix::fs::symlink(root.join("devices/nvme0"), b.join("nvme0n1/device")).unwrap();
        std::os::unix::fs::symlink(root.join("devices/nvme-subsys1"), b.join("nvme1n1/device")).unwrap();
        std::os::unix::fs::symlink(root.join("devices/nvme1"), b.join("nvme1c1n1/device")).unwrap();
        let d = devices(&root);
        assert_eq!(d.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["nvme0", "nvme1", "sda"]);
        assert_eq!(
            d[2],
            Disk {
                name: "sda".into(),
                path: "/dev/sda".into(),
                rotational: Some(true)
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
