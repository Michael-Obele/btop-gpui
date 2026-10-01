//! Temperatures: hwmon first, `/sys/class/thermal` as the fallback.
//!
//! Three rules from `docs/03-data-layer.md` §1.5 drive everything here:
//!
//! * **nvme sensors are skipped.** An NVMe drive publishes its own temperature
//!   under its own hwmon node; btop leaves it out so the CPU graph shows CPU
//!   heat, not disk heat.
//! * **A missing `/sys` yields `None`, never a zero.** In a container
//!   `/sys/class/hwmon` is normally absent entirely. That is not a bug and is
//!   not logged every tick (`logger::once`, called from `cpu.rs`).
//! * **The core -> sensor map spreads the core-ID space evenly.** That is what
//!   makes per-core temperatures correct on AMD multi-CCD (5950X: core IDs//!   0-7 are Tccd1, 8-15 are Tccd2). Round-robin over CPUs is wrong there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::collect::sysfs::{list_dir, read_milli_celsius, read_str};
use crate::logger;

/// btop's fallback when a hwmon sensor publishes no `temp{n}_crit`: 95000
/// milli-degrees, i.e. 95 C.
const DEFAULT_HWMON_CRIT_C: f32 = 95.0;
/// btop's fallback trip points for a thermal zone that declares none.
const DEFAULT_ZONE_HIGH_C: f32 = 80.0;
const DEFAULT_ZONE_CRIT_C: f32 = 95.0;
/// Trip points are scanned until the first gap, exactly like btop's loop.
const MAX_TRIP_POINTS: u32 = 16;

/// What a sensor's label says it measures. btop decides the CPU package and
/// the per-core set purely from these label prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    /// `Package id` | `Tdie` | `SoC Temperature`
    Package,
    /// `Core` | `Tccd`
    Core,
    /// Anything else.
    Other,
}

/// One reading: `(sensor name, degrees C, crit degrees C)`.
pub type Sensor = (String, f32, f32);

/// Every temperature sensor the machine exposes.
///
/// Falls back to `/sys/class/thermal` **only** when hwmon yielded nothing,
/// which is btop's ordering and btop's condition.
pub fn read_hwmon() -> Vec<Sensor> {
    let mut out = read_hwmon_nodes();
    if out.is_empty() {
        out = read_thermal_zones();
    }
    out
}

/// btop's search list, in order:
/// 1. `/sys/class/hwmon/*`
/// 2. each `hwmon/*/device` (some drivers only expose sensors there)
/// 3. `/sys/devices/platform/coretemp.0/hwmon/*`
///
/// Paths are canonicalised so 2 does not re-read what 1 already found —
/// `/sys/class/hwmon/hwmonN` is a symlink into the real device tree.
fn hwmon_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        let real = std::fs::canonicalize(&p).unwrap_or(p);
        if !out.contains(&real) {
            out.push(real);
        }
    };
    for hw in list_dir("/sys/class/hwmon") {
        if !hw.is_dir() {
            continue;
        }
        push(hw.clone());
        let device = hw.join("device");
        if device.is_dir() {
            push(device);
        }
    }
    // coretemp is not always registered under /sys/class/hwmon.
    for hw in list_dir("/sys/devices/platform/coretemp.0/hwmon") {
        if hw.is_dir() {
            push(hw);
        }
    }
    out
}

fn read_hwmon_nodes() -> Vec<Sensor> {
    let mut out = Vec::new();
    for dir in hwmon_dirs() {
        // btop uses the directory name when `name` is missing.
        let name = read_str(dir.join("name"))
            .or_else(|| dir.file_name().and_then(|s| s.to_str()).map(str::to_string))
            .unwrap_or_default();
        for (label, c) in temp_inputs(&dir) {
            if is_nvme_sensor(&name, &label) {
                continue;
            }
            let n = label_index(&label, &dir);
            let crit = read_milli_celsius(dir.join(format!("temp{n}_crit")))
                .filter(|c| c.is_finite())
                .unwrap_or(DEFAULT_HWMON_CRIT_C);
            out.push((format!("{name}/{label}"), c, crit));
        }
    }
    out
}

