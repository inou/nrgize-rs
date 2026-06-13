//! `nrg exec [file]` — evaluate a Rhai orchestration module top-to-bottom. Builtins
//! (`ssh_exec`, `http_get`, …) have real side effects as evaluation reaches them.
//!
//! Supports `import "lib/module" as m;` for importing from other `.rhai` files,
//! resolved relative to the directory of the file being executed.
//!
//! ## Failure contract (exit codes)
//!
//! Exec builtins fold a non-zero command into `ExecResult.ok == false`; they do **not**
//! abort the script by themselves. A script signals failure by `throw`ing (an uncaught
//! `throw` — or a Rhai parse error — surfaces from `run_file` as `Err`, which this
//! command maps to exit code 1). The standard library wraps every fallible call with an
//! `if !r.ok { throw … }` check, so real deploys exit non-zero on failure. A hand-written
//! script that runs `ssh_exec(...)` and ignores `r.ok` exits 0 — by design: it chose not
//! to check. Automation that cares about command failure must either use the stdlib or
//! check `.ok` and `throw`.
//!
//! ## Concurrency note
//!
//! `main` is `#[tokio::main]`, but `execute` is synchronous and the engine blocks the
//! calling worker thread (blocking `ssh`/`sh` via `std::process`, `ssh_exec_all` via
//! `std::thread::scope`). This is acceptable today because nothing else uses the tokio
//! runtime during `nrg exec`/`nrg run`. If these ever share the runtime with async work,
//! offload via `block_in_place` / `spawn_blocking`.

use crate::engine::context::SharedCtx;
use crate::engine::plan::PlannedAction;
use crate::engine::runner::RealRunner;
use crate::engine::state;
use crate::ssh::config::SshConfig;
use clap::Args;
use std::sync::{Arc, Mutex};

/// Default search order for the orchestration file (shared by `nrg exec` and `nrg run`).
const DEFAULT_FILES: &[&str] = &["Energize.rhai", "energize.rhai"];

#[derive(Args)]
pub struct ExecArgs {
    /// Path to the `.rhai` file to evaluate. Defaults to Energize.rhai.
    pub file: Option<String>,

    /// Show the plan of side effects without executing (no lock, no state writes).
    #[arg(long)]
    pub dry_run: bool,
}

/// Find the default orchestration file in the current directory, if any.
pub fn find_default_file() -> Option<String> {
    DEFAULT_FILES
        .iter()
        .find(|f| std::path::Path::new(f).exists())
        .map(|s| s.to_string())
}

/// The wiring a live/dry `nrg exec`/`nrg run` needs: a shared context, a handle to the plan
/// log (for dry-run rendering), and the held advisory lock (kept alive for the run).
pub struct RunWiring {
    pub ctx: SharedCtx,
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
    /// The held lock guard (and its backing `RwLock`), kept alive for the duration of the run.
    /// `None` in dry-run or re-entrant invocations. The field is read only for its `Drop`.
    pub _lock: HeldLock,
}

/// An advisory state lock held for the lifetime of a live run.
///
/// The lock file and its write guard are leaked so the guard can live for `'static` — the lock
/// is released when the process exits anyway. `None` for dry-run / re-entrant invocations.
#[allow(dead_code)] // held only for its lifetime / Drop effect
pub struct HeldLock(Option<fd_lock::RwLockWriteGuard<'static, std::fs::File>>);

/// Resolve the project root, take the advisory state lock (unless dry-run or re-entrant), load
/// the state store, and build the shared engine context. This is the common entry wiring for
/// both `nrg exec` and `nrg run`.
pub fn wire_run(dry_run: bool) -> Result<RunWiring, String> {
    let root = state::find_project_root()?;

    // Dry-run takes NO lock and writes NO state (uses an in-memory overlay). A live run
    // serializes concurrent mutating runs with an advisory flock — unless an ancestor `nrg`
    // already holds it (re-entrancy), to avoid self-deadlock.
    let held = if dry_run {
        HeldLock(None)
    } else {
        let key = state::lock_key(&root);
        let reentrant =
            state::lock_is_reentrant(&key, std::env::var(state::LOCK_ENV).ok().as_deref());
        if reentrant {
            HeldLock(None)
        } else {
            let lock = state::open_lock(&root)
                .map_err(|e| format!("cannot open state lock under {}: {e}", root.display()))?;
            // Leak the lock so the write guard can be `'static` (held for the whole process).
            let lock: &'static mut fd_lock::RwLock<std::fs::File> = Box::leak(Box::new(lock));
            // Probe without blocking so we can tell the user we're waiting; then take the real
            // (blocking) exclusive lock. `.write()` errors only on a syscall failure.
            if lock.try_write().is_err() {
                eprintln!(
                    "Waiting for the state lock (another `nrg` run is in progress under {})...",
                    root.display()
                );
            }
            let guard = lock
                .write()
                .map_err(|e| format!("cannot acquire state lock under {}: {e}", root.display()))?;
            std::env::set_var(state::LOCK_ENV, &key);
            HeldLock(Some(guard))
        }
    };

    let store = if dry_run {
        state::StateStore::load_overlay(&root)?
    } else {
        state::StateStore::load(&root)?
    };

    let ssh = SshConfig::load_default();
    let mode = if dry_run {
        crate::engine::context::EffectMode::DryRun
    } else {
        crate::engine::context::EffectMode::Live
    };
    let ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store, mode);
    let plan = ctx.plan.clone();

    Ok(RunWiring {
        ctx,
        plan,
        _lock: held,
    })
}

/// Execute the `nrg exec` command. Returns the process exit code.
pub fn execute(args: &ExecArgs) -> i32 {
    let path = match args.file.clone().or_else(find_default_file) {
        Some(p) => p,
        None => {
            eprintln!(
                "Error: no Energize.rhai found. Create one or pass a file:\n  nrg exec deploy.rhai"
            );
            return 1;
        }
    };

    let RunWiring { ctx, plan, _lock } = match wire_run(args.dry_run) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let code = match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    };
    if args.dry_run {
        print!("{}", crate::engine::plan::render_plan(&plan.lock().unwrap()));
    }
    code
}
