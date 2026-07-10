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

/// Build the `ssh <resolved> -- <remote_cmd>` invocation (stdio not yet wired — the caller sets
/// that up). Split out from `stream_host` so the exact args are unit-testable.
fn ssh_stream_command(resolved: &str, remote_cmd: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes"])
        .arg("-o")
        .arg(format!("StrictHostKeyChecking={}", host_key_checking()))
        .args(["-o", "ConnectTimeout=10"])
        // Robustness review R5 (same fix as RealRunner::ssh_command in src/engine/runner.rs):
        // `-f` follow mode holds this connection open indefinitely — without a keep-alive, a
        // connection that silently goes dead (network partition, dropped NAT/firewall state)
        // leaves `nrg logs -f` hanging forever showing nothing, with no way to tell a live-but-
        // quiet log stream from a dead one. These make ssh itself notice within ~60s and exit.
        .args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4", "--"])
        .arg(resolved)
        .arg(remote_cmd);
    cmd
}

/// Spawn `ssh <resolved> -- <remote_cmd>`, prefix every line with `host`, and block until the
/// child exits. Non-interactive (no `-t`): this is a passthrough log stream, not a console.
/// Returns whether it succeeded (exit 0).
fn stream_host(host: &str, resolved: &str, remote_cmd: &str) -> bool {
    let mut cmd = ssh_stream_command(resolved, remote_cmd);
    cmd.stdin(Stdio::null())
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
    // still blocked on `child.wait()`. `drain_lines` below MUST keep reading until true EOF no
    // matter what happens on our OWN output side (invalid UTF-8, a downstream reader like `head`
    // closing early, …) — the moment a thread here stops reading, the child fills its pipe and
    // blocks writing, and we're right back in the deadlock this thread exists to prevent.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out_host = host.to_string();
    let out_thread = std::thread::spawn(move || {
        use std::io::Write;
        let mut out = std::io::stdout();
        drain_lines(BufReader::new(stdout), |line| {
            let _ = writeln!(out, "{out_host} | {line}"); // ignore EPIPE; keep draining regardless
        });
    });
    let err_host = host.to_string();
    let err_thread = std::thread::spawn(move || {
        use std::io::Write;
        let mut err = std::io::stderr();
        drain_lines(BufReader::new(stderr), |line| {
            let _ = writeln!(err, "{err_host} | {line}");
        });
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

/// Read `reader` to its true EOF, calling `emit` with each line (trailing `\n`/`\r` stripped,
/// decoded lossily). Deliberately NOT `BufRead::lines()`: that iterator yields `Err` and STOPS
/// on the first invalid-UTF-8 byte — a real possibility in container log output — which would
/// silently stop draining this side of the pipe and leave the child blocked writing to it
/// forever. A write failure inside `emit` (e.g. our own stdout closed because the caller piped
/// us into `head`) must not stop the loop either, for the same reason — so `emit` swallows its
/// own errors and this function only ever stops on a genuine read EOF/error.
fn drain_lines(mut reader: impl BufRead, mut emit: impl FnMut(&str)) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,        // EOF: the child closed this stream
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                    buf.pop();
                }
                emit(&String::from_utf8_lossy(&buf));
            }
            Err(_) => break, // the pipe itself broke — nothing left to drain
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

    #[test]
    fn ssh_stream_command_sets_keepalive_options() {
        // Robustness review R5: `nrg logs -f` holds an ssh connection open indefinitely; without
        // a keep-alive, a connection that silently goes dead leaves it hanging forever with no
        // way to tell "quiet but alive" from "dead".
        let cmd = ssh_stream_command("web1", "docker logs --tail 100 'app-web'");
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        let pairs: Vec<(&str, &str)> = args
            .chunks(2)
            .filter(|c| c.len() == 2 && c[0] == "-o")
            .map(|c| (c[0], c[1]))
            .collect();
        assert!(
            pairs.iter().any(|&(_, v)| v == "ServerAliveInterval=15"),
            "missing ServerAliveInterval: {args:?}"
        );
        assert!(
            pairs.iter().any(|&(_, v)| v == "ServerAliveCountMax=4"),
            "missing ServerAliveCountMax: {args:?}"
        );
        assert!(
            pairs.iter().any(|&(_, v)| v == "ConnectTimeout=10"),
            "ConnectTimeout must still be set: {args:?}"
        );
    }

    #[test]
    fn drain_lines_splits_on_newlines_and_strips_crlf() {
        let mut got = Vec::new();
        drain_lines(std::io::Cursor::new(b"one\r\ntwo\nthree".to_vec()), |l| got.push(l.to_string()));
        assert_eq!(got, vec!["one", "two", "three"]);
    }

    #[test]
    fn drain_lines_keeps_reading_past_invalid_utf8() {
        // Regression: `BufRead::lines()` returns `Err` and STOPS on the first invalid-UTF-8
        // byte, which would leave the rest of a real container's log pipe undrained and the
        // writing child blocked forever. `drain_lines` must decode losslessly and keep going.
        let mut input = b"before\n".to_vec();
        input.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8, no trailing newline handling needed
        input.extend_from_slice(b"\nafter\n");
        let mut got = Vec::new();
        drain_lines(std::io::Cursor::new(input), |l| got.push(l.to_string()));
        assert_eq!(got.first().map(String::as_str), Some("before"));
        assert_eq!(got.last().map(String::as_str), Some("after"));
        assert_eq!(got.len(), 3, "the invalid-UTF-8 line must still be emitted (lossily), not dropped: {got:?}");
    }

    #[test]
    fn drain_lines_survives_emit_erroring_on_every_line() {
        // Regression: a downstream reader closing (e.g. `nrg logs -f | head`) makes our own
        // `println!`/`writeln!` fail. `emit` swallowing that error must not stop the read loop —
        // every line must still be drained from the child, or it blocks writing forever.
        let mut seen = 0;
        drain_lines(std::io::Cursor::new(b"a\nb\nc\n".to_vec()), |_line| {
            seen += 1;
            // simulate a write error being ignored, same as the real `let _ = writeln!(...)`
        });
        assert_eq!(seen, 3);
    }
}
