//! Integration: `nrg ssh`'s option-injection guard (`src/cli/ssh.rs`). Robustness review: zero
//! test coverage existed for this guard. `clap` itself already refuses a positional `host` that
//! LOOKS like a flag (e.g. `-oProxyCommand=...`) unless the caller passes `--` first — but a
//! caller (or a wrapper script) that DOES pass `--` reaches `execute()`'s own explicit refusal,
//! which exists as defense-in-depth against connecting to an attacker-shaped alias that would
//! otherwise run an arbitrary `ProxyCommand` on this machine. This test proves that second layer
//! actually fires, and — critically — that the real `ssh` binary is never invoked at all when it
//! does (a fake `ssh` on PATH that would log its own invocation if it were ever run).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Write a fake `ssh` executable to `<dir>/ssh` that just appends a marker line to `log_path`
/// and exits 0 — standing in for a real network connection. If this ever gets invoked, the
/// option-injection guard failed to stop it beforehand.
fn fake_ssh_bin(dir: &Path, log_path: &Path) {
    let script = format!("#!/bin/sh\nprintf 'invoked: %s\\n' \"$*\" >> {log_path:?}\nexit 0\n");
    let bin = dir.join("ssh");
    fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

#[test]
fn nrg_ssh_refuses_an_option_shaped_host_without_ever_invoking_real_ssh() {
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    // `--` gets an option-shaped host past clap's own positional-arg parsing, reaching
    // execute()'s explicit guard — the scenario the guard exists to defend against.
    Command::cargo_bin("nrg")
        .unwrap()
        .env("PATH", &path)
        .args(["ssh", "--", "-oProxyCommand=touch /tmp/pwned"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("refusing to connect to a host that looks like an option"));

    assert!(
        !log.exists(),
        "the real ssh binary must never be invoked for an option-shaped host: {:?}",
        fs::read_to_string(&log)
    );
}

#[test]
fn nrg_ssh_refuses_a_bare_dash_prefixed_host() {
    // A narrower, more common shape than a full `-oProxyCommand=...` — anything starting with
    // `-` must be refused, not just recognizable long-option strings.
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .env("PATH", &path)
        .args(["ssh", "--", "-x"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("refusing to connect"));

    assert!(!log.exists(), "real ssh must never be invoked for a dash-prefixed host");
}
