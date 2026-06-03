//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::runner::CommandRunner;
use crate::engine::state::StateStore;
use std::sync::{Arc, Mutex};

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
    pub trace: bool,
}

impl RunCtx {
    fn build(runner: Arc<dyn CommandRunner>, state: StateStore) -> Self {
        RunCtx {
            mode: EffectMode::Live,
            runner,
            state: Arc::new(Mutex::new(state)),
            trace: std::env::var("NRG_TRACE").is_ok(),
        }
    }
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
