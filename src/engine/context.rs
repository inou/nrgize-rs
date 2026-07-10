//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::plan::PlannedAction;
use crate::engine::runner::CommandRunner;
use crate::engine::sim::SimState;
use crate::engine::state::StateStore;
use rhai::FnPtr;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Active-transaction state: the compensation stack + nesting depth.
#[derive(Default)]
pub struct TxnState {
    pub comps: Vec<FnPtr>,
    pub depth: usize,
}

/// Whether side effects actually execute (Live) or are recorded only (DryRun, Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectMode {
    Live,
    DryRun,
}

/// State shared across one `nrg` invocation.
///
/// `mode` and `trace` are set ONCE at construction (before any script runs) and never mutated,
/// so they are plain fields. Every other field is already an `Arc<Mutex<…>>`, so the whole
/// `RunCtx` is shared immutably behind an `Arc` (`SharedCtx`) — there is no outer lock and no
/// `snapshot()` dance: a builtin reads `ctx.mode`/`ctx.runner`/… directly and clones the inner
/// `Arc` it needs (e.g. `ctx.runner.clone()`) before a blocking call, exactly as before but
/// without the per-builtin `ctx.lock().unwrap()`.
pub struct RunCtx {
    pub mode: EffectMode,
    /// The command runner. An `Arc` so a builtin can clone it and run a blocking command (or
    /// fan out across threads in `ssh_exec_all`) without holding any lock.
    pub runner: Arc<dyn CommandRunner>,
    /// The persistent state store (its own `Mutex` so disk I/O serializes).
    pub state: Arc<Mutex<StateStore>>,
    /// Plaintext values of resolved secrets, for trace/plan redaction.
    pub secrets: Arc<Mutex<HashSet<String>>>,
    /// The dry-run container/port/health overlay. Only mutated in DryRun mode; ignored in Live
    /// (each builtin probes for real).
    pub sim: Arc<Mutex<SimState>>,
    /// Recorded side effects, populated in DryRun mode.
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
    /// Compensation stack for transaction()/on_rollback().
    pub txn: Arc<Mutex<TxnState>>,
    pub trace: bool,
    /// Set by a SIGINT/SIGTERM handler (installed once for a live CLI run — see
    /// `engine::interrupt::install`); polled by the engine's `on_progress` hook (R7) so an
    /// interrupt aborts the running script as a normal `Err` — letting an enclosing
    /// transaction()'s unwind run — instead of the OS just killing the process. Defaults to a
    /// private flag that's never set (tests and any non-CLI path never receive real signals).
    pub interrupted: Arc<AtomicBool>,
}

impl RunCtx {
    fn build(runner: Arc<dyn CommandRunner>, state: StateStore, mode: EffectMode) -> Self {
        RunCtx {
            mode,
            runner,
            state: Arc::new(Mutex::new(state)),
            secrets: Arc::new(Mutex::new(HashSet::new())),
            sim: Arc::new(Mutex::new(SimState::default())),
            plan: Arc::new(Mutex::new(Vec::new())),
            txn: Arc::new(Mutex::new(TxnState::default())),
            trace: std::env::var("NRG_TRACE").is_ok(),
            interrupted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Register a resolved secret value for redaction.
    #[allow(dead_code)] // used by the secret() builtin (and tests)
    pub fn register_secret(&self, value: &str) {
        self.secrets.lock().unwrap().insert(value.to_string());
    }

    /// Whether this run records side effects instead of executing them (dry-run).
    pub fn is_dry_run(&self) -> bool {
        self.mode == EffectMode::DryRun
    }

    /// Redact `cmd` against the registered secret values (for trace output).
    pub fn redacted(&self, cmd: &str) -> String {
        crate::engine::secret::redact(cmd, &self.secrets.lock().unwrap())
    }

    /// Record a planned action (dry-run). Redacts the detail against the registered secret
    /// values HERE so the plan log (which prints to stdout, bypassing on_print) can never leak
    /// a `reveal()`'d secret — every call site is covered by this one boundary.
    pub fn record(&self, kind: &str, host: Option<&str>, detail: String) {
        let detail = crate::engine::secret::redact(&detail, &self.secrets.lock().unwrap());
        self.plan.lock().unwrap().push(PlannedAction {
            kind: kind.to_string(),
            host: host.map(|h| h.to_string()),
            detail,
        });
    }
}

/// Shared handle threaded into every builtin closure. No outer `Mutex`: `mode`/`trace` are
/// immutable and every other field carries its own lock.
pub type SharedCtx = Arc<RunCtx>;

/// Shared context with an EPHEMERAL (no-disk) store, Live mode — used by unit tests and any
/// non-state command path.
#[allow(dead_code)]
pub fn shared(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(RunCtx::build(runner, StateStore::ephemeral(), EffectMode::Live))
}

/// Shared context with an EPHEMERAL store in DryRun mode (tests + the dry-run path).
#[allow(dead_code)]
pub fn shared_dry(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(RunCtx::build(runner, StateStore::ephemeral(), EffectMode::DryRun))
}

/// Shared context with a real, loaded on-disk store in the given mode (used by `nrg exec`).
pub fn shared_with_state(
    runner: Arc<dyn CommandRunner>,
    state: StateStore,
    mode: EffectMode,
) -> SharedCtx {
    Arc::new(RunCtx::build(runner, state, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn ctx_defaults_to_live_with_ephemeral_state() {
        let ctx = shared(FakeRunner::shared());
        assert_eq!(ctx.mode, EffectMode::Live);
        assert!(!ctx.is_dry_run());
        assert!(ctx.state.lock().unwrap().all().is_empty());
    }

    #[test]
    fn dry_constructor_sets_dry_run() {
        let ctx = shared_dry(FakeRunner::shared());
        assert!(ctx.is_dry_run());
    }

    #[test]
    fn sim_handle_is_shared_through_the_arc() {
        let ctx = shared(FakeRunner::shared());
        ctx.sim.lock().unwrap().set_running("web1", "app", "img");
        assert!(ctx.sim.lock().unwrap().is_running("web1", "app"));
    }
}
