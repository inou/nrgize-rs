//! Robustness review R9: `SshConfig::resolve_host` only understands `HostName`/`User` from
//! `~/.ssh/config`, built a `user@hostname` string from those two alone, and handed THAT literal
//! address to `ssh` as the connection target. Because the argument was no longer the alias, ssh's
//! own `Host` block matching never fired again for it — silently dropping `Port`, `IdentityFile`,
//! `ProxyJump`, `ProxyCommand`, `IdentitiesOnly`, `Host *` wildcards, `Match` blocks, etc. An alias
//! defined with `Port 2222` connected on 22 instead.
//!
//! Every CLI command that spawns a real `ssh` (`nrg ssh`, `nrg app exec`, `nrg logs`) now passes
//! the ORIGINAL alias straight through, letting the real `ssh` binary do its own, complete config
//! resolution — exactly like a plain interactive `ssh <alias>` would. Verified end-to-end here via
//! a fake `ssh` executable on PATH that just records its own argv: a `~/.ssh/config` Host block
//! maps the test alias to a DIFFERENT, distinctive `HostName`/`User` that would have been used
//! under the old scheme — if the fake ssh's logged argv contains the alias (and NOT that
//! substitute address), the fix is doing its job.
//!
//! (`RealRunner::ssh_command`, used by `nrg exec`/`nrg run`, has no `SshConfig` to consult at all
//! any more — see the unit tests in `src/engine/runner.rs` — so there's no separate case to prove
//! here beyond what's already true by construction.)

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Write a fake `ssh` executable to `<dir>/ssh` that appends each of its own args, one per line,
/// to `log_path`, then exits 0 — standing in for a real network connection.
fn fake_ssh_bin(dir: &Path, log_path: &Path) {
    let script = format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {log_path:?}; done\nexit 0\n");
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

/// Write `<home>/.ssh/config` with a Host block for `alias` mapping to a HostName/User that would
/// have been used INSTEAD of the alias under the old hand-resolution scheme.
fn write_ssh_config(home: &Path, alias: &str) {
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(
        home.join(".ssh/config"),
        format!("Host {alias}\n    HostName resolved-instead.example\n    User configuser\n"),
    )
    .unwrap();
}

fn logged_args(log_path: &Path) -> Vec<String> {
    fs::read_to_string(log_path).unwrap_or_default().lines().map(|s| s.to_string()).collect()
}

fn assert_alias_passed_through(args: &[String], alias: &str) {
    assert!(
        args.iter().any(|a| a == alias),
        "ssh must be invoked with the ORIGINAL alias {alias:?}, not a resolved hostname: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("resolved-instead.example") || a.contains("configuser")),
        "must NOT connect to the hand-resolved HostName/User — ssh itself resolves the alias \
         (that's the whole point of R9): {args:?}"
    );
}

#[test]
fn nrg_ssh_passes_the_alias_through_unresolved() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    write_ssh_config(home.path(), "myalias");

    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", &path)
        .args(["ssh", "myalias"])
        .assert()
        .success();

    assert_alias_passed_through(&logged_args(&log), "myalias");
}

#[test]
fn nrg_app_exec_passes_the_alias_through_unresolved() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".energize")).unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    write_ssh_config(home.path(), "myalias");

    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", &path)
        .args(["app", "exec", "myservice", "--host", "myalias"])
        .assert()
        .success();

    assert_alias_passed_through(&logged_args(&log), "myalias");
}

#[test]
fn nrg_logs_passes_the_alias_through_unresolved() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".energize")).unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    write_ssh_config(home.path(), "myalias");

    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("PATH", &path)
        .args(["logs", "myservice", "--host", "myalias"])
        .assert()
        .success();

    assert_alias_passed_through(&logged_args(&log), "myalias");
}
