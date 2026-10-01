//! btop-gpui: a native desktop system monitor in the spirit of btop.
//!
//! Boot order matters and each step depends on the previous one:
//!
//! 1. Load the config, so the theme and the tick interval are known.
//! 2. Install the panic hook **before** anything can fail, so a panic on the UI
//!    thread is logged rather than lost.
//! 3. Initialise gpui-kit (registers the component library and its assets).
//! 4. Start the collector thread, then open a window holding a view that
//!    borrows its `Shared` cell.
//!
//! Nothing here reads `/proc`. That all happens on the collector thread.

// `prelude` brings `Styled`, `ParentElement`, `px`, `div` and the
// generated style helpers (`p_2`, `gap_2`, `text_xs`, …) into scope.
use gpui_kit::prelude::*;
use std::sync::Arc;

use gpui_kit::component::theme::ThemeMode;
use gpui_kit::{WindowOptions, px, size};

use btop_gpui::app::AppView;
use btop_gpui::ui::chrome::window_options;
use btop_gpui::{collect, config::Config, logger};

fn main() {
    // A panic on the UI thread must be logged before the window is up, or the
    // reason for a blank window is lost entirely.
    logger::install_panic_hook();

    let cfg = Config::load_or_default();
    logger::init(&logger::state_dir().join("btop-gpui.log"), cfg.log_level());

    logger::info(&format!(
        "btop-gpui starting: update every {:?}",
        cfg.update_interval()
    ));

    let shared = collect::spawn(cfg.clone());

    // `run` takes a `'static` closure, so everything it touches must be moved
    // in rather than borrowed. Cloning `cfg` and the `Arc` here is what makes
    // that possible — the originals stay alive for the caller, which is
    // harmless, and the clones are what the view owns.
    let cfg_for_run = cfg.clone();
    let shared_for_run = Arc::clone(&shared);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            // Must precede any use of the component library.
            gpui_kit::init(cx);

            // btop's `theme_background` picks light vs dark; the default is
            // dark, which is what a monitor window wants on a desktop.
            let mode = if cfg_for_run.bool("theme_background") {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
            gpui_kit::component::theme::Theme::change(mode, None, cx);

            let options = WindowOptions {
                window_min_size: Some(size(px(900.), px(600.))),
                ..window_options()
            };

            let cfg_for_view = cfg_for_run.clone();
            gpui_kit::open_window(options, cx, move |_window, cx| {
                cx.new(|cx| AppView::new(cfg_for_view.clone(), shared_for_run, cx))
            })
            .expect("failed to open the btop-gpui window");
        });
}
