//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::runner::CommandRunner;
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
    pub trace: bool,
}

impl RunCtx {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        RunCtx {
            mode: EffectMode::Live,
            runner,
            trace: std::env::var("NRG_TRACE").is_ok(),
        }
    }
}

/// Shared, lockable handle threaded into every builtin closure.
pub type SharedCtx = Arc<Mutex<RunCtx>>;

/// Build a shared context with the given command runner.
pub fn shared(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::new(runner)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn ctx_defaults_to_live() {
        let ctx = shared(FakeRunner::shared());
        assert_eq!(ctx.lock().unwrap().mode, EffectMode::Live);
    }
}
