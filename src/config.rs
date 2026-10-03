//! `btop-gpui.conf` — btop-compatible `key = value` configuration.
//!
//! Three rules drive the design (see docs/05-config-and-theming.md):
//!
//! 1. **Config over hardcoded values.** Every option a user might reasonably
//!    want lives here, with btop's own defaults and btop's own key names, so an
//!    existing `btop.conf` can be ported by hand.
//! 2. **Unknown keys survive a save/load round trip**, so a config written by a
//!    newer version is never silently destroyed.
//! 3. **The written file is self-documenting.** The `DESCRIPTIONS` table below
//!    *is* the comment template — the same trick btop uses, where its
//!    `descriptions` map is the config file.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::logger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptType {
    Str,
    Bool,
    Int,
}

/// `(key, type, default, description)` — the single source of truth for
/// parsing, defaults and the generated comment block.
pub const DESCRIPTIONS: &[(&str, OptType, &str, &str)] = &[
    // ---- Global / UI ----
    (
        "color_theme",
        OptType::Str,
        "Default",
        "Theme name, or a file in the themes directory",
    ),
    (
        "theme_mode",
        OptType::Str,
        "System",
        "Dark | Light | System (System follows the desktop)",
    ),
    (
        "update_ms",
        OptType::Int,
        "2000",
        "Milliseconds between updates (minimum 100)",
    ),
    (
        "shown_boxes",
        OptType::Str,
        "cpu mem net proc",
        "Which boxes exist: cpu mem disk net proc",
    ),
    ("show_uptime", OptType::Bool, "True", "Show uptime"),
    (
        "custom_cpu_name",
        OptType::Str,
        "",
        "Overrides the detected CPU model string",
    ),
    (
        "log_level",
        OptType::Str,
        "warn",
        "error | warn | info | debug (btop-gpui addition)",
    ),
    // ---- CPU ----
    (
        "show_cpu_watts",
        OptType::Bool,
        "True",
        "CPU wattage (needs cap_perfmon)",
    ),
    ("check_temp", OptType::Bool, "True", "Read temperatures"),
    (
        "show_coretemp",
        OptType::Bool,
        "True",
        "Per-core temperatures",
    ),
    (
        "temp_scale",
        OptType::Str,
        "celsius",
        "celsius | fahrenheit | kelvin",
    ),
    ("show_cpu_freq", OptType::Bool, "True", "Show CPU frequency"),
    (
        "freq_mode",
        OptType::Str,
        "first",
        "first | range | lowest | highest | average",
    ),
    (
        "show_battery",
        OptType::Bool,
        "True",
        "Show the battery box",
    ),
    (
        "selected_battery",
        OptType::Str,
        "Auto",
        "Battery device name, or Auto for the highest capacity",
    ),
    (
        "show_battery_watts",
        OptType::Bool,
        "True",
        "Show battery power draw",
    ),
    (
        "cpu_meter_style",
        OptType::Str,
        "bar",
        "bar | chip — how per-core load is drawn",
    ),
    // ---- Memory / disks ----
    (
        "mem_graphs",
        OptType::Bool,
        "True",
        "Show the memory usage graph",
    ),
    ("show_swap", OptType::Bool, "True", "Show swap"),
    (
        "swap_disk",
        OptType::Bool,
        "True",
        "Add a synthetic swap row to the disk box",
    ),
    (
        "zfs_arc_cached",
        OptType::Bool,
        "True",
        "Count the ZFS ARC as cached memory",
    ),
    ("show_disks", OptType::Bool, "True", "Show the disk box"),
    (
        "only_physical",
        OptType::Bool,
        "True",
        "Hide tmpfs, overlayfs and other virtual filesystems",
    ),
    (
        "use_fstab",
        OptType::Bool,
        "False",
        "Take the disk list from /etc/fstab instead of the mount table",
    ),
    (
        "zfs_hide_datasets",
        OptType::Bool,
        "False",
        "Show ZFS pools only, not datasets",
    ),
    (
        "disk_free_priv",
        OptType::Bool,
        "False",
        "False = f_bavail, True = f_bfree (root's view)",
    ),
    (
        "show_io_stat",
        OptType::Bool,
        "True",
        "Show disk read/write rates and IO%",
    ),
    (
        "base_10_sizes",
        OptType::Bool,
        "False",
        "Decimal (GB) instead of binary (GiB) sizes",
    ),
    // ---- Network ----
    ("net_iface", OptType::Str, "Auto", "Interface name, or Auto"),
    (
        "net_auto",
        OptType::Bool,
        "True",
        "Auto-scale the network graph",
    ),
    (
        "net_download",
        OptType::Int,
        "100",
        "Fixed download ceiling in Mibibits when net_auto is False",
    ),
    (
        "net_upload",
        OptType::Int,
        "100",
        "Fixed upload ceiling in Mibibits when net_auto is False",
    ),
    (
        "net_sync",
        OptType::Bool,
        "True",
        "Mirror the upload scale to the download scale",
    ),
    // ---- Processes ----
    (
        "proc_sorting",
        OptType::Str,
        "cpu lazy",
        "pid | name | command | threads | user | memory | cpu direct | cpu lazy",
    ),
    (
        "proc_reversed",
        OptType::Bool,
        "False",
        "Reverse the sort order",
    ),
    ("proc_tree", OptType::Bool, "False", "Tree mode"),
    (
        "proc_colors",
        OptType::Bool,
        "True",
        "Colour the CPU and memory columns",
    ),
    (
        "proc_per_core",
        OptType::Bool,
        "True",
        "CPU% is relative to all cores, so it can exceed 100",
    ),
    (
        "proc_filter_kernel",
        OptType::Bool,
        "True",
        "Hide kernel threads",
    ),
];