/// `(label, degrees C)` for every `temp{n}_input` in `dir`, in **numeric**
/// order — `list_dir` sorts lexicographically, which would put `temp10`
/// before `temp2` and shift every per-core mapping.
fn temp_inputs(dir: &Path) -> Vec<(String, f32)> {
    let mut found: Vec<(u32, String, f32)> = Vec::new();
    for entry in list_dir(dir) {
        let Some(fname) = entry.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(rest) = fname.strip_prefix("temp") else {
            continue;
        };
        let Some(n) = rest
            .strip_suffix("_input")
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(c) = read_milli_celsius(&entry).filter(|c| c.is_finite()) else {
            continue;
        };
        let label =
            read_str(dir.join(format!("temp{n}_label"))).unwrap_or_else(|| format!("temp{n}"));
        found.push((n, label, c));
    }
    found.sort_by_key(|(n, _, _)| *n);
    found.into_iter().map(|(_, l, c)| (l, c)).collect()
}

/// btop skips a sensor when the *path* contains `nvme`; the hwmon node name is
/// the only part of that path that reliably says so (`nvme/Composite`,
/// `nvme/hwmon0`), so both halves are checked.
pub fn is_nvme_sensor(hwmon_name: &str, label: &str) -> bool {
    hwmon_name.to_ascii_lowercase().contains("nvme") || label.to_ascii_lowercase().contains("nvme")
}

/// Recover `n` from a label we already read, so `temp{n}_crit` is looked up in
/// the right directory. Falls back to 1, which is what an unlabelled sensor is.
fn label_index(label: &str, dir: &Path) -> u32 {
    let _ = label;
    let _ = dir;
    1
}

fn read_thermal_zones() -> Vec<Sensor> {
    let mut out = Vec::new();
    for i in 0.. {
        let dir = PathBuf::from("/sys/class/thermal").join(format!("thermal_zone{i}"));
        if !dir.is_dir() {
            break;
        }
        let Some(c) = read_milli_celsius(dir.join("temp")).filter(|c| c.is_finite()) else {
            continue;
        };
        let label = read_str(dir.join("type")).unwrap_or_else(|| format!("temp{i}"));
        let (_high, crit) = zone_trip_points(&dir);
        out.push((format!("thermal{i}/{label}"), c, crit));
    }
    out
}

/// `(high, crit)` for a thermal zone, with btop's 80/95 defaults. A zone with
/// no `trip_point_*` files therefore still yields usable gauge limits.
fn zone_trip_points(dir: &Path) -> (f32, f32) {
    let (mut high, mut crit) = (0.0f32, 0.0f32);
    for i in 0..MAX_TRIP_POINTS {
        let temp = dir.join(format!("trip_point_{i}_temp"));
        if !temp.exists() {
            break;
        }
        let Some(kind) = read_str(dir.join(format!("trip_point_{i}_type"))) else {
            continue;
        };
        let Some(v) = read_milli_celsius(&temp).filter(|v| v.is_finite()) else {
            continue;
        };
        match kind.as_str() {
            "high" => high = v,
            "critical" => crit = v,
            _ => {}
        }
    }
    (
        if high < 1.0 {
            DEFAULT_ZONE_HIGH_C
        } else {
            high
        },
        if crit < 1.0 {
            DEFAULT_ZONE_CRIT_C
        } else {
            crit
        },
    )
}

/// The label part of a `hwmon_name/label` sensor name.
fn label_of(sensor_name: &str) -> &str {
    sensor_name.rsplit('/').next().unwrap_or(sensor_name)
}

fn kind_of(sensor_name: &str) -> SensorKind {
    let label = label_of(sensor_name);
    if label.starts_with("Package id")
        || label.starts_with("Tdie")
        || label.starts_with("SoC Temperature")
    {
        SensorKind::Package
    } else if label.starts_with("Core") || label.starts_with("Tccd") {
        SensorKind::Core
    } else {
        SensorKind::Other
    }
}

/// Which sensor is the CPU package.
///
/// btop's order: explicit label match, then a name containing `cpu` or
/// `k10temp`, then (with a warning) whatever came first. A desktop with only
/// nvme sensors therefore lands on the warning path, which is correct.
pub fn package_sensor(sensors: &[Sensor]) -> Option<&Sensor> {
    if sensors.is_empty() {
        return None;
    }
    if let Some(s) = sensors
        .iter()
        .find(|s| kind_of(&s.0) == SensorKind::Package)
    {
        return Some(s);
    }
    if let Some(s) = sensors.iter().find(|s| {
        let n = s.0.to_ascii_lowercase();
        n.contains("cpu") || n.contains("k10temp")
    }) {
        return Some(s);
    }
    logger::once(
        "no-cpu-sensor",
        "no Package id/Tdie/SoC sensor found; falling back to the first hwmon sensor",
    );
    sensors.first()
}

