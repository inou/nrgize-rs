use crate::execution::task_runner::{self, OutputCallback};
use crate::parsing::models::*;
use crate::ssh::config::SshConfig;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::Path;

/// Options for task execution.
pub struct ExecuteOptions {
    pub continue_on_error: bool,
    pub pretend: bool,
    pub variables: HashMap<String, String>,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            continue_on_error: false,
            pretend: false,
            variables: HashMap::new(),
        }
    }
}

/// Execute a target (task or macro) with full lifecycle hooks.
pub async fn execute(
    target: &str,
    config: &ParseResult,
    options: &ExecuteOptions,
    ssh_config: &SshConfig,
    on_output: Option<&OutputCallback>,
) -> Result<IndexMap<String, TaskResult>, String> {
    // Resolve target to task list
    let task_names = config
        .resolve_tasks_for_target(target)
        .ok_or_else(|| {
            let available = config.available_targets().join(", ");
            format!(
                "Unknown target '{}'. Available targets: {}",
                target, available
            )
        })?;

    // Build environment from variable preamble + CLI vars
    let mut env = options.variables.clone();
    // CLI vars are uppercased
    let env_upper: HashMap<String, String> = env
        .drain()
        .map(|(k, v)| (k.to_uppercase(), v))
        .collect();
    let env = env_upper;

    let mut results: IndexMap<String, TaskResult> = IndexMap::new();
    let mut all_succeeded = true;

    // Execute each task in the resolved list
    for task_name in &task_names {
        let task = match config.tasks.get(task_name) {
            Some(t) => t,
            None => {
                return Err(format!(
                    "Task '{}' referenced in macro '{}' does not exist",
                    task_name, target
                ));
            }
        };

        // Run @before hooks
        for hook in config.get_hooks(HookType::Before) {
            run_hook_locally(&hook.script, on_output).await;
        }

        if options.pretend {
            // Pretend mode: just build and show the commands
            let result = TaskResult {
                exit_code: 0,
                outputs: IndexMap::new(),
                duration: std::time::Duration::from_secs(0),
                failed_host: None,
            };
            results.insert(task_name.clone(), result);
            continue;
        }

        // Merge per-task env file if specified
        let task_env = if let Some(ref env_path) = task.env_file {
            let mut merged = env.clone();
            if let Ok(env_vars) = crate::parsing::env_parser::parse_env_file(Path::new(env_path)) {
                for (k, v) in env_vars {
                    merged.entry(k.to_uppercase()).or_insert(v);
                }
            }
            merged
        } else {
            env.clone()
        };

        // Execute the task
        let result = task_runner::run_task(
            task,
            &config.servers,
            &task_env,
            &config.variable_preamble,
            ssh_config,
            on_output,
        )
        .await;

        let succeeded = result.succeeded();
        results.insert(task_name.clone(), result);

        if succeeded {
            // Run @after hooks
            for hook in config.get_hooks(HookType::After) {
                run_hook_locally(&hook.script, on_output).await;
            }
        } else {
            all_succeeded = false;
            // Run @error hooks
            for hook in config.get_hooks(HookType::Error) {
                run_hook_locally(&hook.script, on_output).await;
            }

            if !options.continue_on_error {
                break;
            }
        }
    }

    // Run @success hooks if all tasks succeeded
    if all_succeeded {
        for hook in config.get_hooks(HookType::Success) {
            run_hook_locally(&hook.script, on_output).await;
        }
    }

    // Run @finished hooks unconditionally
    for hook in config.get_hooks(HookType::Finished) {
        run_hook_locally(&hook.script, on_output).await;
    }

    Ok(results)
}

/// Execute a hook script locally, streaming output.
async fn run_hook_locally(script: &str, on_output: Option<&OutputCallback>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let child = tokio::process::Command::new("bash")
        .arg("-se")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok();

    if let Some(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes()).await;
            drop(stdin);
        }

        // Stream hook output too
        if let Some(cb) = on_output {
            let cb = cb.clone();
            if let Some(stdout) = child.stdout.take() {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    cb("hook", "local", &line);
                }
            }
        }

        let _ = child.wait().await;
    }
}
