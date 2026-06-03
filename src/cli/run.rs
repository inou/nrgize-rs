use clap::Args;
use crossterm::style::Stylize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::cli::ui;
use crate::execution::executor::{self, ExecuteOptions};
use crate::execution::ssh_command;
use crate::execution::task_runner::OutputCallback;
use crate::parsing;
use crate::ssh::config::SshConfig;

#[derive(Args)]
pub struct RunArgs {
    /// Task/macro name (Starlark) or function name (Rhai) to execute
    pub target: String,

    /// Positional arguments passed to the Rhai function (ignored for Starlark tasks)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub fn_args: Vec<String>,

    /// Don't stop on first task failure
    #[arg(long = "continue")]
    pub continue_on_error: bool,

    /// Dry-run: print SSH commands without executing
    #[arg(long)]
    pub pretend: bool,

    /// Explicit file path
    #[arg(long)]
    pub path: Option<String>,

    /// Filename in current directory
    #[arg(long)]
    pub conf: Option<String>,

    /// Hide real-time output, show only result table
    #[arg(long)]
    pub summary: bool,

    /// Pass variables (repeatable), format: key=value
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub vars: Vec<String>,

    /// Load environment variables from an .env file
    #[arg(long = "env", value_name = "FILE")]
    pub env_file: Option<String>,
}

/// Build a streaming output callback that prints lines to the terminal.
fn make_output_callback() -> OutputCallback {
    Arc::new(|server_name: &str, host: &str, line: &str| {
        let label = if server_name == "hook" {
            "hook".to_string()
        } else if host.contains(server_name) || server_name == host {
            server_name.to_string()
        } else {
            format!("{}:{}", server_name, host)
        };

        let prefix = if server_name == "hook" {
            format!("[{}]", label.dark_grey())
        } else {
            format!("[{}]", label.cyan())
        };
        println!("  {} {}", prefix, line);
    })
}

/// Default search order for a Rhai orchestration file (mirrors `nrg exec`).
const RHAI_FILES: &[&str] = &["Energize.rhai", "energize.rhai"];

/// Resolve the `.rhai` orchestration file to use for `nrg run <fn>`, if any.
///
/// Honors an explicit `--path`/`--conf` that points at a `.rhai` file; otherwise falls back
/// to a default `Energize.rhai`/`energize.rhai` in the current directory. Returns `None` when
/// no `.rhai` file applies, so `nrg run` falls through to the legacy Starlark task path.
fn find_rhai_file(args: &RunArgs) -> Option<String> {
    if let Some(p) = args.path.as_deref().or(args.conf.as_deref()) {
        if p.ends_with(".rhai") && std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
        // An explicit non-rhai file was requested — leave it to the Starlark path.
        return None;
    }
    RHAI_FILES
        .iter()
        .find(|f| std::path::Path::new(f).exists())
        .map(|s| s.to_string())
}

/// `nrg run <fn> [args...]` against a `.rhai` file: load it into the engine (same
/// `build_engine`/module-resolution path as `nrg exec`) and call the named function via Rhai
/// `call_fn`, passing the trailing CLI args as strings. Returns the process exit code.
fn run_rhai_fn(file: &str, args: &RunArgs) -> i32 {
    use crate::engine::runner::RealRunner;
    use crate::engine::state;

    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    // `nrg run <fn>` is a LIVE entry point (it calls effectful functions), so it takes the
    // advisory state lock — unless an ancestor `nrg` already holds it (re-entrancy). The lock
    // holder + guard must outlive the whole run, so they stay in this stack frame.
    let key = state::lock_key(&root);
    let reentrant = state::lock_is_reentrant(&key, std::env::var(state::LOCK_ENV).ok().as_deref());
    let mut lock_holder: Option<fd_lock::RwLock<std::fs::File>> = if reentrant {
        None
    } else {
        match state::open_lock(&root) {
            Ok(l) => Some(l),
            Err(e) => {
                ui::render_error(&format!(
                    "cannot open state lock under {}: {e}",
                    root.display()
                ));
                return 1;
            }
        }
    };
    let _guard;
    if reentrant {
        _guard = None;
    } else {
        _guard = match lock_holder.as_mut() {
            Some(l) => {
                if l.try_write().is_err() {
                    eprintln!(
                        "Waiting for the state lock (another `nrg` run is in progress under {})...",
                        root.display()
                    );
                }
                match l.write() {
                    Ok(g) => Some(g),
                    Err(e) => {
                        ui::render_error(&format!(
                            "cannot acquire state lock under {}: {e}",
                            root.display()
                        ));
                        return 1;
                    }
                }
            }
            None => None,
        };
        std::env::set_var(state::LOCK_ENV, &key);
    }

    let store = match state::StateStore::load(&root) {
        Ok(s) => s,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    let ssh = SshConfig::load_default();
    let ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store);

    match crate::engine::eval::run_fn(
        std::path::Path::new(file),
        &args.target,
        &args.fn_args,
        ctx,
    ) {
        Ok(()) => 0,
        Err(e) => {
            ui::render_error(&e);
            1
        }
    }
}

