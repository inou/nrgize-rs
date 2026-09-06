//! Command execution abstraction so builtins are testable without a real host.

#[cfg(test)]
use std::collections::HashMap;
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
///
/// Uses `thread::scope` rather than a `'static` `thread::spawn` so the writer thread can borrow
/// `stdin: &str` directly instead of needing an owned copy: `stdin` here is often secret
/// material (a password, an env-file body via `write_remote`), so avoiding a second, un-freed-
/// until-drop heap copy of it is worth the (tiny) extra syntactic ceremony.
fn piped(mut command: Command, stdin: &str) -> RawOutput {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let timeout = std::env::var("NRG_COMMAND_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(600);
    let limit = std::env::var("NRG_MAX_OUTPUT_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(16 * 1024 * 1024);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return rejected(format!("spawn failed: {e}")),
    };
    let mut sin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    fn drain(mut pipe: impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        let mut truncated = false;
        let mut buf = [0; 8192];
        loop {
            let n = pipe.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let keep = n.min(limit.saturating_sub(out.len()));
            out.extend_from_slice(&buf[..keep]);
            truncated |= keep != n;
        }
        Ok((out, truncated))
    }
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || sin.write_all(stdin.as_bytes()));
        let out = scope.spawn(move || drain(stdout, limit));
        let err = scope.spawn(move || drain(stderr, limit));
        let start = Instant::now();
        let mut status = None;
        let mut timed_out = false;
        loop {
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
            if status.is_some() && out.is_finished() && err.is_finished() && writer.is_finished() {
                break;
            }
            if start.elapsed() >= Duration::from_secs(timeout) {
                timed_out = true;
                #[cfg(unix)]
                {
                    unsafe extern "C" {
                        fn kill(pid: i32, sig: i32) -> i32;
                    }
                    unsafe {
                        kill(-(child.id() as i32), 9);
                    }
                }
                let _ = child.kill();
                status = child.wait().ok();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let written = writer.join().expect("stdin thread panicked");
        let (stdout, out_truncated) = match out.join().expect("stdout thread panicked") {
            Ok(v) => v,
            Err(e) => return rejected(format!("stdout read failed: {e}")),
        };
        let (stderr, err_truncated) = match err.join().expect("stderr thread panicked") {
            Ok(v) => v,
            Err(e) => return rejected(format!("stderr read failed: {e}")),
        };
        let mut result = RawOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: status.as_ref().map(exit_code_of).unwrap_or(-1),
        };
        if timed_out {
            result.exit_code = -1;
            result
                .stderr
                .push_str("\ncommand deadline exceeded; remote outcome is unknown");
        }
        if out_truncated || err_truncated {
            result.exit_code = -1;
            result.stderr.push_str("\ncommand output exceeded limit");
        }
        if let Err(e) = written {
            if result.exit_code == 0 {
                result.exit_code = -1;
                result
                    .stderr
                    .push_str(&format!("\nstdin write failed: {e}"));
            }
        }
        result
    })
}

/// The SSH host-key checking policy, from `$NRG_SSH_HOST_KEY_CHECKING`.
///
/// Defaults to `yes` (fail closed: a host whose key isn't already in `~/.ssh/known_hosts` is
/// refused). Because this tool pushes secrets to hosts — registry passwords into
/// `docker login --password-stdin`, plaintext env-files through `write_remote` — the first
/// connection must be verified, not trusted on sight: pre-populate `~/.ssh/known_hosts` (see
/// `docs/safety.md`). `NRG_SSH_HOST_KEY_CHECKING=accept-new` opts back in to trust-on-first-use
/// (pin a host's key on first contact); `no` disables checking entirely (not recommended). An
/// unrecognized value is rejected (falls back to the default) so a typo can't silently weaken
/// the policy to something `ssh` interprets loosely.
pub(crate) fn host_key_checking() -> String {
    match std::env::var("NRG_SSH_HOST_KEY_CHECKING") {
        Ok(v) if matches!(v.as_str(), "yes" | "accept-new" | "no" | "off" | "ask") => v,
        _ => "yes".to_string(),
    }
}

