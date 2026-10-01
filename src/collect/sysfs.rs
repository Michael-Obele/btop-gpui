//! Tiny, total, never-panicking readers for `/proc` and `/sys`.
//!
//! Everything the data layer reads from a pseudo-file goes through here, so
//! there is exactly one place where "a file went missing" turns into `None`
//! instead of an error or a panic. GPUI runs on the UI thread and a panic
//! there kills the window, so the rule is absolute: **no `unwrap`, no `expect`,
//! no indexing that can go out of bounds.**

use std::fs;
use std::path::{Path, PathBuf};

/// Read a file, trim trailing whitespace, `None` on any error.
pub fn read_str(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path.as_ref())
        .ok()
        .map(|s| s.trim_end().to_string())
}

pub fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_str(path)?.trim().parse().ok()
}

pub fn read_i64(path: impl AsRef<Path>) -> Option<i64> {
    read_str(path)?.trim().parse().ok()
}

pub fn read_f64(path: impl AsRef<Path>) -> Option<f64> {
    read_str(path)?.trim().parse().ok()
}

/// sysfs temperatures are milli-degrees Celsius. This returns degrees.
pub fn read_milli_celsius(path: impl AsRef<Path>) -> Option<f32> {
    let v: f32 = read_str(path)?.trim().parse().ok()?;
    let c = v / 1000.0;
    c.is_finite().then_some(c)
}

/// Directory entries, sorted so that every tick produces the same order
/// (important: two ticks that enumerate in different orders make deltas wrong).
pub fn list_dir(path: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(path.as_ref()) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return Vec::new(),
    };
    v.sort();
    v
}

/// Read a `key: value` file into an ordered list. Used by `/proc/meminfo`.
/// Note that ZFS `arcstats` is `name type value` — a different shape, handled
/// by its own reader in `mem.rs`.
pub fn read_kv(path: impl AsRef<Path>) -> Vec<(String, String)> {
    let Some(text) = fs::read_to_string(path.as_ref()).ok() else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Modification time as a unix timestamp, used to decide whether a cached
/// `/etc/passwd` or mount table needs re-reading.
pub fn mtime_secs(path: impl AsRef<Path>) -> Option<u64> {
    let md = fs::metadata(path.as_ref()).ok()?;
    let t = md.modified().ok()?;
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Decode the octal escapes the kernel writes into `/proc/self/mounts`, e.g.
/// a mount point containing a space appears as `\040`.
pub fn unescape_mount(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len()
            && let Ok(oct) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(oct as char);
                i += 4;
                continue;
            }
        // Non-ASCII bytes are passed through as latin-1, matching what the
        // kernel's own escaping round-trips to.
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// The `/etc/passwd` user for a uid, cached on the file's mtime.
///
/// `/etc/passwd` is read once per tick for every distinct uid otherwise, which
/// on a busy machine is a lot of pointless I/O.
pub struct PasswdCache {
    map: std::collections::HashMap<u32, String>,
    mtime: Option<u64>,
}

impl PasswdCache {
    pub fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            mtime: None,
        }
    }

    pub fn user_for(&mut self, uid: u32) -> String {
        let current = mtime_secs("/etc/passwd");
        if current != self.mtime {
            self.map.clear();
            self.mtime = current;
            if let Ok(text) = fs::read_to_string("/etc/passwd") {
                for line in text.lines() {
                    // name:x:uid:gid:gecos:home:shell -- the uid is the THIRD
                    // field, not the second (that is the password placeholder,
                    // which does not parse as a number and so would silently
                    // produce an empty map).
                    let mut f = line.split(':');
                    let (Some(name), Some(_), Some(uid_field)) = (f.next(), f.next(), f.next())
                    else {
                        continue;
                    };
                    if let Ok(uid) = uid_field.parse::<u32>() {
                        self.map.insert(uid, name.to_string());
                    }
                }
            }
        }
        self.map
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| uid.to_string())
    }
}

impl Default for PasswdCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_none_not_panics() {
        assert!(read_str("/definitely/not/here").is_none());
        assert!(read_u64("/definitely/not/here").is_none());
        assert!(read_milli_celsius("/definitely/not/here").is_none());
        assert!(list_dir("/definitely/not/here").is_empty());
        assert!(read_kv("/definitely/not/here").is_empty());
    }

    #[test]
    fn mount_unescaping() {
        assert_eq!(unescape_mount("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount("/mnt/tab\\011here"), "/mnt/tab\there");
        // A real mount table escapes a literal backslash as `\134`, so two
        // consecutive backslashes are not an escape sequence at all.
        assert_eq!(unescape_mount("/mnt/back\\134slash"), "/mnt/back\\slash");
        assert_eq!(unescape_mount("/plain"), "/plain");
        // A trailing lone backslash must not read past the end.
        assert_eq!(unescape_mount("trailing\\"), "trailing\\");
    }

    #[test]
    fn passwd_cache_resolves_uids() {
        let mut c = PasswdCache::new();
        let root = c.user_for(0);
        assert_eq!(root, "root", "uid 0 is always root on Linux");
    }
}
