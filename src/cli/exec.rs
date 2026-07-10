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
//! Everything here is plain synchronous code — there is no async runtime. `main` is a sync
//! `fn main()` (see `src/main.rs`; the crate has no `tokio` dependency), and the engine blocks
//! the calling thread on each command (`ssh`/`sh` via `std::process`). The one place we run in
//! parallel is `ssh_exec_all`, which fans out across OS threads via `std::thread::scope`.

use crate::audit::{self, AuditEntry};
use crate::engine::context::SharedCtx;
use crate::engine::plan::PlannedAction;
use crate::engine::runner::RealRunner;
use crate::engine::state;
use crate::ssh::config::SshConfig;
use clap::Args;
use std::path::PathBuf;
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

/// Find the default orchestration file under `root`, returning its path as a string.
fn find_default_in(root: &std::path::Path) -> Option<String> {
    DEFAULT_FILES
        .iter()
        .map(|f| root.join(f))
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Resolve the orchestration file for any subcommand (the ONE place the search order lives, so a
/// fix lands everywhere — issue #24). An explicit `--file`/positional path wins (used as given).
/// Otherwise the default (`Energize.rhai`/`energize.rhai`) is looked up at the discovered PROJECT
/// ROOT first (so `nrg` works from a subdirectory — issue #19), then CWD. `hint` is appended to
/// the not-found error so each command shows its own usage example.
pub fn resolve_file(explicit: &Option<String>, hint: &str) -> Result<String, String> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }
    if let Ok(root) = state::find_project_root() {
        if let Some(p) = find_default_in(&root) {
            return Ok(p);
        }
    }
    if let Some(p) = find_default_file() {
        return Ok(p);
    }
    Err(format!("no Energize.rhai found. {hint}"))
}

/// Identifies the invocation for the audit trail (`src/audit.rs`) — everything about *which*
/// command ran that isn't already covered by `path`/`dry_run`.
pub struct AuditMeta<'a> {
    /// `"exec"` or `"run"`.
    pub command: &'a str,
    /// The called function name, for `nrg run`; `None` for `nrg exec`.
    pub target: Option<&'a str>,
    /// Positional args passed to the target function (empty for `nrg exec`).
    pub args: &'a [String],
}

/// The shared body of `nrg exec` and `nrg run`: wire the run, evaluate via `eval`, map the error
/// to an exit code, render the dry-run plan, and append an audit entry. Only the eval call
/// differs between the two commands, so they pass it in (issue #24) — keeping the dry-run
/// plan-print identical by construction.
pub fn execute_with(
    path: &str,
    dry_run: bool,
    meta: AuditMeta,
    eval: impl FnOnce(&std::path::Path, SharedCtx) -> Result<(), String>,
) -> i32 {
    let RunWiring { ctx, plan, root, _lock } = match wire_run(dry_run) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    // Cloned before `eval` consumes `ctx`: redaction of the audit entry below needs the
    // registered-secrets set that only accumulates as the script runs `secret()`.
    let ctx_for_audit = ctx.clone();
    let result = eval(std::path::Path::new(path), ctx);
    let code = match &result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    };
    if dry_run {
        print!("{}", crate::engine::plan::render_plan(&plan.lock().unwrap()));
    } else {
        // Dry runs write no state and take no lock (see `wire_run`); the audit trail follows
        // the same "plan touches nothing on disk" contract and only records LIVE runs.
        let outcome = match &result {
            Ok(()) => "success".to_string(),
            Err(e) => format!("failed: {}", ctx_for_audit.redacted(e)),
        };
        let redacted_target = meta.target.map(|t| ctx_for_audit.redacted(t));
        let redacted_args: Vec<String> = meta.args.iter().map(|a| ctx_for_audit.redacted(a)).collect();
        let entry = AuditEntry::new(
            meta.command,
            path,
            redacted_target.as_deref(),
            &redacted_args,
            outcome,
        );
        audit::append(&root, &entry);
    }
    code
}

/// The wiring a live/dry `nrg exec`/`nrg run` needs: a shared context, a handle to the plan
/// log (for dry-run rendering), the discovered project root (for the audit trail), and the
/// held advisory lock (kept alive for the run).
pub struct RunWiring {
    pub ctx: SharedCtx,
    pub plan: Arc<Mutex<Vec<PlannedAction>>>,
    pub root: PathBuf,
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
    let mut ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store, mode);
    // R7: connect the real SIGINT/SIGTERM-backed flag. `ctx` was just constructed, so this is
    // the only `Arc` reference — `get_mut` always succeeds here, no need for a constructor
    // signature change that would ripple into every test call site of `shared_with_state`.
    if let Some(rc) = Arc::get_mut(&mut ctx) {
        rc.interrupted = crate::engine::interrupt::install();
    }
    let plan = ctx.plan.clone();

    Ok(RunWiring {
        ctx,
        plan,
        root,
        _lock: held,
    })
}

/// Execute the `nrg exec` command. Returns the process exit code.
pub fn execute(args: &ExecArgs) -> i32 {
    let path = match resolve_file(&args.file, "Create one or pass a file:\n  nrg exec deploy.rhai") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let meta = AuditMeta { command: "exec", target: None, args: &[] };
    execute_with(&path, args.dry_run, meta, crate::engine::eval::run_file)
}
