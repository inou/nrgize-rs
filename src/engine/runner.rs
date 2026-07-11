//! Command execution abstraction so builtins are testable without a real host.

use std::process::Command;
#[cfg(test)]
use std::collections::HashMap;
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

/// Map a real process's `ExitStatus` to the `i64` this codebase's builtins report as
/// `exit_code`, using the POSIX/shell convention `128 + signal` for a process that was
/// terminated BY A SIGNAL (`status.code()` is `None` in that case) — rather than collapsing it
/// into the SAME `-1` sentinel used for a genuine spawn/wait failure elsewhere in this file (see
/// `rejected` and the `Err` branches below). Robustness review: "signal-killed process
/// indistinguishable from spawn failure" — without this, a script (or this engine's own probe
/// classifiers) branching on `exit_code` can't tell "the remote command was killed by SIGKILL"
/// (137) from "ssh itself never even launched" (-1); `.ok` (`exit_code == 0`) is unaffected
/// either way, since neither -1 nor any `128 + signal` value is ever 0.
#[cfg(unix)]
fn exit_code_of(status: &std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code as i64,
        // A `None` code with no signal shouldn't occur for a status `wait()` actually returns
        // (only a "stopped", non-terminal status lacks both) — fall back to the pre-existing
        // `-1` sentinel rather than fabricate a signal number (NOT `128 + 0`, which would
        // collide with a genuine, successful exit-code-0).
        None => status.signal().map_or(-1, |sig| 128 + i64::from(sig)),
    }
}
#[cfg(not(unix))]
fn exit_code_of(status: &std::process::ExitStatus) -> i64 {
    status.code().unwrap_or(-1) as i64
}

/// Spawn a command with all three stdio piped, write `stdin` concurrently with draining
/// stdout/stderr, and collect output.
///
/// Writing all of `stdin` before reading any output (the previous implementation) can deadlock
/// on a large payload: if `stdin` is bigger than the OS pipe buffer (typically 64 KB) our
/// `write_all` blocks once that buffer fills, waiting for the child to read more — but if the
/// child is itself busy writing a large amount of its OWN output before it finishes reading
/// stdin (e.g. `write_remote` of a large env-file to a remote command that echoes it back), the
/// child's stdout pipe fills too and ITS write blocks, waiting for us to read — and we never
/// will, since we're still stuck in `write_all`. Both sides wait on each other forever.
/// Robustness review: "piped() write-before-read can deadlock on large payloads".
///
/// The fix: write stdin on a dedicated thread, running concurrently with
/// `wait_with_output()`'s own internal draining of stdout/stderr (which itself already reads
/// both streams on separate threads, for the identical reason — one full pipe can't block
/// draining the other). With all three streams serviced concurrently, no side can fill a pipe
/// the other isn't already emptying.
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
    // `stdin` must outlive the spawned thread, which isn't scoped to this function — own a copy
    // rather than borrow.
    let stdin = stdin.to_string();
    let writer = child.stdin.take().map(|mut sin| {
        std::thread::spawn(move || {
            let _ = sin.write_all(stdin.as_bytes());
            // `sin` drops here, closing the pipe (EOF) so the child can finish reading.
        })
    });
    let result = match child.wait_with_output() {
        Ok(o) => RawOutput {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            exit_code: exit_code_of(&o.status),
        },
        Err(e) => RawOutput {
            stdout: String::new(),
            stderr: format!("wait failed: {e}"),
            exit_code: -1,
        },
    };
    // The child has already exited (or `wait_with_output` failed) by this point, so the writer
    // thread is either already done or about to hit a broken pipe — this join is just cleanup,
    // not something that can itself block meaningfully.
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    result
}

/// The SSH host-key checking policy, from `$NRG_SSH_HOST_KEY_CHECKING`.
///
/// Defaults to `accept-new` (TOFU: pin a host's key on first contact). Because this tool pushes
/// secrets to hosts, set `NRG_SSH_HOST_KEY_CHECKING=yes` in production and pre-populate
/// `~/.ssh/known_hosts` so first contact is verified, not trust-on-first-use. `no` disables
/// checking entirely (not recommended). An unrecognized value is rejected (falls back to the
/// default) so a typo can't silently weaken the policy to something `ssh` interprets loosely.
pub(crate) fn host_key_checking() -> String {
    match std::env::var("NRG_SSH_HOST_KEY_CHECKING") {
        Ok(v) if matches!(v.as_str(), "yes" | "accept-new" | "no" | "off" | "ask") => v,
        _ => "accept-new".to_string(),
    }
}

