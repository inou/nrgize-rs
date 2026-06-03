//! Command execution abstraction so builtins are testable without a real host.

use crate::ssh::config::SshConfig;
use std::process::Command;
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
}

/// Production runner: spawns `ssh`/`sh` via std::process.
pub struct RealRunner {
    pub ssh: SshConfig,
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
}

/// Test runner: records every call and replays a canned output.
pub struct FakeRunner {
    pub calls: Mutex<Vec<String>>,
    pub default: RawOutput,
}

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

impl CommandRunner for FakeRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("ssh {host}: {cmd}"));
        self.default.clone()
    }
    fn run_local(&self, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("local: {cmd}"));
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
}