/// The `ControlPersist` duration for SSH connection multiplexing, from
/// `$NRG_SSH_CONTROL_PERSIST` — or `None` if multiplexing is disabled.
///
/// Robustness review: "No connection reuse" — every builtin call previously paid a full fresh
/// TCP+SSH handshake: `wait_healthy_on_host`/`wait_healthy_all` (the per-host, SSH-based health
/// gate deploy() uses — NOT the control-machine-HTTP `wait_healthy`, which never SSHes at all)
/// reconnecting every 2s for up to 30 tries, or a fleet command reconnecting per host per call.
/// `ControlMaster`/`ControlPersist` let `ssh` share one already-authenticated connection across
/// calls to the same host, cutting that latency substantially. Defaults to a 60s persist — long
/// enough to cover a retry loop's burst of
/// reconnects to the same host, short enough that an idle control socket doesn't linger
/// indefinitely between unrelated invocations. `no`/`0`/`off` disables multiplexing entirely
/// (reverting to a fresh connection per call, the pre-existing behavior) for anyone who hits a
/// multiplexing-specific quirk (e.g. a jump host or bastion that mishandles shared control
/// sockets). An unrecognized value falls back to the default rather than being passed through
/// verbatim (Opus review, round 4) — `host_key_checking`'s sibling policy takes the same
/// safe-fallback approach: a typo here shouldn't turn into `ssh`'s own confusing rejection of a
/// nonsense `ControlPersist=<garbage>` value on every single call.
pub(crate) fn control_persist() -> Option<String> {
    match std::env::var("NRG_SSH_CONTROL_PERSIST") {
        Ok(v) if matches!(v.as_str(), "no" | "0" | "off") => None,
        Ok(v) if is_valid_control_persist(&v) => Some(v),
        _ => Some("60s".to_string()),
    }
}

