//! Test-only fakes shared by the `engine::eval` test modules.

use super::super::*;
use crate::engine::runner::RawOutput;

/// A fake `CommandRunner` for `accessory_run` tests: every `docker inspect -f
/// '{{.State.Running}}'` probe reports "not running" for the FIRST call, then "running" for
/// every call after that (simulating a container that starts successfully and stays up). All
/// other commands (the idempotent `rm -f`, `run -d`, etc.) succeed with empty output. Plain
/// `FakeRunner` can't express this — it returns the same canned answer for every call matching
/// a command, and this test genuinely needs the SAME inspect command to answer differently
/// across two calls to prove the post-start re-check doesn't just always agree with the
/// pre-start check.
pub(super) struct StartsThenStaysUpRunner {
    pub(super) inspect_calls: std::sync::atomic::AtomicUsize,
}

impl crate::engine::runner::CommandRunner for StartsThenStaysUpRunner {
    fn run_ssh(&self, _host: &str, cmd: &str) -> RawOutput {
        if cmd.contains("inspect -f '{{.State.Running}}'") {
            let n = self
                .inspect_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let running = n >= 1;
            return RawOutput {
                stdout: running.to_string(),
                stderr: String::new(),
                exit_code: 0,
            };
        }
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local(&self, _cmd: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
}

/// Spawn a real HTTP server whose Nth request (0-indexed) answers `500` if `n` is in
/// `fail_on`, else `200 OK`. Returns the assigned port and a shared counter of requests
/// served so far. Used to test `wait_healthy`'s consecutive-pass requirement (robustness
/// review R12): a caller needs the SAME endpoint to answer differently across sequential
/// requests (which a canned-response fixture can't do), AND needs to prove exactly how many
/// requests it took — a test that only checks "eventually passes" can't distinguish "required
/// N consecutive passes" from "accepted the first pass and got lucky" (both eventually pass
/// here, since every request past the failing one is a 200).
pub(super) fn spawn_flaky_http_server(
    fail_on: std::collections::HashSet<usize>,
) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let counter_thread = counter.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let n = counter_thread.fetch_add(1, Ordering::SeqCst);
            let resp = if fail_on.contains(&n) {
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            };
            let _ = stream.write_all(resp);
        }
    });
    (port, counter)
}

/// A `CommandRunner` whose `curl` call reports a NONZERO exit code but STILL prints a
/// numeric-looking status to stdout (e.g. `curl` itself ran and got a response, but the SSH
/// wrapper around it reported failure for an unrelated reason — a real, if rare, situation).
/// This is the one case that distinguishes checking `r.ok` from just trying to `parse_int` the
/// output: an implementation that dropped the `!r.ok` check but kept the parse/catch would
/// still "succeed" at parsing "200" and wrongly treat the host as healthy.
pub(super) struct FailedExitButNumericStdoutRunner;

impl crate::engine::runner::CommandRunner for FailedExitButNumericStdoutRunner {
    fn run_ssh(&self, _host: &str, cmd: &str) -> RawOutput {
        if cmd.contains("curl -s -o /dev/null") {
            return RawOutput {
                stdout: "200".to_string(),
                stderr: String::new(),
                exit_code: 1,
            };
        }
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local(&self, _cmd: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
}

/// A fake `CommandRunner` for `wait_healthy_on_host` consecutive-pass tests: the Nth `curl`
/// call (0-indexed) reports the status at `responses[n]` (or the last entry once exhausted).
/// Plain `FakeRunner` can't express "the SAME command answers differently across calls" —
/// this is the `ssh_exec`-routed sibling of `StartsThenStaysUpRunner` above.
pub(super) struct SequencedCurlRunner {
    pub(super) responses: Vec<&'static str>,
    pub(super) calls: std::sync::atomic::AtomicUsize,
    pub(super) seen: Mutex<Vec<String>>,
}

impl crate::engine::runner::CommandRunner for SequencedCurlRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.seen.lock().unwrap().push(format!("ssh {host}: {cmd}"));
        if cmd.contains("curl -s -o /dev/null") {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let status = self.responses[n.min(self.responses.len() - 1)];
            return RawOutput {
                stdout: status.to_string(),
                stderr: String::new(),
                exit_code: 0,
            };
        }
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local(&self, _cmd: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
        RawOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
}
