use crate::execution::ssh_command;
use crate::parsing::models::*;
use crate::ssh::config::SshConfig;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Callback for output lines from task execution.
/// Args: (server_name, host, line)
pub type OutputCallback = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// Check if a line should be filtered out (trace lines, SSH warnings).
fn should_filter_line(line: &str) -> bool {
    line.starts_with("NRG_TRACE:")
        || line.contains("Warning: Permanently added")
        || line.contains("Connection to")
}

/// Run a single task across its servers.
pub async fn run_task(
    task: &TaskDefinition,
    servers: &IndexMap<String, ServerDefinition>,
    env: &HashMap<String, String>,
    variable_preamble: &str,
    ssh_config: &SshConfig,
    on_output: Option<&OutputCallback>,
) -> TaskResult {
    let start = Instant::now();

    // Handle upload tasks
    if let Some(ref upload) = task.upload {
        return run_upload_task(task, servers, upload, ssh_config, on_output).await;
    }

    // Handle local tasks (no SSH needed)
    if task.local {
        return run_local_task(task, env, variable_preamble, on_output).await;
    }

    // Build the full script with variable preamble
    let full_script = if variable_preamble.is_empty() {
        task.script.clone()
    } else {
        format!("{}\n{}", variable_preamble, task.script)
    };

    // Resolve all hosts for this task
    let mut all_hosts: Vec<(String, String)> = Vec::new();
    for server_name in &task.servers {
        if let Some(server) = servers.get(server_name) {
            for host in &server.hosts {
                all_hosts.push((server_name.clone(), host.clone()));
            }
        }
    }

    if all_hosts.is_empty() {
        return TaskResult {
            exit_code: 1,
            outputs: IndexMap::new(),
            duration: start.elapsed(),
            failed_host: None,
        };
    }

    let mut outputs = IndexMap::new();
    let mut exit_code = 0i32;
    let mut failed_host = None;

    if task.parallel {
        // Parallel execution — each host streams independently via the shared callback
        let mut handles = Vec::new();

        for (server_name, host) in &all_hosts {
            let host_for_spawn = host.clone();
            let host_for_key = host.clone();
            let server_name = server_name.clone();
            let script = full_script.clone();
            let env = env.clone();
            let ssh_config = ssh_config.clone();
            let cb = on_output.cloned();

            let handle = tokio::spawn(async move {
                execute_on_host_streaming(
                    &server_name,
                    &host_for_spawn,
                    &script,
                    &env,
                    &ssh_config,
                    cb.as_ref(),
                )
                .await
            });
            handles.push((host_for_key, handle));
        }

        for (host, handle) in handles {
            match handle.await {
                Ok((code, output)) => {
                    outputs.insert(host.clone(), output);
                    if code != 0 {
                        exit_code += code;
                        if failed_host.is_none() {
                            failed_host = Some(host);
                        }
                    }
                }
                Err(e) => {
                    outputs.insert(host.clone(), format!("Task panicked: {}", e));
                    exit_code += 1;
                    if failed_host.is_none() {
                        failed_host = Some(host);
                    }
                }
            }
        }
    } else {
        // Sequential execution — stream each host in order
        for (server_name, host) in &all_hosts {
            let (code, output) = execute_on_host_streaming(
                server_name,
                host,
                &full_script,
                env,
                ssh_config,
                on_output,
            )
            .await;

            outputs.insert(host.clone(), output);

            if code != 0 {
                exit_code = code;
                failed_host = Some(host.clone());
                break;
            }
        }
    }

    TaskResult {
        exit_code,
        outputs,
        duration: start.elapsed(),
        failed_host,
    }
}

