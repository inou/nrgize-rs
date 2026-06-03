//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::plan::PlannedAction;
use crate::engine::runner::CommandRunner;
use crate::engine::state::StateStore;
use rhai::FnPtr;
use std::collections::HashSet;
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
pub struct RunCtx {
    pub mode: EffectMode,
    /// In an `Arc` (separate from the `RunCtx`'s own lock) so a builtin can clone it and
    /// release the lock BEFORE the blocking command — enabling real parallelism in
    /// `ssh_exec_all`.
    pub runner: Arc<dyn CommandRunner>,
    /// In its own `Arc<Mutex>` so a builtin can snapshot it out of the `RunCtx` lock before
    /// touching disk (mirrors the runner pattern).
    pub state: Arc<Mutex<StateStore>>,
    /// Plaintext values of resolved secrets, for trace/plan redaction.
    pub secrets: Arc<Mutex<HashSet<String>>>,
    /// Recorded side effects, populated in DryRun mode.
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
    /// Compensation stack for transaction()/on_rollback().
    pub txn: Arc<Mutex<TxnState>>,
    pub trace: bool,
}

impl RunCtx {
    fn build(runner: Arc<dyn CommandRunner>, state: StateStore) -> Self {
        RunCtx {
            mode: EffectMode::Live,
            runner,
            state: Arc::new(Mutex::new(state)),
            secrets: Arc::new(Mutex::new(HashSet::new())),
            plan: Arc::new(Mutex::new(Vec::new())),
            txn: Arc::new(Mutex::new(TxnState::default())),
            trace: std::env::var("NRG_TRACE").is_ok(),
        }
    }

    /// Register a resolved secret value for redaction.
    #[allow(dead_code)] // used by the secret() builtin (and tests)
    pub fn register_secret(&self, value: &str) {
        self.secrets.lock().unwrap().insert(value.to_string());
    }

    /// A consistent snapshot of the shared handles, taken under a short lock and then released
    /// so builtins never hold the `RunCtx` lock across a blocking command.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            mode: self.mode,
            runner: self.runner.clone(),
            state: self.state.clone(),
            secrets: self.secrets.clone(),
            trace: self.trace,
        }
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

/// A point-in-time copy of the shared handles (see `RunCtx::snapshot`).
pub struct Snapshot {
    pub mode: EffectMode,
    pub runner: Arc<dyn CommandRunner>,
    pub state: Arc<Mutex<StateStore>>,
    pub secrets: Arc<Mutex<HashSet<String>>>,
    pub trace: bool,
}

/// Shared, lockable handle threaded into every builtin closure.
pub type SharedCtx = Arc<Mutex<RunCtx>>;

/// Shared context with an EPHEMERAL (no-disk) store — used by unit tests now, and by the
/// dry-run (P3) and `nrg run` (P5) paths that don't load real state.
#[allow(dead_code)] // wired to non-test callers in P3/P5
pub fn shared(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::build(runner, StateStore::ephemeral())))
}

/// Shared context with a real, loaded on-disk store (used by `nrg exec`).
pub fn shared_with_state(runner: Arc<dyn CommandRunner>, state: StateStore) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::build(runner, state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn ctx_defaults_to_live_with_ephemeral_state() {
        let ctx = shared(FakeRunner::shared());
        let g = ctx.lock().unwrap();
        assert_eq!(g.mode, EffectMode::Live);
        assert!(g.state.lock().unwrap().all().is_empty());
    }
}
