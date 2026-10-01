//! Key actions and their bindings.
//!
//! # The three moving parts
//!
//! 1. Declare the action with [`actions!`].
//! 2. Bind a key to it with `cx.bind_keys([KeyBinding::new("key", Action, None)])`.
//! 3. Handle it in `render()` with `.on_key_down(..)` or `.on_action(..)`.
//!
//! All three are required. Declaring and handling without a binding means the
//! key does nothing; a binding without a handler is silently swallowed.

use gpui_kit::actions;

actions!(
    btop_gpui,
    [
        /// Toggle the help overlay.
        ToggleHelp,
        /// Step to the next layout preset.
        NextPreset,
        /// Step to the previous layout preset.
        PrevPreset,
        /// Toggle the process tree.
        ToggleTree,
        /// Cycle the process sort column.
        CycleSort,
        /// Reverse the process sort direction.
        ReverseSort,
        /// Close any open dialog or overlay.
        Dismiss,
    ]
);

use gpui_kit::{App, KeyBinding};

/// Every binding, as `(key, description)`, in the order the help overlay
/// lists them. Kept in one place so the overlay cannot drift from the
/// registrations below.
pub fn key_bindings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("?", "toggle help"),
        ("Esc", "close dialog"),
        ("p", "next layout preset"),
        ("Shift-P", "previous layout preset"),
        ("T", "toggle process tree"),
        ("c", "cycle process sort column"),
        ("r", "reverse process sort"),
        ("f", "toggle process filter"),
        ("t", "terminate selected process"),
        ("k", "kill selected process"),
        ("+", "raise nice value"),
        ("-", "lower nice value"),
    ]
}

/// Register the bindings. Called once, when the root view is built.
pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("question-mark", ToggleHelp, None),
        KeyBinding::new("escape", Dismiss, None),
        KeyBinding::new("p", NextPreset, None),
        KeyBinding::new("shift-p", PrevPreset, None),
        KeyBinding::new("shift-t", ToggleTree, None),
        KeyBinding::new("c", CycleSort, None),
        KeyBinding::new("r", ReverseSort, None),
    ]);
}

/// Send a signal to a pid, turning an errno into something showable.
///
/// `EPERM` is the common one and is not an app error — it just means the
/// process belongs to someone else.
pub fn signal_result(pid: i32, sig: nix::sys::signal::Signal) -> Result<String, String> {
    match crate::collect::proc::send_signal(pid, sig) {
        Ok(()) => Ok(format!("sent {sig:?} to {pid}")),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            Err(format!("not permitted to signal {pid}"))
        }
        Err(e) => Err(format!("could not signal {pid}: {e}")),
    }
}

/// Change a pid's nice value, again mapping errno to a message.
pub fn nice_result(pid: i32, nice: i32) -> Result<String, String> {
    match crate::collect::proc::set_nice(pid, nice) {
        Ok(()) => Ok(format!("nice of {pid} set to {nice}")),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
            Err(format!("not permitted to renice {pid}"))
        }
        Err(e) => Err(format!("could not renice {pid}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_binding_has_a_description() {
        for (key, what) in key_bindings() {
            assert!(!key.is_empty());
            assert!(!what.is_empty(), "{key} has no description");
        }
    }

    #[test]
    fn a_signal_to_our_own_pid_succeeds() {
        let pid = std::process::id() as i32;
        // SIGCONT on ourselves is harmless and must be permitted.
        assert!(signal_result(pid, nix::sys::signal::Signal::SIGCONT).is_ok());
    }

    #[test]
    fn signalling_a_nonexistent_pid_reports_an_error_rather_than_panicking() {
        // PID 0 is the process group; a very high pid is almost certainly free.
        let result = signal_result(i32::MAX - 1, nix::sys::signal::Signal::SIGTERM);
        assert!(result.is_err(), "expected ESRCH, got {result:?}");
    }
}