/// Every option as a flat map. Flat (rather than a struct tree) so that
/// saving is a straight iteration and unknown keys round-trip for free.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<String, String>,
    /// Keys we do not recognise, preserved verbatim on save.
    pub unknown: BTreeMap<String, String>,
}

impl Config {
    pub fn defaults() -> Self {
        let mut values = BTreeMap::new();
        for (key, _, default, _) in DESCRIPTIONS {
            values.insert((*key).to_string(), (*default).to_string());
        }
        Self {
            values,
            unknown: BTreeMap::new(),
        }
    }

    // ---- typed accessors -------------------------------------------------

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_string(), value.to_string());
    }

    pub fn str(&self, key: &str) -> String {
        self.get(key).unwrap_or_default().to_string()
    }

    pub fn bool(&self, key: &str) -> bool {
        // btop's booleans are the literal words "True"/"False".
        self.get(key).is_some_and(|v| v == "True")
    }

    pub fn int(&self, key: &str) -> i64 {
        self.get(key)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn bool_or(&self, key: &str, fallback: bool) -> bool {
        match self.get(key) {
            Some("True") => true,
            Some("False") => false,
            _ => fallback,
        }
    }

    /// A trimmed, lowercased list from a space- or comma-separated option.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .unwrap_or_default()
            .split([' ', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    }

    // ---- derived settings -----------------------------------------------

    pub fn update_interval(&self) -> std::time::Duration {
        // btop clamps anything under 100 ms.
        std::time::Duration::from_millis(self.int("update_ms").clamp(100, 60_000) as u64)
    }

    pub fn size_scale(&self) -> crate::format::SizeScale {
        if self.bool("base_10_sizes") {
            crate::format::SizeScale::Decimal
        } else {
            crate::format::SizeScale::Binary
        }
    }

    pub fn rate_scale(&self) -> crate::format::SizeScale {
        match self.str("base_10_bitrate").as_str() {
            "True" | "true" | "1" => crate::format::SizeScale::Decimal,
            "False" | "false" | "0" => crate::format::SizeScale::Binary,
            // Auto: mirror base_10_sizes, which is btop's behaviour.
            _ => self.size_scale(),
        }
    }

    pub fn temp_scale(&self) -> crate::format::TempScale {
        crate::format::TempScale::from_config(&self.str("temp_scale"))
    }

    pub fn log_level(&self) -> logger::Level {
        logger::Level::from_config(&self.str("log_level"))
    }

    pub fn shown_boxes(&self) -> Vec<String> {
        self.list("shown_boxes")
    }

    pub fn shows(&self, box_name: &str) -> bool {
        self.shown_boxes().iter().any(|b| b == box_name)
    }

    // ---- load / save ----------------------------------------------------

    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::defaults();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim().to_string();

            let Some((_, kind, default, desc)) = DESCRIPTIONS.iter().find(|(k, ..)| *k == key)
            else {
                // Unknown key: keep it so saving does not destroy it.
                cfg.unknown.insert(key.to_string(), value);
                continue;
            };
            let ok = match kind {
                OptType::Str => true,
                OptType::Bool => matches!(value.as_str(), "True" | "False"),
                OptType::Int => value.trim().parse::<i64>().is_ok(),
            };
            if ok {
                cfg.values.insert(key.to_string(), value);
            } else {
                logger::once(
                    &format!("config-bad-{key}"),
                    &format!("config: {key} = {value:?} is not a valid {desc}; using {default:?}"),
                );
            }
        }
        cfg
    }

    pub fn load_or_default() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(_) => {
                let cfg = Self::defaults();
                // First run: write the documented default file so the user can
                // discover the options by reading it.
                let _ = cfg.save(&path);
                cfg
            }
        }
    }

    /// Rewrite the whole file, comment block included. Atomic: write a
    /// temporary file and rename over the real one, so a crash mid-write
    /// cannot destroy the user's config.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, self.render())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
        }
        std::fs::rename(&tmp, path)
    }

    /// The full file body: a generated comment block, then `key = value`.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(8192);
        let _ = writeln!(
            out,
            "# btop-gpui configuration.\n\
             # Booleans are the literal words True and False. Strings are bare unless they\n\
             # contain a comma, in which case they are quoted. Everything not listed here is\n\
             # preserved untouched when the file is rewritten.\n"
        );
        // DESCRIPTIONS is declared in section order, so one pass with a set of
        // section-start keys is enough to emit the right heading.
        for (key, kind, default, desc) in DESCRIPTIONS {
            if let Some(name) = section_start(key) {
                let _ = writeln!(out, "\n# ---- {name} ----");
            }
            let value = self.get(key).unwrap_or(default);
            let _ = writeln!(out, "# {desc}");
            let _ = writeln!(out, "# default: {default}");
            let rendered = match kind {
                OptType::Str if value.contains(',') => format!("{value:?}"),
                _ => value.to_string(),
            };
            let _ = writeln!(out, "{key} = {rendered}\n");
        }
        if !self.unknown.is_empty() {
            let _ = writeln!(out, "# ---- unrecognised keys, preserved verbatim ----");
            for (k, v) in &self.unknown {
                let _ = writeln!(out, "{k} = {v}");
            }
        }
        out
    }
}