/// True if `host` would be parsed by `ssh` as an option (begins with `-`), which an attacker can
/// abuse: a host of `-oProxyCommand=...` runs an arbitrary command on the OPERATOR's machine
/// before any connection. We reject these and rely on a literal `--` separator (below) as a
/// second layer. A legitimate host/alias never starts with `-`.
fn looks_like_option(host: &str) -> bool {
    host.starts_with('-')
}

/// Production runner: spawns `ssh`/`sh` via std::process.
pub struct RealRunner;

impl RealRunner {
    /// Build the base `ssh` command for `host`: the connection options, then a literal `--`
    /// end-of-options separator, then the host itself, PASSED THROUGH VERBATIM (robustness review
    /// R9) — not hand-resolved against a parsed `~/.ssh/config`. This codebase used to look up
    /// `host` in its own mini config parser (`HostName`/`User` only) and hand ssh the RESOLVED
    /// `user@hostname` string instead of the alias — which defeated ssh's own config matching
    /// entirely (the argument is now a literal address, so `Host` blocks never fire again), so
    /// `Port`, `IdentityFile`, `ProxyJump`, `ProxyCommand`, `IdentitiesOnly`, `Host *` wildcards,
    /// etc. were silently dropped: an alias with `Port 2222` connected on 22 instead. Passing the
    /// alias straight through lets the REAL `ssh` binary do its own, complete, authoritative
    /// config resolution — exactly like a plain interactive `ssh <alias>` would. The `--` still
    /// guarantees a host string that begins with `-` is treated as a host, never an option
    /// (option-injection defense, issue #9). Returns an error if `host` looks like an option
    /// (belt and suspenders alongside `--`).
    fn ssh_command(&self, host: &str) -> Result<Command, String> {
        if looks_like_option(host) {
            return Err(format!(
                "refusing to ssh to a host that looks like an option: {host:?} (a host \
                 beginning with '-' can inject ssh options like -oProxyCommand=)"
            ));
        }
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("StrictHostKeyChecking={}", host_key_checking()),
            "-o",
            "ConnectTimeout=10",
            // Robustness review R5: ConnectTimeout only bounds the CONNECT phase. Once
            // connected, a network partition mid-command (or a peer that silently stops
            // responding) leaves the local `ssh` blocked in a `read()` that a dead TCP
            // connection alone never unblocks — the calling thread (and, for a live run, the
            // advisory state lock it holds for its whole lifetime) hangs forever. These make
            // ssh itself detect a dead connection: a keepalive probe every 15s, give up after 4
            // missed replies (~60s of silence) and exit non-zero instead of hanging indefinitely.
            // This does NOT cap how long a genuinely-alive, slow-but-responsive remote command
            // may run — that would need a separate wall-clock command timeout, a still-open gap.
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
            "--",
        ])
        .arg(host);
        Ok(c)
    }
}

/// Render an option-injection rejection as a failed `RawOutput` (so the non-`Result` runner
/// methods surface it as a normal command failure the script can branch on).
fn rejected(msg: String) -> RawOutput {
    RawOutput {
        stdout: String::new(),
        stderr: msg,
        exit_code: -1,
    }
}

impl CommandRunner for RealRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        let mut c = match self.ssh_command(host) {
            Ok(c) => c,
            Err(e) => return rejected(e),
        };
        let out = c.arg(cmd).output();
        match out {
            Ok(o) => RawOutput {
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                exit_code: exit_code_of(&o.status),
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
                exit_code: exit_code_of(&o.status),
            },
            Err(e) => RawOutput {
                stdout: String::new(),
                stderr: format!("sh spawn failed: {e}"),
                exit_code: -1,
            },
        }
    }

    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        let mut c = match self.ssh_command(host) {
            Ok(c) => c,
            Err(e) => return rejected(e),
        };
        c.arg(cmd);
        piped(c, stdin)
    }

    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        piped(c, stdin)
    }
}

