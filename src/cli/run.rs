//! `nrg run <fn> [args...]` — call a Rhai function defined in `Energize.rhai`.
//!
//! Discovers the orchestration file (like `nrg exec`), does the same
//! lock + root-discovery + state + context wiring, then calls the named function via
//! `engine::eval::run_fn`. Trailing CLI args are passed as Rhai strings. With `--dry-run`
//! the run records its side effects instead of executing them and prints the plan.

use crate::cli::exec::{execute_with, resolve_file, AuditMeta};
use clap::Args;

#[derive(Args)]
pub struct RunArgs {
    /// Function name to call in the Rhai orchestration file.
    pub target: String,

    /// Positional arguments passed to the Rhai function (as strings). Flags like `--dry-run`
    /// may appear anywhere; a function argument that itself starts with `-` must be given
    /// after a `--` separator.
    pub fn_args: Vec<String>,

    /// Path to the `.rhai` file. Defaults to Energize.rhai.
    #[arg(long)]
    pub file: Option<String>,

    /// Show the plan of side effects without executing (no lock, no state writes).
    #[arg(long)]
    pub dry_run: bool,
}

/// Execute the `nrg run` command. Returns the process exit code.
pub fn execute(args: &RunArgs) -> i32 {
    let path = match resolve_file(
        &args.file,
        "Create one or pass a file:\n  nrg run <fn> --file deploy.rhai",
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let meta = AuditMeta {
        command: "run",
        target: Some(&args.target),
        args: &args.fn_args,
    };
    execute_with(&path, args.dry_run, meta, |p, ctx| {
        crate::engine::eval::run_fn(p, &args.target, &args.fn_args, ctx)
    })
}
