//! UI layer. The only modules in the crate that may import GPUI.
//!
//! The split is the point: `model` and `collect` are pure, so the data layer
//! is testable without a window, and these modules only ever *read* what a
//! collector produced. No `/proc` access, no sorting and no `cx.notify()`
//! inside `render()`.

pub mod actions;
pub mod chart;
pub mod chrome;
pub mod dialogs;
pub mod panels;
pub mod theme;
pub mod themes;