/// Test runner: records every call and replays a canned output. Supports PER-HOST canned outputs
/// (and a per-host substring->failure rule) so tests can express partial-fleet failures — e.g.
/// "host web2's `docker run` fails" — which the single-canned-output runner couldn't (issue #27).
#[cfg(test)]
pub struct FakeRunner {
    pub calls: Mutex<Vec<String>>,
    pub default: RawOutput,
    /// host -> canned output for ALL of that host's ssh calls (overrides `default`).
    per_host: Mutex<HashMap<String, RawOutput>>,
    /// (host, cmd-substring) -> canned output for a SPECIFIC command on a host.
    per_cmd: Mutex<Vec<(String, String, RawOutput)>>,
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
            per_host: Mutex::new(HashMap::new()),
            per_cmd: Mutex::new(Vec::new()),
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
    /// Make every ssh call to `host` fail with `code`/`stderr`.
    pub fn fail_host(&self, host: &str, code: i64, stderr: &str) {
        self.per_host.lock().unwrap().insert(
            host.to_string(),
            RawOutput { stdout: String::new(), stderr: stderr.to_string(), exit_code: code },
        );
    }
    /// Make an ssh call to `host` whose command CONTAINS `needle` fail (more specific than
    /// `fail_host`; checked first).
    pub fn fail_cmd(&self, host: &str, needle: &str, code: i64, stderr: &str) {
        self.per_cmd.lock().unwrap().push((
            host.to_string(),
            needle.to_string(),
            RawOutput { stdout: String::new(), stderr: stderr.to_string(), exit_code: code },
        ));
    }
    /// Make an ssh call to `host` whose command CONTAINS `needle` succeed with canned `stdout`
    /// (the success-case sibling of `fail_cmd` — for tests that need a specific command's output,
    /// like a `docker images --format` listing, without changing every other call's response).
    pub fn respond_cmd(&self, host: &str, needle: &str, stdout: &str) {
        self.per_cmd.lock().unwrap().push((
            host.to_string(),
            needle.to_string(),
            RawOutput { stdout: stdout.to_string(), stderr: String::new(), exit_code: 0 },
        ));
    }
    /// The canned ssh output for (host, cmd): a matching per-cmd rule, else the host rule, else
    /// the default.
    fn ssh_output(&self, host: &str, cmd: &str) -> RawOutput {
        for (h, needle, out) in self.per_cmd.lock().unwrap().iter() {
            if h == host && cmd.contains(needle.as_str()) {
                return out.clone();
            }
        }
        if let Some(out) = self.per_host.lock().unwrap().get(host) {
            return out.clone();
        }
        self.default.clone()
    }
}

#[cfg(test)]
impl CommandRunner for FakeRunner {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.calls.lock().unwrap().push(format!("ssh {host}: {cmd}"));
        self.ssh_output(host, cmd)
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
        self.ssh_output(host, cmd)
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

    #[test]
    fn ssh_command_sets_keepalive_options() {
        // Robustness review R5: ConnectTimeout alone doesn't detect a connection that goes dead
        // AFTER connecting (network partition mid-command) — ssh must be told to actively probe
        // and give up, or a hung remote command blocks the calling thread (and the held state
        // lock) forever.
        let r = RealRunner;
        let cmd = r.ssh_command("web1").unwrap();
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
        // The existing ConnectTimeout must still be present too (not replaced).
        assert!(
            pairs.iter().any(|&(_, v)| v == "ConnectTimeout=10"),
            "ConnectTimeout must still be set: {args:?}"
        );
        // `--` must immediately precede the host (option-injection defense, issue #9) — pin this
        // explicitly so a future edit can't slot the new options in AFTER it by mistake.
        assert_eq!(
            &args[args.len() - 2..],
            ["--", "web1"],
            "-- must immediately precede the host: {args:?}"
        );
    }

