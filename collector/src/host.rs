// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Metrics of the host itself read from sysfs: the collector as an exporter of
//! what no service publishes. Service `host`, resource host.name.
//!
//! - hwmon: temperatures, fans, voltages, currents, power, frequencies and
//!   energy of every chip (CPU, NVMe, memory, motherboard, GPU, NIC)
//! - RAPL: accumulated energy per domain (package, cores, uncore, DRAM)
//! - GPU (amdgpu): utilisation and VRAM/GTT memory
//! - power supplies: connected mains/USB adapters and battery
//! - EDAC: corrected and uncorrected memory errors
//!
//! Power supplies with their own hwmon (battery, charger) are read only through
//! power_supply so they are not duplicated. Temperatures, fans, voltages and
//! currents that have never reported anything but 0 are unconnected inputs:
//! they are not emitted until they report a value, and always after that (a
//! stopped fan must be visible).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::prom::{Descriptor, Kind, Labels, Scalar, Scrape};

pub const SERVICE: &str = "host";

pub struct Host {
    sys: PathBuf,
    /// Inputs (fan/voltage/current) that have ever reported ≠ 0
    seen: HashSet<String>,
    /// RAPL: last raw reading (µJ) and accumulated total in joules
    rapl: HashMap<String, (u64, f64)>,
}

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}
fn num(p: &Path) -> Option<f64> {
    read(p)?.parse().ok()
}
fn base(p: &Path) -> Option<String> {
    fs::canonicalize(p).ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
}
fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}
fn lbl(pairs: &[(&str, &str)]) -> Labels {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

struct Out(Scrape);
impl Out {
    fn put(&mut self, metric: &str, kind: Kind, unit: &str, help: &str, labels: Labels, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.0.descriptors.entry(metric.to_owned()).or_insert_with(|| Descriptor {
            kind,
            unit: unit.to_owned(),
            help: help.to_owned(),
        });
        self.0.scalars.push(Scalar {
            metric: metric.to_owned(),
            labels,
            value,
            counter: kind == Kind::Cumulative,
        });
    }
    fn gauge(&mut self, metric: &str, unit: &str, help: &str, labels: Labels, value: f64) {
        self.put(metric, Kind::Gauge, unit, help, labels, value);
    }
    fn counter(&mut self, metric: &str, unit: &str, help: &str, labels: Labels, value: f64) {
        self.put(metric, Kind::Cumulative, unit, help, labels, value);
    }
}

impl Host {
    /// `root` is where the host's root filesystem is mounted (/host by
    /// default). None if there is no sysfs there (e.g. in development).
    pub fn new(root: &Path) -> Option<Self> {
        let sys = root.join("sys");
        sys.join("class").is_dir().then(|| Self {
            sys,
            seen: HashSet::new(),
            rapl: HashMap::new(),
        })
    }

    pub fn collect(&mut self) -> Scrape {
        let mut o = Out(Scrape::default());
        self.hwmon(&mut o);
        self.rapl(&mut o);
        self.gpu(&mut o);
        self.power_supply(&mut o);
        self.edac(&mut o);
        o.0
    }

    fn hwmon(&mut self, o: &mut Out) {
        for h in entries(&self.sys.join("class/hwmon")) {
            let Some(chip) = read(&h.join("name")) else { continue };
            if base(&h.join("device/subsystem")).as_deref() == Some("power_supply") {
                continue;
            }
            let device = base(&h.join("device")).unwrap_or_else(|| h.file_name().unwrap().to_string_lossy().into_owned());
            // sensor ("temp1") → (kind, raw value, is_average); _average wins over _input
            let mut vals: BTreeMap<String, (String, f64, bool)> = BTreeMap::new();
            for f in entries(&h) {
                let name = f.file_name().unwrap().to_string_lossy().into_owned();
                let Some((sensor, attr)) = name.split_once('_') else { continue };
                let avg = match attr {
                    "input" => false,
                    "average" => true,
                    _ => continue,
                };
                let kind = sensor.trim_end_matches(|c: char| c.is_ascii_digit());
                if kind.len() == sensor.len() || !matches!(kind, "temp" | "fan" | "in" | "curr" | "power" | "freq" | "energy") {
                    continue;
                }
                let Some(v) = num(&f) else { continue };
                if vals.get(sensor).is_some_and(|x| x.2 && !avg) {
                    continue;
                }
                vals.insert(sensor.to_owned(), (kind.to_owned(), v, avg));
            }
            for (sensor, (kind, raw, _)) in vals {
                let label = read(&h.join(format!("{sensor}_label"))).unwrap_or_else(|| sensor.clone());
                let labels = lbl(&[("chip", &chip), ("device", &device), ("sensor", &sensor), ("label", &label)]);
                let key = format!("{chip}|{device}|{sensor}");
                match kind.as_str() {
                    "temp" => {
                        let c = raw / 1000.0;
                        // -40 °C and 127 °C are the "no probe" values of several chips, and
                        // unused Super I/O inputs report 0 °C
                        if c != 0.0 {
                            self.seen.insert(key.clone());
                        }
                        if c > -40.0 && c < 127.0 && self.seen.contains(&key) {
                            o.gauge("host_temperature_celsius", "°C", "Hardware sensor temperature (hwmon).", labels, c);
                        }
                    }
                    "fan" | "in" | "curr" => {
                        if raw != 0.0 {
                            self.seen.insert(key.clone());
                        }
                        if !self.seen.contains(&key) {
                            continue;
                        }
                        match kind.as_str() {
                            "fan" => o.gauge("host_fan_rpm", "rpm", "Fan speed (hwmon).", labels, raw),
                            "in" => o.gauge("host_voltage_volts", "V", "Hardware sensor voltage (hwmon).", labels, raw / 1000.0),
                            _ => o.gauge("host_current_amperes", "A", "Hardware sensor current (hwmon).", labels, raw / 1000.0),
                        }
                    }
                    "power" => o.gauge(
                        "host_power_watts",
                        "W",
                        "Power reported by a hardware sensor, e.g. the GPU package (hwmon).",
                        labels,
                        raw / 1e6,
                    ),
                    "freq" => o.gauge(
                        "host_frequency_hertz",
                        "Hz",
                        "Clock frequency reported by a hardware sensor (hwmon).",
                        labels,
                        raw,
                    ),
                    _ => o.counter(
                        "host_energy_joules_total",
                        "J",
                        "Energy reported by a hardware sensor (hwmon).",
                        labels,
                        raw / 1e6,
                    ),
                }
            }
        }
    }

    /// RAPL energy counters. They wrap at max_energy_range_uj, so they are
    /// accumulated here and the counter only resets when the collector restarts.
    fn rapl(&mut self, o: &mut Out) {
        for z in entries(&self.sys.join("class/powercap")) {
            let id = z.file_name().unwrap().to_string_lossy().into_owned();
            if !id.starts_with("intel-rapl:") {
                continue;
            }
            let (Some(name), Some(raw)) = (read(&z.join("name")), read(&z.join("energy_uj")).and_then(|s| s.parse::<u64>().ok())) else {
                continue;
            };
            let max = read(&z.join("max_energy_range_uj")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let total = match self.rapl.get(&id) {
                None => raw as f64 / 1e6,
                Some(&(last, total)) => {
                    let delta = if raw >= last { raw - last } else { raw + max.saturating_sub(last) };
                    total + delta as f64 / 1e6
                }
            };
            self.rapl.insert(id.clone(), (raw, total));
            o.counter(
                "host_rapl_energy_joules_total",
                "J",
                "Energy consumed by a RAPL domain (CPU package, cores, uncore, DRAM); rate() gives watts.",
                lbl(&[("zone", &name), ("id", &id)]),
                total,
            );
        }
    }

    fn gpu(&mut self, o: &mut Out) {
        for c in entries(&self.sys.join("class/drm")) {
            let card = c.file_name().unwrap().to_string_lossy().into_owned();
            if !card
                .strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|x| x.is_ascii_digit()))
            {
                continue;
            }
            let d = c.join("device");
            let Some(driver) = base(&d.join("driver")) else { continue };
            let device = base(&d).unwrap_or_default();
            let l = |extra: &[(&str, &str)]| {
                let mut v = lbl(&[("card", &card), ("device", &device), ("driver", &driver)]);
                v.extend(lbl(extra));
                v
            };
            if let Some(b) = num(&d.join("gpu_busy_percent")) {
                o.gauge("host_gpu_busy_ratio", "1", "Fraction of time the GPU was busy.", l(&[]), b / 100.0);
            }
            for pool in ["vram", "gtt"] {
                if let Some(v) = num(&d.join(format!("mem_info_{pool}_used"))) {
                    o.gauge(
                        "host_gpu_memory_used_bytes",
                        "By",
                        "GPU memory in use: dedicated (vram) or system memory mapped by the GPU (gtt).",
                        l(&[("pool", pool)]),
                        v,
                    );
                }
                if let Some(v) = num(&d.join(format!("mem_info_{pool}_total"))) {
                    o.gauge(
                        "host_gpu_memory_total_bytes",
                        "By",
                        "GPU memory available: dedicated (vram) or system memory mapped by the GPU (gtt).",
                        l(&[("pool", pool)]),
                        v,
                    );
                }
            }
        }
    }

    fn power_supply(&mut self, o: &mut Out) {
        for p in entries(&self.sys.join("class/power_supply")) {
            let supply = p.file_name().unwrap().to_string_lossy().into_owned();
            let Some(kind) = read(&p.join("type")) else { continue };
            // Peripheral batteries (pen, mouse)
            if read(&p.join("scope")).as_deref() == Some("Device") {
                continue;
            }
            if kind != "Battery" {
                if let Some(v) = num(&p.join("online")) {
                    o.gauge(
                        "host_power_supply_online",
                        "1",
                        "1 when the power supply (mains adapter, USB-C) is connected.",
                        lbl(&[("supply", &supply), ("type", &kind)]),
                        v,
                    );
                }
                continue;
            }
            let l = || lbl(&[("supply", &supply)]);
            if let Some(v) = num(&p.join("capacity")) {
                o.gauge("host_battery_charge_ratio", "1", "Battery charge.", l(), v / 100.0);
            }
            let volts = num(&p.join("voltage_now")).map(|v| v / 1e6);
            // energy_* in µWh; if there is only charge_* (µAh), convert it with the voltage
            let wh = |e: &str, c: &str| num(&p.join(e)).map(|v| v / 1e6).or_else(|| Some(num(&p.join(c))? / 1e6 * volts?));
            for (metric, e, c, help) in [
                ("host_battery_energy_watthours", "energy_now", "charge_now", "Energy stored in the battery."),
                (
                    "host_battery_energy_full_watthours",
                    "energy_full",
                    "charge_full",
                    "Energy the battery holds when full (wears down with age).",
                ),
                (
                    "host_battery_energy_full_design_watthours",
                    "energy_full_design",
                    "charge_full_design",
                    "Energy the battery held when new.",
                ),
            ] {
                if let Some(v) = wh(e, c) {
                    o.gauge(metric, "Wh", help, l(), v);
                }
            }
            let watts = num(&p.join("power_now"))
                .map(|v| v / 1e6)
                .or_else(|| Some(num(&p.join("current_now"))? / 1e6 * volts?));
            if let Some(w) = watts {
                let sign = if read(&p.join("status")).as_deref() == Some("Discharging") {
                    -1.0
                } else {
                    1.0
                };
                o.gauge(
                    "host_battery_power_watts",
                    "W",
                    "Battery power: positive while charging, negative while discharging.",
                    l(),
                    sign * w,
                );
            }
            if let Some(n) = num(&p.join("cycle_count")).filter(|n| *n > 0.0) {
                o.gauge("host_battery_cycles", "1", "Battery charge cycles.", l(), n);
            }
        }
    }

    fn edac(&mut self, o: &mut Out) {
        for mc in entries(&self.sys.join("devices/system/edac/mc")) {
            let controller = mc.file_name().unwrap().to_string_lossy().into_owned();
            if !controller.starts_with("mc") {
                continue;
            }
            for (file, kind) in [("ce_count", "corrected"), ("ue_count", "uncorrected")] {
                if let Some(v) = num(&mc.join(file)) {
                    o.counter(
                        "host_memory_ecc_errors_total",
                        "1",
                        "Memory errors detected by ECC (EDAC).",
                        lbl(&[("controller", &controller), ("type", kind)]),
                        v,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn w(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn find<'a>(s: &'a Scrape, metric: &str, k: &str, v: &str) -> Option<&'a Scalar> {
        s.scalars.iter().find(|x| x.metric == metric && x.labels.get(k).map(String::as_str) == Some(v))
    }

    #[test]
    fn reads_a_fake_sysfs() {
        let root = std::env::temp_dir().join(format!("atalaya-host-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sys = root.join("sys");
        // CPU with a label, a disconnected probe (-40 °C), an unconnected fan
        let cpu = sys.join("devices/platform/coretemp.0");
        w(&cpu.join("hwmon/hwmon1/name"), "coretemp\n");
        w(&cpu.join("hwmon/hwmon1/temp1_input"), "45000\n");
        w(&cpu.join("hwmon/hwmon1/temp1_label"), "Package id 0\n");
        w(&cpu.join("hwmon/hwmon1/temp2_input"), "-40000\n");
        w(&cpu.join("hwmon/hwmon1/temp3_input"), "0\n");
        w(&cpu.join("hwmon/hwmon1/fan1_input"), "0\n");
        w(&cpu.join("hwmon/hwmon1/fan2_input"), "900\n");
        w(&cpu.join("hwmon/hwmon1/power1_input"), "1000000\n");
        w(&cpu.join("hwmon/hwmon1/power1_average"), "2000000\n");
        fs::create_dir_all(sys.join("bus/platform")).unwrap();
        symlink(sys.join("bus/platform"), cpu.join("subsystem")).unwrap();
        symlink(&cpu, cpu.join("hwmon/hwmon1/device")).unwrap();
        fs::create_dir_all(sys.join("class/hwmon")).unwrap();
        symlink(cpu.join("hwmon/hwmon1"), sys.join("class/hwmon/hwmon1")).unwrap();
        // Battery with its own hwmon: only through power_supply
        let bat = sys.join("devices/BAT0");
        w(&bat.join("type"), "Battery\n");
        w(&bat.join("status"), "Discharging\n");
        w(&bat.join("capacity"), "80\n");
        w(&bat.join("power_now"), "10000000\n");
        w(&bat.join("energy_now"), "50000000\n");
        w(&bat.join("hwmon/hwmon2/name"), "BAT0\n");
        w(&bat.join("hwmon/hwmon2/power1_input"), "10000000\n");
        fs::create_dir_all(sys.join("class/power_supply")).unwrap();
        symlink(sys.join("class/power_supply"), bat.join("subsystem")).unwrap();
        symlink(&bat, bat.join("hwmon/hwmon2/device")).unwrap();
        symlink(bat.join("hwmon/hwmon2"), sys.join("class/hwmon/hwmon2")).unwrap();
        symlink(&bat, sys.join("class/power_supply/BAT0")).unwrap();
        let pen = sys.join("devices/pen");
        w(&pen.join("type"), "Battery\n");
        w(&pen.join("scope"), "Device\n");
        w(&pen.join("capacity"), "0\n");
        symlink(&pen, sys.join("class/power_supply/pen")).unwrap();
        // RAPL that wraps around
        let z = sys.join("class/powercap/intel-rapl:0");
        w(&z.join("name"), "package-0\n");
        w(&z.join("max_energy_range_uj"), "1000000000\n");
        w(&z.join("energy_uj"), "999000000\n");
        // GPU
        let g = sys.join("devices/pci/0000:c4:00.0");
        w(&g.join("gpu_busy_percent"), "25\n");
        w(&g.join("mem_info_gtt_used"), "1024\n");
        fs::create_dir_all(sys.join("bus/pci/drivers/amdgpu")).unwrap();
        symlink(sys.join("bus/pci/drivers/amdgpu"), g.join("driver")).unwrap();
        fs::create_dir_all(sys.join("class/drm/card1")).unwrap();
        symlink(&g, sys.join("class/drm/card1/device")).unwrap();
        fs::create_dir_all(sys.join("class/drm/card1-eDP-1")).unwrap();
        // EDAC
        w(&sys.join("devices/system/edac/mc/mc0/ce_count"), "2\n");
        w(&sys.join("devices/system/edac/mc/mc0/ue_count"), "0\n");

        let mut h = Host::new(&root).unwrap();
        let s = h.collect();
        let t = find(&s, "host_temperature_celsius", "label", "Package id 0").unwrap();
        assert_eq!(t.value, 45.0);
        assert_eq!(t.labels["device"], "coretemp.0");
        assert_eq!(s.scalars.iter().filter(|x| x.metric == "host_temperature_celsius").count(), 1);
        assert!(find(&s, "host_fan_rpm", "sensor", "fan1").is_none());
        assert_eq!(find(&s, "host_fan_rpm", "sensor", "fan2").unwrap().value, 900.0);
        assert_eq!(find(&s, "host_power_watts", "sensor", "power1").unwrap().value, 2.0);
        assert!(find(&s, "host_power_watts", "chip", "BAT0").is_none());
        assert_eq!(find(&s, "host_battery_power_watts", "supply", "BAT0").unwrap().value, -10.0);
        assert_eq!(find(&s, "host_battery_charge_ratio", "supply", "BAT0").unwrap().value, 0.8);
        assert_eq!(find(&s, "host_battery_energy_watthours", "supply", "BAT0").unwrap().value, 50.0);
        assert!(find(&s, "host_battery_charge_ratio", "supply", "pen").is_none());
        assert_eq!(find(&s, "host_gpu_busy_ratio", "card", "card1").unwrap().value, 0.25);
        assert_eq!(find(&s, "host_gpu_memory_used_bytes", "pool", "gtt").unwrap().labels["driver"], "amdgpu");
        assert_eq!(s.scalars.iter().filter(|x| x.metric == "host_gpu_busy_ratio").count(), 1);
        assert_eq!(find(&s, "host_memory_ecc_errors_total", "type", "corrected").unwrap().value, 2.0);
        assert_eq!(find(&s, "host_rapl_energy_joules_total", "zone", "package-0").unwrap().value, 999.0);
        // A fan that stops is still emitted; RAPL adds the wrap-around
        w(&cpu.join("hwmon/hwmon1/fan2_input"), "0\n");
        w(&z.join("energy_uj"), "1000000\n");
        let s = h.collect();
        assert_eq!(find(&s, "host_fan_rpm", "sensor", "fan2").unwrap().value, 0.0);
        assert_eq!(find(&s, "host_rapl_energy_joules_total", "zone", "package-0").unwrap().value, 1001.0);
        let _ = fs::remove_dir_all(&root);
    }
}