/// Execute a script on a single host, streaming output line-by-line.
/// Returns (exit_code, accumulated_output).
async fn execute_on_host_streaming(
    server_name: &str,
    host: &str,
    script: &str,
    env: &HashMap<String, String>,
    ssh_config: &SshConfig,
    on_output: Option<&OutputCallback>,
) -> (i32, String) {
    let full_script = ssh_command::build_script(script, env, host);
    let mut cmd = ssh_command::build_process(host, script, env, ssh_config);

    match cmd.spawn() {
        Ok(mut child) => {
            // Write script to stdin
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(full_script.as_bytes()).await;
                drop(stdin);
            }

            // Set up line-by-line readers for stdout and stderr
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let mut accumulated = String::new();

            let server_name_owned = server_name.to_string();
            let host_owned = host.to_string();
            let cb = on_output.cloned();

            // Spawn a task to read stdout line-by-line
            let stdout_handle = {
                let server_name = server_name_owned.clone();
                let host = host_owned.clone();
                let cb = cb.clone();
                tokio::spawn(async move {
                    let mut lines_buf = String::new();
                    if let Some(out) = stdout {
                        let mut reader = BufReader::new(out).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            if !should_filter_line(&line) {
                                if let Some(ref cb) = cb {
                                    cb(&server_name, &host, &line);
                                }
                                if !lines_buf.is_empty() {
                                    lines_buf.push('\n');
                                }
                                lines_buf.push_str(&line);
                            }
                        }
                    }
                    lines_buf
                })
            };

            // Spawn a task to read stderr line-by-line
            let stderr_handle = {
                let server_name = server_name_owned.clone();
                let host = host_owned.clone();
                let cb = cb.clone();
                tokio::spawn(async move {
                    let mut lines_buf = String::new();
                    if let Some(err) = stderr {
                        let mut reader = BufReader::new(err).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            if !should_filter_line(&line) {
                                if let Some(ref cb) = cb {
                                    cb(&server_name, &host, &line);
                                }
                                if !lines_buf.is_empty() {
                                    lines_buf.push('\n');
                                }
                                lines_buf.push_str(&line);
                            }
                        }
                    }
                    lines_buf
                })
            };

            // Wait for both readers to finish
            let stdout_output = stdout_handle.await.unwrap_or_default();
            let stderr_output = stderr_handle.await.unwrap_or_default();

            // Combine output
            accumulated.push_str(&stdout_output);
            if !stderr_output.is_empty() {
                if !accumulated.is_empty() {
                    accumulated.push('\n');
                }
                accumulated.push_str(&stderr_output);
            }

            // Wait for the process to exit
            let status = child.wait().await;
            let exit_code = status.map(|s| s.code().unwrap_or(1)).unwrap_or(1);

            (exit_code, accumulated)
        }
        Err(e) => (1, format!("Failed to spawn process: {}", e)),
    }
}

/// Execute a task locally (no SSH).
async fn run_local_task(
    task: &TaskDefinition,
    env: &HashMap<String, String>,
    variable_preamble: &str,
    on_output: Option<&OutputCallback>,
) -> TaskResult {
    let start = Instant::now();

    let full_script = if variable_preamble.is_empty() {
        task.script.clone()
    } else {
        format!("{}\n{}", variable_preamble, task.script)
    };

    let (exit_code, output) = execute_on_host_streaming(
        "local",
        "local",
        &full_script,
        env,
        &SshConfig::empty(),
        on_output,
    )
    .await;

    TaskResult {
        exit_code,
        outputs: {
            let mut m = IndexMap::new();
            m.insert("local".to_string(), output);
            m
        },
        duration: start.elapsed(),
        failed_host: if exit_code != 0 {
            Some("local".to_string())
        } else {
            None
        },
    }
}

