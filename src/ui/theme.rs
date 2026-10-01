//! Theme: a gpui-kit `Theme` built from a btop-compatible palette, plus the
//! btop `.theme` file parser.
//!
//! The rule that makes this work: **nothing in the panels may name a literal
//! colour.** Every colour comes from `cx.theme()`, so changing the theme
//! restyles every panel at once and there is nothing to hunt down.

use gpui_kit::component::theme::{Theme, ThemeMode};

use crate::config::Config;
use crate::logger;

/// What the user asked for, as distinct from what the desktop currently is.
///
/// gpui-kit's `ThemeMode` has only `Light` and `Dark` — there is no `System`
/// variant — so "follow the desktop" has to be resolved here, once, into one of
/// those two. Keeping the choice separate from the resolved mode is what makes
/// `System` survive a desktop that changes its mind: the choice is what gets
/// persisted, the mode is what gets painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// Always dark, whatever the desktop says.
    Dark,
    /// Always light, whatever the desktop says.
    Light,
    /// Follow the desktop's colour-scheme preference. The default.
    #[default]
    System,
}

impl ThemeChoice {
    /// Parse the `theme_mode` config value. An unknown or misspelled word falls
    /// back to `System` rather than silently picking a side.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Self::Dark,
            "light" => Self::Light,
            _ => Self::System,
        }
    }

    /// The name written back to the config file and shown in the title bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "System",
        }
    }

    /// Cycle order for the toggle: System -> Dark -> Light -> System.
    ///
    /// `System` sits between the two explicit choices so one press either way
    /// reaches a fixed mode, and a second press returns to following the
    /// desktop.
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::System,
        }
    }

    /// The mode to actually paint with.
    pub fn resolve(self) -> ThemeMode {
        match self {
            Self::Dark => ThemeMode::Dark,
            Self::Light => ThemeMode::Light,
            // Dark is the fallback when the desktop cannot be asked. This is a
            // monitor HUD pinned to a desktop; being wrong towards dark is far
            // kinder than flashing a white window at 3am.
            Self::System => probe_system_mode().unwrap_or(ThemeMode::Dark),
        }
    }
}

/// The icon for a theme choice.
///
/// A moon, a sun, or the half-and-half that means "follow the desktop" — so the
/// current state is legible without clicking, and `SunMoon` reads as "automatic"
/// rather than as a third colour.
pub fn icon_for(choice: ThemeChoice) -> gpui_kit::assets::IconName {
    // The full Lucide set, not `component::IconName` — that one is a small
    // compatibility subset and does not carry `SunMoon`.
    use gpui_kit::assets::IconName;
    match choice {
        ThemeChoice::Dark => IconName::Moon,
        ThemeChoice::Light => IconName::Sun,
        ThemeChoice::System => IconName::SunMoon,
    }
}

/// Decide light or dark from the desktop's own settings.
///
/// gpui-pre ships no Linux colour-scheme detection — `Window::appearance()` is
/// fed by the platform crate and does not consult the desktop — so this reads
/// the same two GSettings keys GNOME itself reads. Returns `None` on a
/// non-GNOME session, in a container, or with no `gsettings` on PATH, and
/// `ThemeChoice::resolve` then falls back to dark.
pub fn probe_system_mode() -> Option<ThemeMode> {
    let scheme = gsettings_get("color-scheme")?;
    let gtk_theme = gsettings_get("gtk-theme").unwrap_or_default();
    colour_scheme_mode(&scheme, &gtk_theme)
}

/// `gsettings get org.gnome.desktop.interface <key>`, quotes stripped.
///
/// `gsettings` prints the value *with* its GVariant quotes — `'prefer-dark'` —
/// so comparing against `prefer-dark` without trimming never matches.
fn gsettings_get(key: &str) -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    Some(raw.trim().trim_matches('\'').to_string())
}

/// The decision itself, split out from the subprocess so it can be tested.
///
/// GNOME treats `color-scheme = 'default'` as "whatever the GTK theme says", so
/// a `-Dark` GTK theme with no explicit preference has to come out dark. An
/// explicit `prefer-light` wins over a dark GTK theme, because that is the key
/// the user actually set.
pub fn colour_scheme_mode(scheme: &str, gtk_theme: &str) -> Option<ThemeMode> {
    match scheme.trim() {
        "prefer-dark" => Some(ThemeMode::Dark),
        "prefer-light" => Some(ThemeMode::Light),
        _ => {
            let theme = gtk_theme.trim().to_ascii_lowercase();
            if theme.is_empty() {
                None
            } else if theme.contains("dark") {
                Some(ThemeMode::Dark)
            } else {
                Some(ThemeMode::Light)
            }
        }
    }
}

