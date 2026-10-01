//! btop-gpui: a native desktop system monitor in the spirit of btop.
//!
//! # Layering
//!
//! This library target exists so the data layer can be tested **without a
//! window**. The rule the project is built around:
//!
//! * `model`, `format`, `history`, `logger`, `config` and everything under
//!   `collect` are **pure** — no GPUI import, ever.
//! * `app` and `ui` are the only GPUI-aware modules, and they only ever *read*
//!   what a collector produced.
//!
//! That split is what makes `cargo test` meaningful: the parsers can be checked
//! against captured `/proc` fixtures on a machine with no display at all.

pub mod collect;
pub mod config;
pub mod format;
pub mod history;
pub mod logger;
pub mod model;
