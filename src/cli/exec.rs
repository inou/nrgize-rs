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

    /// Give up waiting for the state lock after this many seconds (another `nrg` run holding it
    /// is reported as an error instead of blocking forever). Default: wait indefinitely.
    #[arg(long)]
    pub lock_timeout: Option<u64>,

    /// Namespace this run's state (and its `.energize/secrets.<dest>` file) under a destination
    /// (e.g. `staging`, `production`), so two environments deployed from the same directory don't
    /// share one state keyspace. Letters, digits, `-`, `_` only. Defaults to the unnamespaced
    /// destination — behaves exactly as if this flag didn't exist.
    #[arg(long)]
    pub dest: Option<String>,
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
    lock_timeout: Option<std::time::Duration>,
    dest: Option<String>,
    meta: AuditMeta,
    eval: impl FnOnce(&std::path::Path, SharedCtx) -> Result<(), String>,
) -> i32 {
    let RunWiring { ctx, plan, root, _lock } = match wire_run(dry_run, lock_timeout, dest) {
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

/// How often to re-poll the lock while a `--lock-timeout` is in effect. Short enough that the
/// reported wait time never overshoots the requested timeout by more than a blink.
const LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wait (up to `timeout`) until `lock`'s exclusive write lock is observed uncontended, printing
/// a one-time "waiting" message. Never itself returns (or holds) the guard — the caller takes
/// it with one final, single `try_write()` immediately after this returns `Ok(())`.
///
/// This split exists purely for the borrow checker: a loop or recursive helper that *itself*
/// sometimes returns a `Guard` borrowed from `lock` and otherwise keeps reusing `lock` to retry
/// hits a known NLL limitation (rust-lang/rust#54663) — the single `try_write()` call site gets
/// its reborrow of `*lock` forced to last as long as the function's whole input lifetime
/// (because *some* path returns a `Guard` tied to it), which then conflicts with any later reuse
/// of `lock`, on every path, not just the one that returns. A loop that only ever asks
/// `.is_ok()` — never binding or returning the `Guard` — never creates that forced lifetime, so
/// it retries freely; the real, single `try_write()` call that actually produces the returned
/// `Guard` happens exactly once, outside any loop, in `wire_run` below.
fn wait_until_lock_available(
    lock: &mut fd_lock::RwLock<std::fs::File>,
    timeout: std::time::Duration,
    root: &std::path::Path,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    let mut printed_waiting = false;
    loop {
        // `.is_ok()` — if this momentarily acquires the lock to check, the temporary guard
        // drops (releasing it again) at the end of this statement; that's fine, since we only
        // want to know "was it free just now", not hold it across iterations.
        if lock.try_write().is_ok() {
            return Ok(());
        }
        if !printed_waiting {
            eprintln!(
                "Waiting for the state lock (another `nrg` run is in progress under {}, timeout \
                 {}s)...",
                root.display(),
                timeout.as_secs()
            );
            printed_waiting = true;
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(format!(
                "timed out after {}s waiting for the state lock under {} — another `nrg` run \
                 appears to be holding it; pass a longer --lock-timeout, or investigate/stop \
                 the other run",
                timeout.as_secs(),
                root.display()
            ));
        }
        std::thread::sleep(std::cmp::min(LOCK_POLL_INTERVAL, timeout - elapsed));
    }
}

/// Resolve the project root, take the advisory state lock (unless dry-run or re-entrant), load
/// the state store, and build the shared engine context. This is the common entry wiring for
/// both `nrg exec` and `nrg run`. `lock_timeout` bounds how long to wait for a contended lock —
/// `None` waits indefinitely (the original, and still default, behavior).
pub fn wire_run(
    dry_run: bool,
    lock_timeout: Option<std::time::Duration>,
    dest: Option<String>,
) -> Result<RunWiring, String> {
    // "default" is not special-cased here — it's already all ASCII-alphanumeric, so it passes
    // `is_valid_dest_name` on its own merit. The magic "means no destination" behavior lives in
    // `StateStore::with_dest`, not here (Opus review, round 7: an earlier version of this check
    // had a redundant/misleading `d != "default"` guard implying otherwise).
    if let Some(d) = &dest {
        if !state::is_valid_dest_name(d) {
            return Err(format!(
                "invalid --dest {d:?}: must be non-empty and contain only letters, digits, '-', \
                 or '_'"
            ));
        }
    }
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
            let guard = match lock_timeout {
                None => {
                    // Probe without blocking so we can tell the user we're waiting; then take
                    // the real (blocking) exclusive lock. `.write()` errors only on a syscall
                    // failure.
                    if lock.try_write().is_err() {
                        eprintln!(
                            "Waiting for the state lock (another `nrg` run is in progress under \
                             {})...",
                            root.display()
                        );
                    }
                    lock.write().map_err(|e| {
                        format!("cannot acquire state lock under {}: {e}", root.display())
                    })?
                }
                Some(timeout) => {
                    wait_until_lock_available(lock, timeout, &root)?;
                    // Single, un-looped acquire immediately after observing it free. In the
                    // vanishingly unlikely case another process grabs it in that exact gap, this
                    // surfaces as a plain error rather than silently blocking again past the
                    // timeout the caller already agreed to wait — a rerun succeeds normally.
                    lock.try_write().map_err(|e| {
                        format!(
                            "state lock under {} became contended again immediately after \
                             becoming available; rerun the command: {e}",
                            root.display()
                        )
                    })?
                }
            };
            std::env::set_var(state::LOCK_ENV, state::lock_env_value(&key));
            HeldLock(Some(guard))
        }
    };

    let store = if dry_run {
        state::StateStore::load_overlay(&root)?
    } else {
        state::StateStore::load(&root)?
    }
    .with_dest(dest);

    let mode = if dry_run {
        crate::engine::context::EffectMode::DryRun
    } else {
        crate::engine::context::EffectMode::Live
    };
    let mut ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner), store, mode);
    // R7: connect the real SIGINT/SIGTERM-backed flag. `ctx` was just constructed, so this is
    // the only `Arc` reference — `get_mut` always succeeds here, no need for a constructor
    // signature change that would ripple into every test call site of `shared_with_state`.
    // `.expect(...)`, not a silent `if let Some`: if a future refactor ever clones `ctx` before
    // this line, this must fail loudly rather than quietly leaving the interrupt handler
    // disconnected from the real signal (R7 would silently stop working, with no test catching
    // it except the slow end-to-end one in tests/interrupt.rs).
    Arc::get_mut(&mut ctx)
        .expect("ctx was just constructed; no other Arc clone exists yet")
        .interrupted = crate::engine::interrupt::install();
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
    execute_with(
        &path,
        args.dry_run,
        args.lock_timeout.map(std::time::Duration::from_secs),
        args.dest.clone(),
        meta,
        crate::engine::eval::run_file,
    )
}