/// Apply the theme chosen in the config. Called once at startup.
///
/// btop's themes are per-`.theme` files with `theme[main]`/`theme[cpu]` key
/// blocks; only the handful of keys that have a gpui-kit equivalent are mapped,
/// and anything unmapped keeps the library default. That is deliberate — an
/// unmapped key is better than a wrong guess.
pub fn apply(cfg: &Config, cx: &mut gpui_kit::App) {
    validate_palette(cfg);
    set(ThemeChoice::parse(&cfg.str("theme_mode")).resolve(), cx);
}

/// Paint with an already-resolved mode.
///
/// `Theme::change` is global — its `window` parameter is accepted and ignored —
/// so this needs no window and one call restyles every panel that reads
/// `cx.theme()`.
pub fn set(mode: ThemeMode, cx: &mut gpui_kit::App) {
    Theme::change(mode, None, cx);
}

/// Parse the `color_theme` palette for validation only.
///
/// gpui-kit drives the actual colours; this exists so a missing or unusable
/// theme file is reported once instead of silently looking like a bug. A read
/// error yields `None`, never a panic.
fn validate_palette(cfg: &Config) {
    let name = cfg.str("color_theme");
    let theme_name = name.trim();
    let path = crate::config::themes_dir().join(format!("{theme_name}.theme"));
    if !theme_name.is_empty() && !path.exists() {
        logger::once(
            "theme-missing",
            "colour theme file not found; using the built-in palette",
        );
    }
    if let Some(text) = std::fs::read_to_string(&path).ok()
        && crate::ui::themes::parse(&text).is_none()
    {
        logger::once("theme-unreadable", "colour theme file had no usable keys");
    }
}

/// btop's `temp_scale` option, resolved for `format.rs`.
pub fn temp_scale(cfg: &Config) -> crate::format::TempScale {
    cfg.temp_scale()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_is_the_default_choice() {
        // The brief was "default to dark or system"; system must win by default
        // so the app matches the desktop out of the box.
        assert_eq!(ThemeChoice::default(), ThemeChoice::System);
    }

    #[test]
    fn parsing_is_case_insensitive_and_falls_back_to_system() {
        assert_eq!(ThemeChoice::parse("dark"), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::parse("DARK"), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::parse(" Light "), ThemeChoice::Light);
        assert_eq!(ThemeChoice::parse("System"), ThemeChoice::System);
        // A typo must not silently pick a side.
        assert_eq!(ThemeChoice::parse("dar"), ThemeChoice::System);
        assert_eq!(ThemeChoice::parse(""), ThemeChoice::System);
    }

    #[test]
    fn the_cycle_visits_every_choice_and_returns() {
        let start = ThemeChoice::System;
        let second = start.next();
        let third = second.next();
        assert_ne!(second, start);
        assert_ne!(third, second);
        assert_ne!(third, start, "all three choices must be distinct");
        assert_eq!(third.next(), start, "three presses must return");
    }

    #[test]
    fn labels_round_trip_through_parse() {
        // `label` is what gets written to the config, so `parse(label())` must
        // give the choice back or a restart would silently lose the setting.
        for choice in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
            assert_eq!(ThemeChoice::parse(choice.label()), choice);
        }
    }

    #[test]
    fn an_explicit_preference_beats_the_gtk_theme_name() {
        // The key the user actually set wins, even when the GTK theme name
        // suggests the opposite.
        assert_eq!(
            colour_scheme_mode("prefer-dark", "Adwaita"),
            Some(ThemeMode::Dark)
        );
        assert_eq!(
            colour_scheme_mode("prefer-light", "Adwaita-Dark"),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn the_neutral_scheme_defers_to_the_gtk_theme() {
        // GNOME's 'default' means "whatever the GTK theme says", so a dark GTK
        // theme with no explicit preference still has to read dark.
        assert_eq!(
            colour_scheme_mode("default", "ZorinOrange-Dark"),
            Some(ThemeMode::Dark)
        );
        assert_eq!(
            colour_scheme_mode("default", "Adwaita"),
            Some(ThemeMode::Light)
        );
        // Nothing to go on at all: the caller then falls back to dark.
        assert_eq!(colour_scheme_mode("default", ""), None);
    }

    #[test]
    fn every_choice_resolves_to_a_real_mode_without_panicking() {
        // Must hold on any machine, including CI with no gsettings at all.
        for choice in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
            assert!(matches!(
                choice.resolve(),
                ThemeMode::Dark | ThemeMode::Light
            ));
        }
        assert_eq!(ThemeChoice::Dark.resolve(), ThemeMode::Dark);
        assert_eq!(ThemeChoice::Light.resolve(), ThemeMode::Light);
    }
}