pub async fn execute(args: &RunArgs) -> i32 {
    // Rhai dispatch (design D1): if a `.rhai` orchestration file applies, `nrg run <fn> [args]`
    // loads it into the engine and calls the named function. Falls through to the legacy
    // Starlark task path when no `.rhai` file is found (Phase 6 will remove that path).
    if let Some(rhai_file) = find_rhai_file(args) {
        return run_rhai_fn(&rhai_file, args);
    }

    // Resolve and parse file
    // Parse CLI variables first — they're needed at parse time for Starlark var() calls
    let mut variables = HashMap::new();

    // Load --env file first (lowest precedence)
    if let Some(ref env_path) = args.env_file {
        match crate::parsing::env_parser::parse_env_file(std::path::Path::new(env_path)) {
            Ok(env_vars) => {
                for (k, v) in env_vars {
                    variables.insert(k, v);
                }
            }
            Err(e) => {
                ui::render_error(&format!("Failed to load env file '{}': {}", env_path, e));
                return 1;
            }
        }
    }

    // CLI --var overrides env file values
    for var in &args.vars {
        if let Some((key, value)) = var.split_once('=') {
            variables.insert(key.to_string(), value.to_string());
        }
    }

    let file_path = match parsing::resolve_file(args.path.as_deref(), args.conf.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    let config = match parsing::parse_file(&file_path, &variables) {
        Ok(c) => c,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    // Handle pretend mode
    if args.pretend {
        let task_names = match config.resolve_tasks_for_target(&args.target) {
            Some(names) => names,
            None => {
                let available = config.available_targets().join(", ");
                ui::render_error(&format!(
                    "Unknown target '{}'. Available: {}",
                    args.target, available
                ));
                return 1;
            }
        };

        let ssh_config = SshConfig::load_default();

        println!("\n{}", "Pretend mode — commands that would be executed:\n");
        for task_name in &task_names {
            if let Some(task) = config.tasks.get(task_name) {
                // Local tasks
                if task.local {
                    println!("# Task: {} (local)", task_name);
                    println!("bash -se << \\EOF-NRG\n{}\nEOF-NRG\n", task.script);
                    continue;
                }

                // Upload tasks
                if let Some(ref upload) = task.upload {
                    for server_name in &task.servers {
                        if let Some(server) = config.servers.get(server_name) {
                            for host in &server.hosts {
                                let resolved = ssh_config.resolve_host(host);
                                println!("# Upload: {} → {}:{}", upload.src, resolved, upload.dest);
                                println!("rsync -az -e ssh {} {}:{}\n", upload.src, resolved, upload.dest);
                            }
                        }
                    }
                    continue;
                }

                // Regular SSH tasks
                for server_name in &task.servers {
                    if let Some(server) = config.servers.get(server_name) {
                        for host in &server.hosts {
                            let cmd = ssh_command::build_ssh_command(
                                host,
                                &task.script,
                                &variables,
                                &ssh_config,
                            );
                            println!("# Task: {} on {}", task_name, host);
                            println!("{}\n", cmd);
                        }
                    }
                }
            }
        }
        return 0;
    }

    // Handle confirmation prompts
    if let Some(task_names) = config.resolve_tasks_for_target(&args.target) {
        for task_name in &task_names {
            if let Some(task) = config.tasks.get(task_name) {
                if let Some(confirm_msg) = &task.confirm {
                    let confirmed = dialoguer::Confirm::new()
                        .with_prompt(confirm_msg)
                        .default(false)
                        .interact()
                        .unwrap_or(false);

                    if !confirmed {
                        println!("Aborted.");
                        return 0;
                    }
                }
            }
        }
    }

    let ssh_config = SshConfig::load_default();
    let options = ExecuteOptions {
        continue_on_error: args.continue_on_error,
        pretend: false,
        variables,
    };

    // Build the output callback — None if --summary, real callback otherwise
    let callback = if args.summary {
        None
    } else {
        Some(make_output_callback())
    };

    // Execute
    let results = executor::execute(
        &args.target,
        &config,
        &options,
        &ssh_config,
        callback.as_ref(),
    )
    .await;

    match results {
        Ok(results) => {
            ui::render_result_table(&results);

            // Return non-zero if any task failed
            if results.values().all(|r| r.succeeded()) {
                0
            } else {
                1
            }
        }
        Err(e) => {
            ui::render_error(&e);
            1
        }
    }
}
