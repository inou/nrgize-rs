//! `nrg run <fn> [args...]` — call a Rhai function defined in `Energize.rhai`.
//!
//! Discovers the orchestration file (like `nrg exec`), does the same
//! lock + root-discovery + state + context wiring, then calls the named function via
//! `engine::eval::run_fn`. Trailing CLI args are passed as Rhai strings. With `--dry-run`
//! the run records its side effects instead of executing them and prints the plan.

use crate::cli::exec::{find_default_file, wire_run, RunWiring};
use clap::Args;

#[derive(Args)]
pub struct RunArgs {
    /// Function name to call in the Rhai orchestration file.
    pub target: String,

    /// Positional arguments passed to the Rhai function (as strings).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
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
    let path = match args.file.clone().or_else(find_default_file) {
        Some(p) => p,
        None => {
            eprintln!(
                "Error: no Energize.rhai found. Create one or pass a file:\n  nrg run <fn> --file deploy.rhai"
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

    let code = match crate::engine::eval::run_fn(
        std::path::Path::new(&path),
        &args.target,
        &args.fn_args,
        ctx,
    ) {
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
