//! Key actions and their descriptions.
//!
//! # There is deliberately no `actions!` block here
//!
//! There used to be, with a `register()` that called `cx.bind_keys`. It never
//! worked: `main.rs` never called `register()`, and nothing handled the actions
//! with `on_action`. The result was that `p`, `T`, `c` and `r` appeared in the
//! help overlay and did nothing when pressed — the overlay was simply lying.
//!
//! Rather than keep two half-wired mechanisms, the app dispatches raw keys in
//! `AppView::on_key_down`, one place that provably runs. This module keeps
//! [`key_bindings`] as the single source of truth for what the overlay lists,
//! plus the signal and renice helpers those keys call.

/// Every key the app handles, as `(key, description)`, in the order the help
/// overlay lists them.
///
/// This must stay in step with `AppView::on_key_down` — the two cannot be tied
/// together by the compiler, so a key added in one place has to be added here
/// too or the overlay starts lying again.
pub fn key_bindings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("?", "toggle help"),
        ("h", "toggle help"),
        ("Esc", "close dialog"),
        ("m", "open the options menu"),
        ("q", "quit"),
        ("p", "next layout preset"),
        ("Shift-P", "previous layout preset"),
        ("e", "toggle process tree"),
        ("f", "filter the process list"),
        ("Del", "clear the filter"),
        ("r", "reverse the sort order"),
        ("Shift-D", "cycle theme: system / dark / light"),
        ("Enter", "show the selected process"),
        ("t", "terminate selected process (asks first)"),
        ("k", "kill selected process (asks first)"),
        ("+", "raise nice value"),
        ("-", "lower nice value"),
    ]
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
