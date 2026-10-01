//! btop `.theme` file parsing.
//!
//! A btop theme is an INI-ish file with one section per box:
//!
//! ```text
//! theme[main]
//! theme[selected]
//! theme[inactive]
//! theme[proc_misc]
//! ```
//!
//! Keys are btop's own names (`main_bg`, `main_fg`, `hi_fg`, `graph_start`,
//! `meter_start`, `cpu_start` …) and values are `#rrggbb`.
//!
//! Only the keys with a gpui-kit equivalent are kept. An unmapped key is
//! **dropped**, not guessed at — a wrong colour is worse than the library's
//! default, and the panel would look broken in a way that is hard to trace.

use std::collections::HashMap;

/// The palette extracted from a theme file, as `#rrggbb` strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeColors {
    /// `[main]` background, the panel fill.
    pub main_bg: Option<String>,
    /// `[main]` foreground, the body text.
    pub main_fg: Option<String>,
    /// `[selected]` background, a selected row or process.
    pub selected_bg: Option<String>,
    /// `[hi]` foreground, the bright accent.
    pub hi_fg: Option<String>,
    /// `[selected]` foreground.
    pub selected_fg: Option<String>,
    /// The CPU graph gradient, as six comma-separated `#rrggbb`.
    pub cpu_start: Option<String>,
    /// The meter/bar gradient start colour.
    pub meter_start: Option<String>,
}

/// Parse a btop theme file. Returns `None` when nothing usable was found, which
/// is the signal to fall back to the library defaults.
pub fn parse(text: &str) -> Option<ThemeColors> {
    let mut section = String::new();
    let mut out = ThemeColors::default();
    let mut found_any = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') && !line.starts_with("theme[") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("theme[") {
            section = rest.trim_end_matches(']').trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }

        // The key is only meaningful inside its own section: btop reuses names
        // like `bg` in several of them.
        match (section.as_str(), key.as_str()) {
            ("main", "main_bg") | ("", "main_bg") => out.main_bg = Some(value.to_string()),
            ("main", "main_fg") | ("", "main_fg") => out.main_fg = Some(value.to_string()),
            ("selected", "selected_bg") => out.selected_bg = Some(value.to_string()),
            ("selected", "selected_fg") => out.selected_fg = Some(value.to_string()),
            ("main", "hi_fg") | ("", "hi_fg") => out.hi_fg = Some(value.to_string()),
            ("cpu", "cpu_start") => out.cpu_start = Some(value.to_string()),
            ("meter", "meter_start") => out.meter_start = Some(value.to_string()),
            _ => continue,
        }
        found_any = true;
    }

    found_any.then_some(out)
}

/// The hex colours in a gradient list, in order. A value that is not a list
/// yields a single entry rather than nothing.
pub fn gradient(value: &str) -> Vec<String> {
    let parts: Vec<String> = value
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        vec![value.to_string()]
    } else {
        parts
    }
}

/// Is this a `#rrggbb` or `#rgb` colour? Used to reject a theme file that has
/// been edited into something unusable.
pub fn is_hex_colour(value: &str) -> bool {
    let v = value.strip_prefix('#').unwrap_or(value);
    matches!(v.len(), 3 | 6) && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every colour key in a theme file, for a debug listing.
pub fn all_keys(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut section = String::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("theme[") {
            section = rest.trim_end_matches(']').trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !value.trim().is_empty() {
            out.insert(
                format!("{section}.{}", key.trim()),
                value.trim().to_string(),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# A comment line
theme[main]
main_bg=#1c1c1c
main_fg=#dddddd
hi_fg=#8ab4d8
selected_fg=#ffffff

theme[selected]
selected_bg=#3465a4
selected_fg=#ffffff

theme[cpu]
cpu_start=#4e9a06,#c4a000,#c4a000,#3465a4,#75507b

theme[meter]
meter_start=#4e9a06
"#;

    #[test]
    fn parses_the_main_palette() {
        let t = parse(SAMPLE).expect("parses");
        assert_eq!(t.main_bg.as_deref(), Some("#1c1c1c"));
        assert_eq!(t.main_fg.as_deref(), Some("#dddddd"));
        assert_eq!(t.selected_bg.as_deref(), Some("#3465a4"));
        assert_eq!(t.hi_fg.as_deref(), Some("#8ab4d8"));
    }

    #[test]
    fn sections_do_not_leak_into_each_other() {
        // `selected_fg` appears in both [main] and [selected]; the section
        // decides which one wins, and [main] comes first so it is overwritten
        // by [selected] — the same file, two values.
        let t = parse(SAMPLE).expect("parses");
        assert_eq!(t.selected_fg.as_deref(), Some("#ffffff"));
    }

    #[test]
    fn gradients_split_on_commas() {
        let t = parse(SAMPLE).expect("parses");
        let cpu = t.cpu_start.expect("cpu_start");
        assert_eq!(gradient(&cpu).len(), 5);
        assert_eq!(gradient(&cpu)[0], "#4e9a06");
        assert!(gradient(&cpu).iter().all(|c| is_hex_colour(c)));
    }

    #[test]
    fn an_empty_or_useless_file_is_none() {
        assert!(parse("").is_none());
        assert!(parse("# just a comment\n\n").is_none());
        // Keys we do not map must not count as "usable".
        assert!(parse("theme[main]\nsome_unknown_key=#123456\n").is_none());
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let text = "theme[main]\nthis line has no equals\nmain_bg=#000000\n";
        let t = parse(text).expect("parses");
        assert_eq!(t.main_bg.as_deref(), Some("#000000"));
    }

    #[test]
    fn hex_validation() {
        assert!(is_hex_colour("#fff"));
        assert!(is_hex_colour("ffffff"));
        assert!(is_hex_colour("#1c1c1c"));
        assert!(!is_hex_colour("#gggggg"));
        assert!(!is_hex_colour("#12345"));
        assert!(!is_hex_colour(""));
    }
}
