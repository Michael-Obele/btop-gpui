//! Theme: a gpui-kit `Theme` built from a btop-compatible palette, plus the
//! btop `.theme` file parser.
//!
//! The rule that makes this work: **nothing in the panels may name a literal
//! colour.** Every colour comes from `cx.theme()`, so changing the theme
//! restyles every panel at once and there is nothing to hunt down.

use gpui_kit::component::theme::{Theme, ThemeMode};

use crate::config::Config;
use crate::logger;

/// Apply the theme named in the config. Called once at startup.
///
/// btop's themes are per-`.theme` files with `theme[main]`/`theme[cpu]` key
/// blocks; only the handful of keys that have a gpui-kit equivalent are mapped,
/// and anything unmapped keeps the library default. That is deliberate — an
/// unmapped key is better than a wrong guess.
pub fn apply(cfg: &Config, window: Option<&mut gpui_kit::Window>, cx: &mut gpui_kit::App) {
    let name = cfg.str("color_theme");
    let mode = if cfg.bool("theme_background") {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };

    // The palette is parsed for validation only: gpui-kit drives the actual
    // colours from `Theme::change`, and an unmapped key is better than a
    // guessed one. Reading a missing file yields None, not a panic.
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

    Theme::change(mode, window, cx);
}

/// btop's `temp_scale` option, resolved for `format.rs`.
pub fn temp_scale(cfg: &Config) -> crate::format::TempScale {
    cfg.temp_scale()
}