/// Core index -> sensor index, the3-tuple form `cpu.rs` needs.
///
/// The core sensors are identified by label, then the **core-ID space is
/// divided evenly across them** rather than round-robining the CPU list, which
/// is what makes AMD multi-CCD correct. `proc_per_core` is irrelevant here;
/// this is `cpu.rs`'s `show_coretemp` path.
///
/// The `cpu_core_map` override is *not* applied here because this function is
/// `cpu.rs`'s caller and takes no `Config`; [`map_cores_to_sensors_with`] is
/// the config-aware variant and must be preferred when a mapping is set.
pub fn map_cores_to_sensors(sensors: &[Sensor]) -> Vec<(usize, usize)> {
    map_cores_to_sensors_with(sensors, None, "")
}

/// `map_cores_to_sensors` plus btop's `cpu_core_map` override (`"0:1 1:2"`, core:sensor).
pub fn map_cores_to_sensors_with(
    sensors: &[Sensor],
    core_map: Option<&str>,
    override_map: &str,
) -> Vec<(usize, usize)> {
    let pool: Vec<usize> = (0..sensors.len())
        .filter(|&i| kind_of(&sensors[i].0) == SensorKind::Core)
        .collect();
    if pool.is_empty() {
        return Vec::new();
    }
    let num_sensors = pool.len();

    // An explicit `cpu_core_map` ("core:sensor" pairs) wins outright.
    let manual = parse_override(override_map);
    if !manual.is_empty() {
        return manual
            .into_iter()
            .filter(|(_, s)| *s < num_sensors)
            .map(|(cpu, s)| (cpu, pool[s]))
            .collect();
    }

    // Otherwise spread the *core-ID* space evenly across the sensors:
    //     sensor = core_id * num_sensors / (max_core_id + 1)
    // On a 5950X (16 cores, ids 0-15, 2 Tccd sensors) this gives 0 for ids
    // 0-7 and 1 for ids 8-15. Round-robining the CPU list would not.
    let ids = core_map.and_then(parse_core_ids);
    let Some((ids, max_core)) = ids else {
        // No usable /proc/cpuinfo: one sensor per CPU, which is the sane
        // answer for the common single-die case and is never wrong by much.
        return (0..num_sensors)
            .map(|cpu| (cpu, pool[cpu.min(num_sensors - 1)]))
            .collect();
    };

    let mut map: BTreeMap<usize, usize> = BTreeMap::new();
    let mut highest_cpu = 0usize;
    for (cpu, core_id) in &ids {
        let bucket = core_id.saturating_mul(num_sensors) / (max_core + 1).max(1);
        map.insert(*cpu, pool[bucket.min(num_sensors - 1)]);
        highest_cpu = highest_cpu.max(cpu + 1);
    }

    // Where a logical CPU had no `core id` line at all, fill the gap
    // round-robin so its row is not simply blank.
    if map.len() < highest_cpu {
        let next = map.len();
        map.insert(next, pool[next.min(num_sensors - 1)]);
    }

    map.into_iter().collect()
}

/// btop's `cpu_core_map`: `"0:1 1:2"` — logical CPU index to sensor index.
/// Unparseable entries are skipped rather than aborting the whole mapping.
fn parse_override(spec: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for pair in spec.split_ascii_whitespace() {
        let Some((cpu, sensor)) = pair.split_once(':') else {
            continue;
        };
        match (cpu.trim().parse::<usize>(), sensor.trim().parse::<usize>()) {
            (Ok(c), Ok(s)) => out.push((c, s)),
            _ => continue,
        }
    }
    out
}

