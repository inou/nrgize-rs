//! Starlark built-in functions for command execution (local and remote SSH).
//!
//! These functions have side effects — they actually execute commands when Starlark
//! evaluation reaches them. This is the core of the "Starlark as orchestration runtime"
//! model.

use crate::runtime::types::ExecResult;
use anyhow::anyhow;
use starlark::environment::GlobalsBuilder;
use starlark::values::list::UnpackList;
use starlark::values::Heap;
use starlark::values::Value;
use std::process::Command;
use std::thread;

/// Raw result from command execution — used internally before converting to ExecResult.
pub struct RawExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Execute a command on a remote host via the system `ssh` binary, capturing output.
pub fn do_ssh_exec(host: &str, cmd: &str) -> anyhow::Result<RawExecResult> {
    // Build SSH command with common options:
    //   -o BatchMode=yes         — never prompt for passwords (fail fast)
    //   -o StrictHostKeyChecking=accept-new — auto-accept new hosts, reject changed keys
    //   -o ConnectTimeout=10     — don't hang forever on unreachable hosts
    let output = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg(cmd)
        .output()
        .map_err(|e| anyhow!("Failed to spawn ssh process for host '{}': {}", host, e))?;

    Ok(RawExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Execute a command locally via `sh -c`, capturing output.
pub fn do_local_exec(cmd: &str) -> anyhow::Result<RawExecResult> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| anyhow!("Failed to spawn local command: {}", e))?;

    Ok(RawExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// Register execution built-in functions into the Starlark global environment.
#[starlark::starlark_module]
pub fn exec_builtins(builder: &mut GlobalsBuilder) {
    /// Execute a command on a remote host via SSH.
    ///
    /// Returns an ExecResult with stdout, stderr, exit_code, host, and ok attributes.
    ///
    /// Example:
    ///   result = ssh_exec("10.0.0.1", "docker ps")
    ///   if result.ok:
    ///       print(result.stdout)
    fn ssh_exec<'v>(
        host: &str,
        cmd: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!("[nrg] ssh_exec {} -> {}", host, cmd);
        }

        let raw = do_ssh_exec(host, cmd)?;

        if trace {
            eprintln!(
                "[nrg]   exit_code={} stdout_len={} stderr_len={}",
                raw.exit_code,
                raw.stdout.len(),
                raw.stderr.len()
            );
        }

        Ok(heap.alloc(ExecResult::remote(host, raw.stdout, raw.stderr, raw.exit_code)))
    }

    /// Execute a command locally via `sh -c`.
    ///
    /// Returns an ExecResult with stdout, stderr, exit_code, and ok attributes.
    /// The host attribute will be None.
    ///
    /// Example:
    ///   result = local_exec("docker build -t myapp:v2 .")
    ///   if not result.ok:
    ///       fail("Build failed: " + result.stderr)
    fn local_exec<'v>(
        cmd: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!("[nrg] local_exec -> {}", cmd);
        }

        let raw = do_local_exec(cmd)?;

        if trace {
            eprintln!(
                "[nrg]   exit_code={} stdout_len={} stderr_len={}",
                raw.exit_code,
                raw.stdout.len(),
                raw.stderr.len()
            );
        }

        Ok(heap.alloc(ExecResult::local(raw.stdout, raw.stderr, raw.exit_code)))
    }

    /// Execute a command on multiple remote hosts in parallel via SSH.
    ///
    /// Returns a list of ExecResult, one per host, in the same order as the input.
    /// If SSH fails for a particular host, that entry will have exit_code=-1 and
    /// stderr containing the error. The function does NOT abort on single-host failure.
    ///
    /// Example:
    ///   results = ssh_exec_all(["10.0.0.1", "10.0.0.2"], "docker pull myapp:v2")
    ///   failed = [r for r in results if not r.ok]
    ///   if failed:
    ///       fail("Failed on: " + ", ".join([r.host for r in failed]))
    fn ssh_exec_all<'v>(
        hosts: UnpackList<String>,
        cmd: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        let host_list = hosts.items;

        if trace {
            eprintln!(
                "[nrg] ssh_exec_all [{} hosts] -> {}",
                host_list.len(),
                cmd
            );
        }

        // Fan out SSH execution across threads using scoped threads.
        // Each thread gets its own copy of host + cmd.
        let cmd_owned = cmd.to_string();
        let results: Vec<ExecResult> = thread::scope(|s| {
            let handles: Vec<_> = host_list
                .iter()
                .map(|host| {
                    let host = host.clone();
                    let cmd = cmd_owned.clone();
                    s.spawn(move || -> ExecResult {
                        match do_ssh_exec(&host, &cmd) {
                            Ok(raw) => ExecResult::remote(&host, raw.stdout, raw.stderr, raw.exit_code),
                            Err(e) => ExecResult::remote(
                                &host,
                                String::new(),
                                format!("SSH connection failed: {}", e),
                                -1,
                            ),
                        }
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| h.join().expect("SSH thread panicked"))
                .collect()
        });

        if trace {
            for r in &results {
                eprintln!(
                    "[nrg]   {} -> exit_code={}",
                    r.host.as_deref().unwrap_or("?"),
                    r.exit_code
                );
            }
        }

        // Allocate each ExecResult on the Starlark heap, then create a list.
        let values: Vec<Value<'v>> = results.into_iter().map(|r| heap.alloc(r)).collect();
        Ok(heap.alloc(values))
    }
}
