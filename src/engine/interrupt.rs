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

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Install SIGINT/SIGTERM handlers that flip a shared flag. Best-effort: if registration fails
/// (an exotic platform, or a process already at its signal-handler limit), the flag simply
/// never gets set and `nrg` behaves exactly as it did before this fix — no crash, no panic.
pub fn install() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag));
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
