//! SIGINT/SIGTERM handling (robustness review R7).
//!
//! Without this, Ctrl-C during a live run kills the process with **zero** cleanup: no
//! `on_rollback` compensations run, orphaned `<service>-web-<ver>-<port>` containers and
//! half-switched proxy targets are left behind, and the state lock is released only because
//! the OS reclaims the fd on process death, not because anything asked for it.
//!
//! The fix: install a signal handler that flips a shared flag (via `signal_hook`'s
//! async-signal-safe `flag::register`, so we do no unsafe work of our own), and have the Rhai
//! engine poll that flag via `Engine::on_progress` (called on every script-level operation). A
//! set flag terminates the script with a normal `Err` — the SAME path a `throw` takes — so an
//! enclosing `transaction()`'s existing unwind machinery (`src/engine/transaction.rs`) runs
//! every registered compensation, LIFO, before the process actually exits. The held state lock
//! (`RunWiring::_lock` in `src/cli/exec.rs`) then releases via its normal `Drop`, not because
//! the OS killed the process.
//!
//! SCOPE: `on_progress` is checked BETWEEN Rhai-level operations, not preemptively during one
//! blocking native call. A `for` loop (e.g. `healthcheck.rhai`'s retry loop, bounded by a few
//! seconds of `sleep()` per iteration) responds within about one iteration. A single long- or
//! forever-blocking `ssh_exec`/`local_exec`/`http_get` call can't be interrupted mid-flight —
//! the check only fires once that call returns. That's a separate, still-open gap (command
//! timeouts; see docs/robustness-review.md).
//!
//! FORCE-QUIT ESCAPE HATCH: because installing a handler for a signal REPLACES its default
//! "terminate immediately" disposition, a signal delivered while nrg is stuck inside one of
//! those un-preemptible blocking calls would otherwise just set the flag and vanish — nothing
//! polls it until the blocking call returns, so the operator's Ctrl-C appears to do nothing,
//! where the OLD (no-handler) behavior would have killed the process outright. To keep that
//! escape hatch, a SECOND SIGINT/SIGTERM (received after the first already armed the flag)
//! terminates the process immediately via `signal_hook::flag::register_conditional_shutdown` —
//! the exact "double Ctrl-C: first tries to shut down gracefully, second forces it" pattern
//! that function's own docs describe. One signal = try to unwind gracefully; two = give up and
//! exit now.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Install SIGINT/SIGTERM handlers that flip a shared flag, with a force-quit escape hatch on a
/// second signal (see the module doc). Best-effort: if registration fails (an exotic platform,
/// or a process already at its signal-handler limit), the flag simply never gets set and `nrg`
/// behaves exactly as it did before this fix — no crash, no panic.
pub fn install() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        // (signal, the conventional 128+signum exit code a shell reports for it)
        for (sig, exit_status) in [
            (signal_hook::consts::SIGINT, 130),
            (signal_hook::consts::SIGTERM, 143),
        ] {
            // ORDER MATTERS (per register_conditional_shutdown's own docs): the shutdown check
            // must be registered BEFORE the flag-setter, so on a first signal it sees the flag
            // still false (no-op) and only the flag-setter runs; on a SECOND signal it sees the
            // flag the first signal already set and exits immediately.
            let _ = signal_hook::flag::register_conditional_shutdown(
                sig,
                exit_status,
                Arc::clone(&flag),
            );
            let _ = signal_hook::flag::register(sig, Arc::clone(&flag));
        }
    }
    flag
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn install_returns_a_flag_that_starts_false() {
        let flag = install();
        assert!(!flag.load(Ordering::Relaxed));
    }
}
