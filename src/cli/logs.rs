//! `nrg logs <service> [--host <host>] [--follow] [--lines <n>]` — tail a service's container
//! logs across its deployed hosts, host-prefixed, fanned out over SSH.

use crate::engine::runner::host_key_checking;
use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use crate::ssh::config::SshConfig;
use clap::Args;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[derive(Args)]
pub struct LogsArgs {
    /// Service name (the `service` argument passed to `deploy()`).
    pub service: String,

    /// Restrict to one host. Defaults to every host recorded for the service.
    #[arg(long)]
    pub host: Option<String>,

    /// Stream new log lines as they arrive (like `docker logs -f`). Runs until interrupted.
    #[arg(short, long)]
    pub follow: bool,

    /// Number of trailing lines to show per host before following. 0 shows the whole log.
    #[arg(short = 'n', long, default_value_t = 100)]
    pub lines: u32,
}

pub fn execute(args: &LogsArgs) -> i32 {
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let store = match StateStore::load(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let hosts: Vec<String> = match &args.host {
        Some(h) => vec![h.clone()],
        None => store.hosts_for(&args.service),
    };
    if hosts.is_empty() {
        eprintln!(
            "Error: no hosts recorded for service '{}' (has it been deployed?); pass --host explicitly",
            args.service
        );
        return 1;
    }

    let container_cmd = store.get("nrg.runtime.cmd").unwrap_or_else(|| "docker".to_string());
    let container = format!("{}-web", args.service);
    let remote_cmd = build_remote_cmd(&container_cmd, &container, args.follow, args.lines);
    let ssh_config = SshConfig::load_default();

    let mut any_failed = false;
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for host in &hosts {
            let resolved = ssh_config.resolve_host(host);
            if resolved.starts_with('-') {
                eprintln!("Error: refusing to connect to a host that looks like an option: {resolved:?}");
                any_failed = true;
                continue;
            }
            let remote_cmd = remote_cmd.clone();
            let label = host.clone();
            handles.push(scope.spawn(move || stream_host(&label, &resolved, &remote_cmd)));
        }
        for h in handles {
            if !h.join().unwrap_or(false) {
                any_failed = true;
            }
        }
    });

    if any_failed {
        1
    } else {
        0
    }
}

/// Build the remote `docker logs` (or configured runtime) invocation. One templated string per
/// host, not per-host-specific — the same command runs against each host's own container.
fn build_remote_cmd(container_cmd: &str, container: &str, follow: bool, lines: u32) -> String {
    let tail = if lines == 0 { "all".to_string() } else { lines.to_string() };
    let mut parts = vec![container_cmd.to_string(), "logs".to_string(), "--tail".to_string(), tail];
    if follow {
        parts.push("-f".to_string());
    }
    parts.push(posix_quote(container));
    parts.join(" ")
}

/// Spawn `ssh <resolved> -- <remote_cmd>`, prefix every line with `host`, and block until the
/// child exits. Non-interactive (no `-t`): this is a passthrough log stream, not a console.
/// Returns whether it succeeded (exit 0).
fn stream_host(host: &str, resolved: &str, remote_cmd: &str) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes"])
        .arg("-o")
        .arg(format!("StrictHostKeyChecking={}", host_key_checking()))
        .args(["-o", "ConnectTimeout=10", "--"])
        .arg(resolved)
        .arg(remote_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{host} | failed to start ssh: {e}");
            return false;
        }
    };

    // Piped stdout/stderr must be drained on separate threads (relaying to nrg's own stdout/
    // stderr as lines arrive) or a chatty child can deadlock filling its pipe buffer while we're
    // still blocked on `child.wait()`.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out_host = host.to_string();
    let out_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("{out_host} | {line}");
        }
    });
    let err_host = host.to_string();
    let err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("{err_host} | {line}");
        }
    });
    let _ = out_thread.join();
    let _ = err_thread.join();

    match child.wait() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("{host} | ssh wait failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_remote_cmd_defaults_to_tail_100_no_follow() {
        assert_eq!(build_remote_cmd("docker", "app-web", false, 100), "docker logs --tail 100 'app-web'");
    }

    #[test]
    fn build_remote_cmd_follow_adds_f_flag() {
        assert_eq!(build_remote_cmd("docker", "app-web", true, 50), "docker logs --tail 50 -f 'app-web'");
    }

    #[test]
    fn build_remote_cmd_zero_lines_means_tail_all() {
        assert_eq!(build_remote_cmd("docker", "app-web", false, 0), "docker logs --tail all 'app-web'");
    }

    #[test]
    fn build_remote_cmd_quotes_container_name() {
        let cmd = build_remote_cmd("docker", "app-web; rm -rf /", false, 100);
        assert!(cmd.contains("'app-web; rm -rf /'"), "got: {cmd}");
    }
}
