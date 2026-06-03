//! `nrg exec` — evaluate a Rhai orchestration module top-to-bottom. Builtins
//! (`ssh_exec`, `http_get`, …) have real side effects as evaluation reaches them.
//!
//! Supports `import "lib/module" as m;` for importing from other `.rhai` files,
//! resolved relative to the directory of the file being executed.

use crate::engine::context::shared;
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

    let ssh = SshConfig::load_default();
    let ctx = shared(Arc::new(RealRunner { ssh }));

    match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}