/// Whether `v` looks like a value `ssh`'s own `ControlPersist` option would accept: `yes` (persist
/// forever), or a run of digits optionally followed by one time-unit suffix (`s`/`m`/`h`/`d`/`w`,
/// matching `ssh_config(5)`'s own time-value grammar) — not a full re-implementation of that
/// grammar, just enough to catch an obvious typo before it reaches every single `ssh` invocation.
fn is_valid_control_persist(v: &str) -> bool {
    if v == "yes" {
        return true;
    }
    let digits = v.trim_end_matches(['s', 'm', 'h', 'd', 'w']);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// The `ControlPath` template for SSH connection multiplexing. Must contain one of ssh's own
/// per-connection tokens (`%C`, a hash of user/host/port, or `%h`) so DIFFERENT hosts resolve to
/// DIFFERENT sockets — this codebase never sees the resolved path itself. `$NRG_SSH_CONTROL_PATH`
/// overrides it, but ONLY if it contains one of those tokens (Fable review, round 4): a static
/// override with no host-distinguishing token would let ssh silently multiplex EVERY host onto
/// the SAME control socket — the second host's client would attach to the first host's already-
/// authenticated master and every command would run on the wrong machine, with no error at all.
/// An override failing that check is ignored (falls back to the safe per-host default below)
/// rather than trusted verbatim.
///
/// Absent a valid override, defaults to `$HOME/.ssh/nrg-cm/%C`, creating `$HOME/.ssh/nrg-cm`
/// first. Unlike the client-attach side of `ControlMaster=auto` (which DOES degrade gracefully to
/// a plain connection if an existing socket is stale/unreachable), the MASTER side does not
/// degrade if it can't bind its listening socket at all — a missing `ControlPath` directory makes
/// `ssh` itself hard-fail the whole connection (`cleanup_exit`), not silently skip multiplexing
/// (Fable review, round 4: an earlier version of this comment claimed the opposite). So a failed
/// `create_dir_all` here must skip multiplexing entirely, not proceed and let every ssh call start
/// failing. Also returns `None` if neither a valid override nor `$HOME` is available.
fn control_path_template() -> Option<String> {
    if let Ok(path) = std::env::var("NRG_SSH_CONTROL_PATH") {
        if path.contains("%C") || path.contains("%h") {
            return Some(path);
        }
        // Falls through to the default below rather than trusting an override that can't tell
        // hosts apart (see the doc comment above) — deliberately not returning None/erroring
        // here, so a misconfigured override degrades to "multiplexing still works, just via the
        // normal per-host path" instead of "multiplexing silently vanishes everywhere."
    }
    let home = std::env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join(".ssh").join("nrg-cm");
    create_dir_all_0700(&dir).ok()?;
    Some(format!("{}/%C", dir.display()))
}

/// `create_dir_all`, but with the containing directories created `0700` (owner-only) rather than
/// umask-default `0755` — matching the permissions `ssh`/`ssh-keygen` themselves use for `~/.ssh`
/// and its contents, so a from-scratch `~/.ssh/nrg-cm` isn't accidentally world-readable (Opus +
/// Fable review, round 4; the control sockets `ssh` creates inside it are separately
/// owner-restricted by `ssh` itself regardless, so this is defense in depth, not the only guard).
#[cfg(unix)]
fn create_dir_all_0700(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(dir)
}
#[cfg(not(unix))]
fn create_dir_all_0700(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
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
        ]);
        // Robustness review: "No connection reuse" — share one authenticated connection per host
        // across calls instead of paying a fresh handshake every time. Skipped entirely (falling
        // back to the pre-existing per-call-fresh-connection behavior) if multiplexing is
        // disabled or no control-path template is available.
        if let Some(persist) = control_persist() {
            if let Some(path) = control_path_template() {
                c.args([
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    &format!("ControlPersist={persist}"),
                    "-o",
                    &format!("ControlPath={path}"),
                ]);
            }
        }
        c.args(["--"]).arg(host);
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
        c.arg(cmd);
        piped(c, "")
    }

    fn run_local(&self, cmd: &str) -> RawOutput {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        piped(c, "")
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
            RawOutput {
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code: code,
            },
        );
    }
    /// Make an ssh call to `host` whose command CONTAINS `needle` fail (more specific than
    /// `fail_host`; checked first).
    pub fn fail_cmd(&self, host: &str, needle: &str, code: i64, stderr: &str) {
        self.per_cmd.lock().unwrap().push((
            host.to_string(),
            needle.to_string(),
            RawOutput {
                stdout: String::new(),
                stderr: stderr.to_string(),
                exit_code: code,
            },
        ));
    }
    /// Make an ssh call to `host` whose command CONTAINS `needle` succeed with canned `stdout`
    /// (the success-case sibling of `fail_cmd` — for tests that need a specific command's output,
    /// like a `docker images --format` listing, without changing every other call's response).
    pub fn respond_cmd(&self, host: &str, needle: &str, stdout: &str) {
        self.per_cmd.lock().unwrap().push((
            host.to_string(),
            needle.to_string(),
            RawOutput {
                stdout: stdout.to_string(),
                stderr: String::new(),
                exit_code: 0,
            },
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
        self.calls
            .lock()
            .unwrap()
            .push(format!("ssh {host}: {cmd}"));
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
        //
        // Robustness review: "No connection reuse" (Fable review, round 4) — ssh_command now also
        // reads NRG_SSH_CONTROL_PERSIST/NRG_SSH_CONTROL_PATH, so this must serialize against the
        // other tests that mutate those same process-global env vars, same as every other
        // env-reading test in this file.
        let _env_guard = crate::test_support::lock_env();
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
    fn ssh_command_enables_connection_multiplexing_by_default() {
        // Robustness review: "No connection reuse" — by default (both env vars unset), ssh_command
        // must request ControlMaster/ControlPersist/ControlPath so repeated calls to the same host
        // share one authenticated connection instead of a fresh handshake every time.
        let _env_guard = crate::test_support::lock_env();
        std::env::remove_var("NRG_SSH_CONTROL_PERSIST");
        std::env::remove_var("NRG_SSH_CONTROL_PATH");
        let cmd = RealRunner.ssh_command("web1").unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        let pairs: Vec<(&str, &str)> = args
            .chunks(2)
            .filter(|c| c.len() == 2 && c[0] == "-o")
            .map(|c| (c[0], c[1]))
            .collect();
        assert!(
            pairs.iter().any(|&(_, v)| v == "ControlMaster=auto"),
            "missing ControlMaster=auto: {args:?}"
        );
        assert!(
            pairs.iter().any(|&(_, v)| v == "ControlPersist=60s"),
            "missing default ControlPersist=60s: {args:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|&(_, v)| v.starts_with("ControlPath=") && v.ends_with("/%C")),
            "ControlPath must end in ssh's %C token: {args:?}"
        );
        // `--` must still immediately precede the host even with the new options inserted before it.
        assert_eq!(&args[args.len() - 2..], ["--", "web1"], "got: {args:?}");
        // Clean up the directory `control_path_template` creates as a side effect of this test
        // (Opus review, round 4) — best-effort, and only removable at all if left empty (which it
        // is: nothing in this test actually connects, so no real control socket ever appears in it).
        if let Some(home) = std::env::var_os("HOME") {
            let _ = std::fs::remove_dir(std::path::PathBuf::from(home).join(".ssh").join("nrg-cm"));
        }
    }

    #[test]
    fn ssh_command_falls_back_to_default_on_an_unrecognized_persist_value() {
        // Robustness review: "No connection reuse" (Opus review, round 4) — control_persist's
        // sibling host_key_checking() falls back to a safe default on a garbage value rather than
        // passing it through verbatim; this must do the same, or a typo turns into `ssh` rejecting
        // EVERY call with a confusing `ControlPersist=<garbage>` error instead of just being ignored.
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SSH_CONTROL_PERSIST", "sixty");
        let cmd = RealRunner.ssh_command("web1").unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(
            args.contains(&"ControlPersist=60s"),
            "an unrecognized value must fall back to the default, not pass through: {args:?}"
        );
        std::env::remove_var("NRG_SSH_CONTROL_PERSIST");
    }

    #[test]
    fn ssh_command_disables_multiplexing_via_env() {
        let _env_guard = crate::test_support::lock_env();
        for off in ["no", "0", "off"] {
            std::env::set_var("NRG_SSH_CONTROL_PERSIST", off);
            let cmd = RealRunner.ssh_command("web1").unwrap();
            let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert!(
                !args.iter().any(|a| a.starts_with("ControlMaster")
                    || a.starts_with("ControlPersist")
                    || a.starts_with("ControlPath")),
                "{off:?} must disable multiplexing entirely: {args:?}"
            );
        }
        std::env::remove_var("NRG_SSH_CONTROL_PERSIST");
    }

    #[test]
    fn ssh_command_respects_custom_persist_and_path_overrides() {
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SSH_CONTROL_PERSIST", "10m");
        std::env::set_var("NRG_SSH_CONTROL_PATH", "/tmp/nrg-test-cm/%C");
        let cmd = RealRunner.ssh_command("web1").unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        let pairs: Vec<(&str, &str)> = args
            .chunks(2)
            .filter(|c| c.len() == 2 && c[0] == "-o")
            .map(|c| (c[0], c[1]))
            .collect();
        assert!(
            pairs.iter().any(|&(_, v)| v == "ControlMaster=auto"),
            "overriding persist/path must not drop ControlMaster=auto: {args:?}"
        );
        assert!(
            pairs.iter().any(|&(_, v)| v == "ControlPersist=10m"),
            "got: {args:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|&(_, v)| v == "ControlPath=/tmp/nrg-test-cm/%C"),
            "got: {args:?}"
        );
        std::env::remove_var("NRG_SSH_CONTROL_PERSIST");
        std::env::remove_var("NRG_SSH_CONTROL_PATH");
    }

    #[test]
    fn ssh_command_rejects_a_control_path_override_with_no_host_distinguishing_token() {
        // Robustness review: "No connection reuse" (Fable review, round 4) — the single most
        // important property here: a static ControlPath with no %C/%h would make ssh silently
        // multiplex EVERY host onto the SAME control socket, so a command meant for host B could
        // actually run on host A (whichever host's client happened to become the master first).
        // An override failing this check must be IGNORED (falling back to the safe per-host
        // default), not trusted verbatim.
        let _env_guard = crate::test_support::lock_env();
        std::env::set_var("NRG_SSH_CONTROL_PATH", "/tmp/nrg-test-cm-no-token.sock");
        let cmd = RealRunner.ssh_command("web1").unwrap();
        std::env::remove_var("NRG_SSH_CONTROL_PATH");
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(
            !args.contains(&"ControlPath=/tmp/nrg-test-cm-no-token.sock"),
            "a token-less override must never be used verbatim: {args:?}"
        );
        // Multiplexing itself isn't necessarily disabled — it falls back to the safe per-host
        // default path instead, so ControlPath should still be present, just not the bad value.
        assert!(
            args.iter()
                .any(|a| a.starts_with("ControlPath=") && a.ends_with("/%C")),
            "must fall back to the %C-suffixed default, not silently drop multiplexing: {args:?}"
        );
        if let Some(home) = std::env::var_os("HOME") {
            let _ = std::fs::remove_dir(std::path::PathBuf::from(home).join(".ssh").join("nrg-cm"));
        }
    }

    #[test]
    fn ssh_command_skips_multiplexing_entirely_if_the_control_dir_cannot_be_created() {
        // Robustness review: "No connection reuse" (Fable review, round 4) — this is the exact
        // scenario the earlier (wrong) doc comment claimed ssh handles gracefully: it does NOT.
        // ssh's control-MASTER side hard-fails the whole connection if it can't bind its listening
        // socket, so a `create_dir_all` failure here must skip multiplexing entirely rather than
        // emit a ControlPath ssh can never use. Point $HOME at a path that can't be mkdir'd into
        // (a regular FILE, not a directory) to force that failure deterministically.
        let _env_guard = crate::test_support::lock_env();
        std::env::remove_var("NRG_SSH_CONTROL_PATH");
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &blocker);
        let cmd = RealRunner.ssh_command("web1").unwrap();
        std::env::remove_var("HOME");
        if let Some(home) = old_home {
            std::env::set_var("HOME", home);
        }
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(
            !args.iter().any(|a| a.starts_with("ControlMaster")
                || a.starts_with("ControlPersist")
                || a.starts_with("ControlPath")),
            "an uncreatable control dir must skip multiplexing, not emit a ControlPath ssh can \
             never bind to: {args:?}"
        );
    }

    #[test]
    fn ssh_command_skips_multiplexing_silently_when_home_is_unavailable() {
        // Robustness review: "No connection reuse" (Opus review, round 4) — control_path_template
        // returns None when neither $NRG_SSH_CONTROL_PATH nor $HOME is set, and the whole
        // ControlMaster/ControlPersist/ControlPath trio must then be omitted rather than emitting a
        // half-formed option set. This is the one previously-untested branch of that function.
        let _env_guard = crate::test_support::lock_env();
        std::env::remove_var("NRG_SSH_CONTROL_PATH");
        let old_home = std::env::var_os("HOME");
        std::env::remove_var("HOME");
        let cmd = RealRunner.ssh_command("web1").unwrap();
        std::env::remove_var("HOME");
        if let Some(home) = old_home {
            std::env::set_var("HOME", home);
        }
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(
            !args.iter().any(|a| a.starts_with("ControlMaster")
                || a.starts_with("ControlPersist")
                || a.starts_with("ControlPath")),
            "no $HOME and no override must skip multiplexing entirely, not emit a partial option \
             set: {args:?}"
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
        assert!(
            out.stderr.contains("looks like an option"),
            "got: {}",
            out.stderr
        );
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
        assert_eq!(
            exit_code_of(&status),
            137,
            "SIGKILL must map to 128 + 9 = 137"
        );
        assert_ne!(
            exit_code_of(&status),
            -1,
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
        // A `Disconnected` error (channel closed because the spawned thread panicked before
        // sending) is reported as a distinct, immediate failure below rather than being lumped
        // into the "deadlocked" message the plain 10s `Timeout` case gets — `recv_timeout`
        // returns `Disconnected` promptly on a sender panic, it does not wait out the timeout.
        let out = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(out) => out,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "run_local_stdin deadlocked on a large stdin/stdout payload — piped() must \
                 write stdin concurrently with draining stdout/stderr, not before"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the spawned thread running run_local_stdin panicked before it could send a result")
            }
        };
        assert_eq!(out.exit_code, 0, "got: stderr={:?}", out.stderr);
        assert_eq!(out.stdout, payload, "cat must echo the exact payload back");
    }

    #[test]
    fn host_key_checking_defaults_and_validates() {
        // Robustness review: "Flaky patterns" — serialize against every other env-mutating
        // test in this binary (cargo runs test threads in parallel by default, and
        // set_var/getenv racing across threads is UB-adjacent on glibc).
        let _env_guard = crate::test_support::lock_env();
        // Default (env unset) is `yes`: fail closed on an unknown host key. This tool streams
        // registry passwords and env-file plaintext over these connections, so trust-on-first-use
        // must be an explicit choice, never what you get by forgetting to set anything.
        std::env::remove_var("NRG_SSH_HOST_KEY_CHECKING");
        assert_eq!(host_key_checking(), "yes");
        // TOFU is still available, but only as an opt-in.
        std::env::set_var("NRG_SSH_HOST_KEY_CHECKING", "accept-new");
        assert_eq!(host_key_checking(), "accept-new");
        std::env::set_var("NRG_SSH_HOST_KEY_CHECKING", "yes");
        assert_eq!(host_key_checking(), "yes");
        // A bogus value falls back to the safe default rather than being passed through.
        std::env::set_var("NRG_SSH_HOST_KEY_CHECKING", "bogus");
        assert_eq!(host_key_checking(), "yes");
        std::env::remove_var("NRG_SSH_HOST_KEY_CHECKING");
    }

    // Robustness review: "Real ssh/docker never exercised" — every RealRunner test above drives
    // ordinary host processes (sh, kill, cat). None of them prove `run_local`/`run_local_stdin`
    // actually work against a real `docker` invocation: a genuinely separate, namespaced process
    // whose own exit code and stdout must survive being wrapped in `sh -c "docker run ..."` and
    // then re-mapped by `exit_code_of`. `sshd` isn't installable in this sandbox (no fixture
    // available), but `docker` is real here, so this covers the `docker` half of the finding
    // directly. The `cat` test above already reaches `exit_code_of`'s `Some(code)` branch, but
    // only ever with `code == 0`; the exit-code test below is the first where that branch's
    // return value is load-bearing (a real non-zero code), so a mutation collapsing it to a
    // constant 0 is caught here rather than surviving because 0 == 0 either way.

    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("info")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn cc_available() -> bool {
        std::process::Command::new("cc")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build a `FROM scratch` image (no registry pull — this compiles its own static entrypoint
    /// with `cc`) tagged `tag`. The entrypoint copies stdin to stdout, then exits with its first
    /// argv (or 0 if none) — enough to probe argv passthrough, stdin piping, and exit-code mapping
    /// all through one tiny binary. Returns the failing command's captured stderr on failure.
    fn build_echo_exit_image(dir: &std::path::Path, tag: &str) -> Result<(), String> {
        let src = dir.join("entry.c");
        std::fs::write(
            &src,
            r#"#include <stdio.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    int c;
    while ((c = getchar()) != EOF) putchar(c);
    return argc > 1 ? atoi(argv[1]) : 0;
}
"#,
        )
        .unwrap();
        let bin = dir.join("entry");
        let cc_out = std::process::Command::new("cc")
            .args(["-static", "-O2", "-o"])
            .arg(&bin)
            .arg(&src)
            .output()
            .map_err(|e| format!("failed to spawn cc: {e}"))?;
        if !cc_out.status.success() {
            return Err(format!(
                "cc failed: {}",
                String::from_utf8_lossy(&cc_out.stderr)
            ));
        }
        std::fs::write(
            dir.join("Dockerfile"),
            "FROM scratch\nCOPY entry /entry\nENTRYPOINT [\"/entry\"]\n",
        )
        .unwrap();
        let build_out = std::process::Command::new("docker")
            .args(["build", "-t", tag, "."])
            .current_dir(dir)
            .output()
            .map_err(|e| format!("failed to spawn docker build: {e}"))?;
        if !build_out.status.success() {
            return Err(format!(
                "docker build failed: {}",
                String::from_utf8_lossy(&build_out.stderr)
            ));
        }
        Ok(())
    }

    /// Skip quietly outside CI (a contributor's machine may just not have `docker`/`cc`), but
    /// panic in CI: this pair of tests is this file's only real-container coverage of
    /// `RealRunner`, and every skip condition here — docker/cc missing, `cc -static` failing,
    /// `docker build` failing — must be as loud as the dedicated `docker_and_cc_must_be_available_
    /// in_ci` canary below, or a regression in any ONE of them (not just plain absence) could still
    /// silently drop this coverage on an all-green CI build.
    fn skip_or_fail_loudly_in_ci(reason: &str) {
        if std::env::var("CI").is_ok() {
            panic!("{reason} — in CI this must be a hard failure, not a silent skip");
        }
        eprintln!("skipping: {reason}");
    }

    #[test]
    fn real_docker_container_exit_code_and_argv_survive_run_local() {
        if !docker_available() || !cc_available() {
            skip_or_fail_loudly_in_ci("docker daemon or cc not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tag = format!("nrgize-runner-test-exit:{}", std::process::id());
        // This covers `cc`/`docker build` failing outright — if `-static` links but the result
        // can't actually exec under `FROM scratch` (an incomplete static libc on some platform),
        // `docker run` below fails to exec, which still fails the test loudly rather than
        // skipping — just for a different, less obvious reason. Not a concern on the pinned
        // `ubuntu-latest` CI image this is written for.
        if let Err(e) = build_echo_exit_image(dir.path(), &tag) {
            skip_or_fail_loudly_in_ci(&format!("failed to build the local test image: {e}"));
            return;
        }
        // The exit code is passed as a CMD arg (real argv construction through the shell command
        // string, then into the container's own argv), not hardcoded — so this also proves
        // `docker run`'s arguments actually reach the container's entrypoint. `< /dev/null` is
        // belt-and-suspenders (Command::output() already gives the child a closed/null stdin, and
        // `docker run` without `-i` doesn't attach container stdin either) — kept explicit so a
        // reader doesn't have to know either of those facts to see this test isn't accidentally
        // depending on inherited stdin.
        let out = RealRunner.run_local(&format!("docker run --rm {tag} 42 < /dev/null"));
        let _ = std::process::Command::new("docker")
            .args(["rmi", "-f", &tag])
            .status();
        assert_eq!(
            out.exit_code, 42,
            "a real container's own exit code must survive run_local's sh -c wrapping and \
             exit_code_of's mapping: got {out:?}"
        );
    }

    #[test]
    fn real_docker_container_stdin_pipes_through_run_local_stdin() {
        if !docker_available() || !cc_available() {
            skip_or_fail_loudly_in_ci("docker daemon or cc not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tag = format!("nrgize-runner-test-stdin:{}", std::process::id());
        if let Err(e) = build_echo_exit_image(dir.path(), &tag) {
            skip_or_fail_loudly_in_ci(&format!("failed to build the local test image: {e}"));
            return;
        }
        let out = RealRunner.run_local_stdin(
            &format!("docker run --rm -i {tag}"),
            "hello-through-a-real-container",
        );
        let _ = std::process::Command::new("docker")
            .args(["rmi", "-f", &tag])
            .status();
        assert_eq!(out.exit_code, 0, "got: {out:?}");
        assert_eq!(
            out.stdout, "hello-through-a-real-container",
            "stdin piped into `docker run -i` must reach the container and its stdout must \
             come back out through run_local_stdin unchanged"
        );
    }

    #[test]
    fn docker_and_cc_must_be_available_in_ci() {
        // Same canary pattern as the `age`/`age-keygen` one (robustness review: "Age-CI slice"):
        // both tests above silently report PASS, not fail, when docker/cc are absent, so if
        // GitHub's ubuntu-latest runner ever stopped shipping a running docker daemon or a C
        // compiler, this file's only real-container coverage would vanish with an all-green
        // build. This makes that specific regression loud in CI while staying a quiet skip on a
        // contributor's machine that doesn't have docker/cc installed.
        if std::env::var("CI").is_err() {
            eprintln!(
                "skipping (not running in CI): only enforced as a hard failure when $CI is set"
            );
            return;
        }
        assert!(
            docker_available(),
            "`docker` must be running in CI for real-container coverage"
        );
        assert!(
            cc_available(),
            "`cc` must be on PATH in CI to build the test image"
        );
    }
}