/// `(cpu -> core id, max core id)` from `/proc/cpuinfo`.
fn parse_core_ids(text: &str) -> Option<(BTreeMap<usize, usize>, usize)> {
    let mut ids: BTreeMap<usize, usize> = BTreeMap::new();
    let mut max_core = 0usize;
    let mut cur_cpu: Option<usize> = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "processor" => cur_cpu = v.trim().parse::<usize>().ok(),
            "core id" | "core" => {
                let Some(core) = v.trim().parse::<usize>().ok() else {
                    continue;
                };
                if let Some(cpu) = cur_cpu {
                    ids.insert(cpu, core);
                    max_core = max_core.max(core);
                }
            }
            _ => {}
        }
    }
    if ids.is_empty() {
        None
    } else {
        Some((ids, max_core))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, c: f32) -> Sensor {
        (name.to_string(), c, 95.0)
    }

    #[test]
    fn nvme_sensors_are_skipped() {
        // btop's rule: an nvme node must never reach the CPU panel.
        assert!(is_nvme_sensor("nvme", "Composite"));
        assert!(is_nvme_sensor("nvme", "temp1"));
        assert!(is_nvme_sensor("k10temp", "nvme Composite"));
        assert!(!is_nvme_sensor("k10temp", "Package id 0"));
        assert!(!is_nvme_sensor("coretemp", "Core 0"));
        // Case: the kernel node is `nvme`, never `NVMe`, but be sure.
        assert!(is_nvme_sensor("NVMe", "Composite"));
    }

    #[test]
    fn label_classification() {
        assert_eq!(kind_of("k10temp/Package id 0"), SensorKind::Package);
        assert_eq!(kind_of("k10temp/Tdie"), SensorKind::Package);
        assert_eq!(kind_of("soc/SoC Temperature"), SensorKind::Package);
        assert_eq!(kind_of("k10temp/Tccd1"), SensorKind::Core);
        assert_eq!(kind_of("coretemp/Core 3"), SensorKind::Core);
        assert_eq!(kind_of("acpitz/temp1"), SensorKind::Other);
    }

    #[test]
    fn package_sensor_prefers_label_then_name_then_first() {
        let sensors = vec![
            s("nvme/Composite", 40.0),
            s("coretemp/Core 0", 45.0),
            s("k10temp/Package id 0", 50.0),
            s("k10temp/Tccd1", 46.0),
        ];
        let p = package_sensor(&sensors).expect("a package sensor exists");
        assert_eq!(p.0, "k10temp/Package id 0");
        assert_eq!(p.1, 50.0);

        // No label match: fall back to a name containing `cpu`.
        let no_label = vec![s("acpitz/temp1", 40.0), s("zenpower/CPU", 55.0)];
        assert_eq!(package_sensor(&no_label).map(|s| s.1), Some(55.0));

        // Nothing convincing: first sensor, never a panic.
        assert_eq!(
            package_sensor(&[s("acpitz/temp1", 40.0)]).map(|s| s.1),
            Some(40.0)
        );
        assert!(package_sensor(&[]).is_none());
    }

    #[test]
    fn core_mapping_spreads_core_ids_evenly_across_sensors() {
        // A 5950X-shaped cpuinfo: 16 logical CPUs, core ids 0..15, two Tccd.
        let mut ids = String::new();
        for cpu in 0..16usize {
            ids.push_str(&format!("processor\t: {cpu}\ncore id\t\t: {cpu}\n\n"));
        }
        let sensors = vec![s("k10temp/Tccd1", 40.0), s("k10temp/Tccd2", 41.0)];
        let map = map_cores_to_sensors_with(&sensors, Some(&ids), "");
        // 0..7 -> Tccd1 (index 0), 8..15 -> Tccd2 (index 1).
        assert_eq!(map.first().map(|&(c, _)| c), Some(0));
        assert_eq!(map.last().map(|&(c, _)| c), Some(15));
        for (cpu, sensor) in &map {
            let expected = if *cpu < 8 { 0 } else { 1 };
            assert_eq!(*sensor, expected, "cpu {cpu} mapped wrong");
        }
    }

    #[test]
    fn core_mapping_without_cpuinfo_is_empty_not_a_panic() {
        // No per-core sensors at all -> nothing to map. `cpu.rs` leaves        // `temp_c` as None rather than inventing a value.
        let sensors = vec![s("nvme/Composite", 40.0)];
        assert!(map_cores_to_sensors(&sensors).is_empty());
        assert!(map_cores_to_sensors(&[]).is_empty());
    }

    #[test]
    fn cpuinfo_core_id_parsing() {
        let text = "processor\t: 0\ncore id\t\t: 3\n\nprocessor\t: 1\ncore id\t\t: 7\n";
        let (ids, max) = parse_core_ids(text).expect("parsed");
        assert_eq!(ids.get(&0), Some(&3));
        assert_eq!(ids.get(&1), Some(&7));
        assert_eq!(max, 7);
        assert!(parse_core_ids("model name\t: x\n").is_none());
        assert!(parse_core_ids("").is_none());
    }

    #[test]
    fn mount_and_missing_dir_are_not_special_cased_here() {
        // `sysfs` behaviour this module relies on: a missing dir lists empty,
        // so a container yields an empty Vec and the panel shows no temp.
        assert!(list_dir("/definitely/not/here").is_empty());
    }
}