/// Execute an upload task using rsync (or scp fallback).
async fn run_upload_task(
    task: &TaskDefinition,
    servers: &IndexMap<String, ServerDefinition>,
    upload: &UploadSpec,
    ssh_config: &SshConfig,
    on_output: Option<&OutputCallback>,
) -> TaskResult {
    let start = Instant::now();

    // Resolve all hosts
    let mut all_hosts: Vec<(String, String)> = Vec::new();
    for server_name in &task.servers {
        if let Some(server) = servers.get(server_name) {
            for host in &server.hosts {
                all_hosts.push((server_name.clone(), host.clone()));
            }
        }
    }

    if all_hosts.is_empty() {
        return TaskResult {
            exit_code: 1,
            outputs: IndexMap::new(),
            duration: start.elapsed(),
            failed_host: None,
        };
    }

    let mut outputs = IndexMap::new();
    let mut exit_code = 0i32;
    let mut failed_host = None;

    for (server_name, host) in &all_hosts {
        let resolved = ssh_config.resolve_host(host);
        let (code, output) = execute_upload_to_host(
            server_name,
            &resolved,
            &upload.src,
            &upload.dest,
            on_output,
        )
        .await;

        outputs.insert(host.clone(), output);
        if code != 0 {
            exit_code = code;
            failed_host = Some(host.clone());
            break;
        }
    }

    TaskResult {
        exit_code,
        outputs,
        duration: start.elapsed(),
        failed_host,
    }
}

/// Upload a file/directory to a single host using rsync (fallback to scp).
async fn execute_upload_to_host(
    server_name: &str,
    host: &str,
    src: &str,
    dest: &str,
    on_output: Option<&OutputCallback>,
) -> (i32, String) {
    // Check if rsync is available
    let rsync_check = Command::new("which")
        .arg("rsync")
        .output()
        .await;

    let use_rsync = rsync_check.map(|o| o.status.success()).unwrap_or(false);

    let mut cmd = if use_rsync {
        let mut c = Command::new("rsync");
        c.args(["-az", "--progress", "-e", "ssh", src, &format!("{}:{}", host, dest)]);
        c
    } else {
        // scp fallback
        if let Some(cb) = on_output {
            cb(server_name, host, "rsync not found, falling back to scp");
        }
        let mut c = Command::new("scp");
        if std::path::Path::new(src).is_dir() {
            c.arg("-r");
        }
        c.args([src, &format!("{}:{}", host, dest)]);
        c
    };

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let mut accumulated = String::new();

            let sn = server_name.to_string();
            let h = host.to_string();
            let cb = on_output.cloned();

            let stdout_handle = {
                let sn = sn.clone();
                let h = h.clone();
                let cb = cb.clone();
                tokio::spawn(async move {
                    let mut buf = String::new();
                    if let Some(out) = stdout {
                        let mut reader = BufReader::new(out).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            if let Some(ref cb) = cb {
                                cb(&sn, &h, &line);
                            }
                            if !buf.is_empty() { buf.push('\n'); }
                            buf.push_str(&line);
                        }
                    }
                    buf
                })
            };

            let stderr_handle = {
                let sn = sn.clone();
                let h = h.clone();
                let cb = cb.clone();
                tokio::spawn(async move {
                    let mut buf = String::new();
                    if let Some(err) = stderr {
                        let mut reader = BufReader::new(err).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            if let Some(ref cb) = cb {
                                cb(&sn, &h, &line);
                            }
                            if !buf.is_empty() { buf.push('\n'); }
                            buf.push_str(&line);
                        }
                    }
                    buf
                })
            };

            let stdout_out = stdout_handle.await.unwrap_or_default();
            let stderr_out = stderr_handle.await.unwrap_or_default();

            accumulated.push_str(&stdout_out);
            if !stderr_out.is_empty() {
                if !accumulated.is_empty() { accumulated.push('\n'); }
                accumulated.push_str(&stderr_out);
            }

            let status = child.wait().await;
            let code = status.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
            (code, accumulated)
        }
        Err(e) => (1, format!("Failed to spawn upload process: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_filter_trace_lines() {
        assert!(should_filter_line("NRG_TRACE:some command"));
        assert!(!should_filter_line("normal output"));
    }

    #[test]
    fn should_filter_ssh_warnings() {
        assert!(should_filter_line(
            "Warning: Permanently added 'host' to known hosts."
        ));
        assert!(should_filter_line("Connection to host closed."));
        assert!(!should_filter_line("actual error message"));
    }

    #[test]
    fn should_not_filter_normal_output() {
        assert!(!should_filter_line("deploying to production..."));
        assert!(!should_filter_line(""));
        assert!(!should_filter_line("exit code 0"));
    }
}