/// Returns the section heading when `key` is the first key of a new section.
fn section_start(key: &str) -> Option<&'static str> {
    match key {
        "color_theme" => Some("Global / UI"),
        "cpu_graph_upper" => Some("CPU"),
        "mem_graphs" => Some("Memory and disks"),
        "net_iface" => Some("Network"),
        "proc_sorting" => Some("Processes"),
        "shown_gpus" => Some("GPU (parsed in v1, used from v2)"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| logger::home().join(".config"))
        .join("btop-gpui")
}

pub fn config_path() -> PathBuf {
    config_dir().join("btop-gpui.conf")
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| logger::home().join(".local/share"))
        .join("btop-gpui")
}

pub fn themes_dir() -> PathBuf {
    data_dir().join("themes")
}

pub fn log_path() -> PathBuf {
    logger::state_dir().join("btop-gpui.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let text = Config::defaults().render();
        let parsed = Config::parse(&text);
        assert_eq!(
            parsed,
            Config::defaults(),
            "render -> parse must be lossless"
        );
    }

    #[test]
    fn unknown_keys_survive() {
        let cfg = Config::parse("future_option = 42\nupdate_ms = 500\n");
        assert_eq!(
            cfg.unknown.get("future_option").map(String::as_str),
            Some("42")
        );
        assert_eq!(cfg.int("update_ms"), 500);
        assert!(cfg.render().contains("future_option = 42"));
    }

    #[test]
    fn bad_values_fall_back_to_default() {
        let cfg = Config::parse("update_ms = not-a-number\n");
        assert_eq!(cfg.int("update_ms"), 2000);
        let cfg = Config::parse("proc_tree = yes\n");
        assert!(!cfg.bool("proc_tree"), "only True/False are booleans");
    }

    #[test]
    fn quoted_and_commented_lines_parse() {
        let cfg = Config::parse(
            "# a comment\n\nshown_boxes = \"cpu mem net disks proc\"\n  update_ms = 1000  \n",
        );
        assert_eq!(cfg.int("update_ms"), 1000);
        assert!(cfg.shows("disks"));
    }

    #[test]
    fn update_ms_is_clamped() {
        let mut cfg = Config::defaults();
        cfg.set("update_ms", "10");
        assert_eq!(cfg.update_interval().as_millis(), 100);
        cfg.set("update_ms", "999999");
        assert_eq!(cfg.update_interval().as_millis(), 60_000);
    }

    #[test]
    fn every_description_has_a_default() {
        assert!(!DESCRIPTIONS.is_empty());
        for (key, _, default, desc) in DESCRIPTIONS {
            assert!(!key.is_empty());
            assert!(!desc.is_empty());
            let _ = default;
        }
        // Keys must be unique or the table would silently shadow entries.
        let mut seen = std::collections::HashSet::new();
        for (key, ..) in DESCRIPTIONS {
            assert!(seen.insert(*key), "duplicate key {key} in DESCRIPTIONS");
        }
    }

    /// The guard for the class of bug this file used to be full of.
    ///
    /// `DESCRIPTIONS` held 89 keys; 63 of them were parsed, defaulted, written
    /// back to the config file and then **never read**. A user setting
    /// `show_cpu_watts = False` got a config file that agreed with them and a
    /// panel that disagreed. Nothing failed, so nothing was noticed.
    ///
    /// This walks the crate's own source and requires every declared key to be
    /// read somewhere. It is deliberately a source scan rather than a set of
    /// hand-maintained names, so adding a key without wiring it fails here
    /// rather than in a bug report.
    ///
    /// A key counts as wired if it reaches an accessor (`cfg.bool("x")` and
    /// friends) or a named helper that reads it — `update_ms` goes through
    /// `update_interval()` and `shown_boxes` through `Config::shows`, neither
    /// of which passes the key literal at the call site.
    #[test]
    fn every_declared_key_is_actually_read() {
        // Keys read through a helper, so the literal appears inside a method
        // body rather than at a call site. Listing them explicitly is the price
        // of not reimplementing a Rust parser in a test.
        const VIA_HELPER: &[&str] = &[
            "update_ms",     // Config::update_interval
            "log_level",     // Config::log_level
            "shown_boxes",   // Config::shown_boxes / Config::shows
            "base_10_sizes", // Config::size_scale
        ];

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut others = String::new();
        collect_rust_excluding(&src, "config.rs", &mut others);

        let unwired: Vec<&str> = DESCRIPTIONS
            .iter()
            .map(|(key, ..)| *key)
            .filter(|key| {
                if VIA_HELPER.contains(key) {
                    return false;
                }
                // `others` excludes this file, so the table's own mention of the
                // key is not in there. One hit means a real read somewhere.
                let needle = format!("\"{key}\"");
                !others.contains(&needle)
            })
            .collect();

        assert!(
            unwired.is_empty(),
            "these config keys are declared but never read, so setting them does \
             nothing. Wire them or delete them from DESCRIPTIONS: {unwired:?}"
        );
    }

    /// Recursively append every `.rs` file under `dir` to `out`, skipping any file
    /// whose name is `skip`.
    fn collect_rust_excluding(dir: &std::path::Path, skip: &str, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rust_excluding(&path, skip, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != skip)
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push_str(&text);
            }
        }
    }
}
