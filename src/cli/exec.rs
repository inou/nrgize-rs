//! The `nrg exec` subcommand — evaluate a Starlark file in script/orchestration mode.
//!
//! Unlike `nrg run <task>` which looks up a named task and executes it through
//! the task runner, `nrg exec` evaluates the entire Starlark file with runtime
//! primitives available. Top-level calls to ssh_exec(), local_exec(), etc. are
//! executed as the file is evaluated — Starlark IS the orchestration engine.
//!
//! Supports `load("module.star", "symbol")` for importing from other .star files.

use crate::runtime;
use crate::runtime::loader::NrgFileLoader;
use clap::Args;
use dupe::Dupe;
use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

/// Default search order for Energize files (only .star for exec mode).
const DEFAULT_STAR_FILES: &[&str] = &["Energize.star", "energize.star"];

#[derive(Args)]
pub struct ExecArgs {
    /// Path to the .star file to evaluate. Defaults to Energize.star.
    pub file: Option<String>,
}

/// Find a default Starlark file in the current directory.
fn find_star_file() -> Option<String> {
    for name in DEFAULT_STAR_FILES {
        if std::path::Path::new(name).exists() {
            return Some(name.to_string());
        }
    }
    None
}

/// Execute the `nrg exec` command. Returns process exit code.
pub fn execute(args: &ExecArgs) -> i32 {
    let path = match &args.file {
        Some(f) => f.clone(),
        None => match find_star_file() {
            Some(f) => f,
            None => {
                eprintln!(
                    "Error: No Energize.star found. Create one or specify a file:\n  nrg exec deploy.star"
                );
                return 1;
            }
        },
    };

    if !path.ends_with(".star") {
        eprintln!(
            "Error: Only .star files are supported in exec mode. Got: {}\n\
             Hint: Use `nrg run <task>` for running named tasks.",
            path
        );
        return 1;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: Failed to read {}: {}", path, e);
            return 1;
        }
    };

    let trace = std::env::var("NRG_TRACE").is_ok();
    if trace {
        eprintln!("[nrg] exec mode: evaluating {}", path);
    }

    // Build globals with standard Starlark builtins + runtime primitives.
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
        .with(runtime::register_all)
        .build();

    // Parse the Starlark file.
    let ast = match AstModule::parse(&path, content, &Dialect::Extended) {
        Ok(ast) => ast,
        Err(e) => {
            eprintln!("Parse error in {}:\n{}", path, e);
            return 1;
        }
    };

    // Resolve the base directory for load() statements.
    let abs_path = std::path::Path::new(&path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(&path));
    let base_dir = abs_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    // Create the file loader for load() support.
    let loader = NrgFileLoader::new(base_dir, globals.dupe(), trace);

    // Create module and evaluator.
    let module = Module::new();
    let mut eval = Evaluator::new(&module);
    eval.set_loader(&loader);

    // Evaluate — side-effectful built-in calls execute as Starlark reaches them.
    match eval.eval_module(ast, &globals) {
        Ok(_) => {
            if trace {
                eprintln!("[nrg] exec completed successfully");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    }
}
