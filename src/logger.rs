//! File logging plus a "say this once" guard.
//!
//! The guard is the important part. A missing `/sys` file must not append a
//! line 1000 times a second; btop solves the same problem with its
//! `ignore_list`, and we solve it with a process-wide `HashSet` of keys.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// `$XDG_STATE_HOME/btop-gpui` (or `~/.local/state/btop-gpui`).
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join("btop-gpui")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Self::Error,
            "info" => Self::Info,
            "debug" => Self::Debug,
            _ => Self::Warn,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
        }
    }
}

struct Logger {
    path: Option<PathBuf>,
    level: Level,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();
static ONCE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn logger() -> &'static Mutex<Logger> {
    LOGGER.get_or_init(|| {
        Mutex::new(Logger {
            path: None,
            level: Level::Warn,
        })
    })
}

fn once_set() -> &'static Mutex<HashSet<String>> {
    ONCE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Point the logger at a file and set the level. Failure is not fatal — the
/// app must still start when `$HOME` is read-only.
pub fn init(path: &Path, level: Level) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut g) = logger().lock() {
        g.path = Some(path.to_path_buf());
        g.level = level;
    }
}

pub fn set_level(level: Level) {
    if let Ok(mut g) = logger().lock() {
        g.level = level;
    }
}

fn write_line(level: Level, msg: &str) {
    let Ok(g) = logger().lock() else { return };
    if level > g.level {
        return;
    }
    let Some(path) = g.path.clone() else { return };
    // A log write must never take the app down, so every error is discarded.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] {}", level.tag(), msg);
    }
}

pub fn error(msg: &str) {
    write_line(Level::Error, msg);
}
pub fn warn(msg: &str) {
    write_line(Level::Warn, msg);
}
pub fn info(msg: &str) {
    write_line(Level::Info, msg);
}
pub fn debug(msg: &str) {
    write_line(Level::Debug, msg);
}

/// Log `msg` the first time `key` is seen, and never again this run. Returns
/// `true` when the message was actually written.
pub fn once(key: &str, msg: &str) -> bool {
    {
        let Ok(mut seen) = once_set().lock() else {
            return false;
        };
        if !seen.insert(key.to_string()) {
            return false;
        }
    }
    write_line(Level::Warn, msg);
    true
}

/// Forget every `once` key. Used when an option changes such that a previously
/// unavailable source might now be readable.
pub fn reset_once() {
    if let Ok(mut seen) = once_set().lock() {
        seen.clear();
    }
}

/// Log a panic with its location, then let the default hook print it. A panic
/// on the UI thread otherwise leaves a silently frozen window.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info.location().map_or_else(
            || "unknown".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        write_line(Level::Error, &format!("PANIC at {where_}: {info}"));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_fires_exactly_once() {
        let key = "unit-test-once-key";
        assert!(once(key, "first"), "first call must log");
        assert!(!once(key, "second"), "second call must be suppressed");
    }

    #[test]
    fn level_parsing_defaults_to_warn() {
        assert_eq!(Level::from_config("debug"), Level::Debug);
        assert_eq!(Level::from_config("nonsense"), Level::Warn);
        assert_eq!(Level::from_config("ERROR"), Level::Error);
    }
}
