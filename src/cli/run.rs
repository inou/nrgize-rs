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
    /// Task or macro name to execute
    pub target: String,

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

pub async fn execute(args: &RunArgs) -> i32 {
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