    #[test]
    fn option_like_hosts_are_detected() {
        assert!(looks_like_option("-oProxyCommand=touch /tmp/pwned"));
        assert!(looks_like_option("--"));
        assert!(!looks_like_option("web1"));
        assert!(!looks_like_option("deploy@10.0.0.1"));
    }

    #[test]
    fn real_runner_rejects_option_like_host() {
        let r = RealRunner;
        // A host starting with '-' must be rejected BEFORE spawning ssh (it would otherwise be
        // parsed as an option = local RCE).
        let out = r.run_ssh("-oProxyCommand=touch /tmp/pwned", "echo hi");
        assert_eq!(out.exit_code, -1);
        assert!(out.stderr.contains("looks like an option"), "got: {}", out.stderr);
        let out2 = r.run_ssh_stdin("-oProxyCommand=x", "cat", "data");
        assert_eq!(out2.exit_code, -1);
    }

    #[test]
    fn exit_code_of_maps_a_signal_kill_to_128_plus_signal_not_the_spawn_failure_sentinel() {
        // Robustness review: "signal-killed process indistinguishable from spawn failure" — a
        // process that actually ran and was killed by SIGKILL (9) must report 128+9=137, the
        // POSIX/shell convention, rather than collapsing into the SAME -1 sentinel this file
        // uses elsewhere for a genuine local spawn/wait failure (`rejected`, the `Err` arms).
        let mut child = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .spawn()
            .unwrap();
        let status = child.wait().unwrap();
        assert_eq!(exit_code_of(&status), 137, "SIGKILL must map to 128 + 9 = 137");
        assert_ne!(
            exit_code_of(&status), -1,
            "a real, signal-killed process must not report the same code as a spawn failure"
        );
    }

    #[test]
    fn real_runner_run_local_reports_128_plus_signal_for_a_killed_process() {
        // Same property as above, but through the full `RealRunner::run_local` pipeline (not
        // just the helper function in isolation), so the wiring is covered end to end.
        let r = RealRunner;
        let out = r.run_local("kill -9 $$");
        assert_eq!(out.exit_code, 137, "got: {out:?}");
    }

    #[test]
    fn piped_does_not_deadlock_on_a_large_stdin_payload_paired_with_large_output() {
        // Robustness review: "piped() write-before-read can deadlock on large payloads". `cat`
        // simultaneously reads stdin and echoes it straight back to stdout — a scenario that,
        // under the old write-everything-then-read implementation, deadlocks once the payload
        // exceeds the OS pipe buffer (typically 64 KB): our write blocks waiting for `cat` to
        // read more of stdin, while `cat`'s own write (of the bytes it already read) blocks
        // waiting for us to read stdout — and we never do, since we're still stuck writing.
        //
        // Run on a background thread with a bounded `recv_timeout` rather than calling directly:
        // if this ever regresses to the deadlocking implementation, the call itself would hang
        // forever, which would hang the whole test suite rather than fail this one test.
        let payload = "x".repeat(4 * 1024 * 1024); // 4 MiB — far past any pipe buffer
        let payload_for_thread = payload.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(RealRunner.run_local_stdin("cat", &payload_for_thread));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect(
                "run_local_stdin deadlocked on a large stdin/stdout payload — piped() must \
                 write stdin concurrently with draining stdout/stderr, not before",
            );
        assert_eq!(out.exit_code, 0, "got: stderr={:?}", out.stderr);
        assert_eq!(out.stdout, payload, "cat must echo the exact payload back");
    }

    #[test]
    fn host_key_checking_defaults_and_validates() {
        // Default (env unset) is accept-new.
        std::env::remove_var("NRG_SSH_HOST_KEY_CHECKING");
        assert_eq!(host_key_checking(), "accept-new");
        std::env::set_var("NRG_SSH_HOST_KEY_CHECKING", "yes");
        assert_eq!(host_key_checking(), "yes");
        // A bogus value falls back to the safe default rather than being passed through.
        std::env::set_var("NRG_SSH_HOST_KEY_CHECKING", "bogus");
        assert_eq!(host_key_checking(), "accept-new");
        std::env::remove_var("NRG_SSH_HOST_KEY_CHECKING");
    }
}
