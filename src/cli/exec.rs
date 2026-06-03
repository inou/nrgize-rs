//! `nrg exec` — evaluate a Rhai orchestration module top-to-bottom. Builtins
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
//! runtime during `nrg exec` (the async `task_runner` is only on the legacy `run` path).
//! If `exec` ever shares the runtime with async work, offload via `block_in_place` /
//! `spawn_blocking`.

use crate::engine::runner::RealRunner;
use crate::ssh::config::SshConfig;
use clap::Args;
use std::sync::Arc;

/// Default search order for the orchestration file.
const DEFAULT_FILES: &[&str] = &["Energize.rhai", "energize.rhai"];

#[derive(Args)]
pub struct ExecArgs {
    /// Path to the `.rhai` file to evaluate. Defaults to Energize.rhai.
    pub file: Option<String>,
}

fn find_default() -> Option<String> {
    DEFAULT_FILES
        .iter()
        .find(|f| std::path::Path::new(f).exists())
        .map(|s| s.to_string())
}

/// Execute the `nrg exec` command. Returns the process exit code.
pub fn execute(args: &ExecArgs) -> i32 {
    let path = match args.file.clone().or_else(find_default) {
        Some(p) => p,
        None => {
            eprintln!(
                "Error: no Energize.rhai found. Create one or pass a file:\n  nrg exec deploy.rhai"
            );
            return 1;
        }
    };

    use crate::engine::state;

    // Discover the project root and serialize concurrent mutating runs with an advisory
    // flock — UNLESS an ancestor `nrg` already holds it (re-entrancy), to avoid deadlock.
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let key = state::lock_key(&root);
    let reentrant =
        state::lock_is_reentrant(&key, std::env::var(state::LOCK_ENV).ok().as_deref());

    // Keep both the RwLock and its guard alive for the whole run (the guard borrows the
    // RwLock, so they must share this stack frame — do not move them into a struct).
    let mut lock_holder = if reentrant {
        None
    } else {
        match state::open_lock(&root) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("Error: cannot open state lock under {}: {e}", root.display());
                return 1;
            }
        }
    };
    let _guard = match lock_holder.as_mut() {
        Some(l) => match l.write() {
            Ok(g) => Some(g),
            Err(_) => {
                eprintln!(
                    "Error: another `nrg` run is in progress (state lock held under {}).",
                    root.display()
                );
                return 1;
            }
        },
        None => None,
    };
    if !reentrant {
        std::env::set_var(state::LOCK_ENV, &key);
    }

    let store = match state::StateStore::load(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let ssh = SshConfig::load_default();
    let ctx =
        crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store);

    match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}
