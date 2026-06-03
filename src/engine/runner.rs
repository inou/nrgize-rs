//! Command execution abstraction so builtins are testable without a real host.

use crate::ssh::config::SshConfig;
use std::process::Command;
#[cfg(test)]
use std::sync::{Arc, Mutex};

/// Raw output of a single command.
#[derive(Debug, Clone)]
pub struct RawOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}

/// Anything that can run a command locally or over SSH.
/// `Send + Sync` so it can be shared across the parallel fan-out threads.
pub trait CommandRunner: Send + Sync {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput;
    fn run_local(&self, cmd: &str) -> RawOutput;
    /// Run a remote command with `stdin` piped to it (off-argv secret delivery).
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput;
    /// Run a local command with `stdin` piped to it.
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput;
}

/// Spawn a command with all three stdio piped, write `stdin`, close it, and collect output.
/// Write-before-read is safe for the small payloads we use (passwords, env-file bodies).
fn piped(mut command: Command, stdin: &str) -> RawOutput {
    use std::io::Write;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RawOutput {
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
                exit_code: -1,
            }
        }
    };
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(stdin.as_bytes());
        // `sin` drops here, closing the pipe (EOF) so the child can finish reading.
    }
    match child.wait_with_output() {
        Ok(o) => RawOutput {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            exit_code: o.status.code().unwrap_or(-1) as i64,
        },
        Err(e) => RawOutput {
            stdout: String::new(),
            stderr: format!("wait failed: {e}"),
            exit_code: -1,
        },
    }
}

/// Production runner: spawns `ssh`/`sh` via std::process.
pub struct RealRunner {
    pub ssh: SshConfig,
}

impl RealRunner {
    fn ssh_command(&self, host: &str) -> Command {
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=10",
        ])
        .arg(self.ssh.resolve_host(host));
        c
    }
}

impl CommandRunner for RealRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        let resolved = self.ssh.resolve_host(host);
        let out = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
            ])
            .arg(&resolved)
            .arg(cmd)
            .output();
        match out {
            Ok(o) => RawOutput {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: o.status.code().unwrap_or(-1) as i64,
            },
            Err(e) => RawOutput {
                stdout: String::new(),
                stderr: format!("ssh spawn failed: {e}"),
                exit_code: -1,
            },
        }
    }

    fn run_local(&self, cmd: &str) -> RawOutput {
        let out = Command::new("sh").arg("-c").arg(cmd).output();
        match out {
            Ok(o) => RawOutput {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: o.status.code().unwrap_or(-1) as i64,
            },
            Err(e) => RawOutput {
                stdout: String::new(),
                stderr: format!("sh spawn failed: {e}"),
                exit_code: -1,
            },
        }
    }

    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        let mut c = self.ssh_command(host);
        c.arg(cmd);
        piped(c, stdin)
    }

    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        piped(c, stdin)
    }
}

/// Test runner: records every call and replays a canned output.
#[cfg(test)]
pub struct FakeRunner {
    pub calls: Mutex<Vec<String>>,
    pub default: RawOutput,
}

#[cfg(test)]
impl Default for FakeRunner {
    fn default() -> Self {
        FakeRunner {
            calls: Mutex::new(Vec::new()),
            default: RawOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            },
        }
    }
}

#[cfg(test)]
impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl CommandRunner for FakeRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("ssh {host}: {cmd}"));
        self.default.clone()
    }
    fn run_local(&self, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("local: {cmd}"));
        self.default.clone()
    }
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        self.calls
            .lock()
            .unwrap()
            .push(format!("ssh-stdin {host}: {cmd} <<< {stdin}"));
        self.default.clone()
    }
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        self.calls
            .lock()
            .unwrap()
            .push(format!("local-stdin: {cmd} <<< {stdin}"));
        self.default.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_runner_records_calls() {
        let r = FakeRunner::new();
        r.run_ssh("web1", "uptime");
        r.run_local("ls");
        assert_eq!(
            r.calls(),
            vec!["ssh web1: uptime".to_string(), "local: ls".to_string()]
        );
    }

    #[test]
    fn fake_runner_records_stdin_separately() {
        let r = FakeRunner::new();
        r.run_ssh_stdin("web1", "docker login -u u --password-stdin", "topsecret");
        assert_eq!(
            r.calls(),
            vec!["ssh-stdin web1: docker login -u u --password-stdin <<< topsecret".to_string()]
        );
    }
}
