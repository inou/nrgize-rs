//! Integration: `nrg remove` (roadmap 1.5 step 2) — the CLI wiring around `remove_container`
//! (unit-tested directly in `src/cli/remove.rs`): host discovery from state, the `--yes`
//! confirmation gate, `--host` override, and `--purge-state`.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// A fake `ssh` on PATH standing in for a real host. Every invocation is logged (so tests can
/// assert whether it ran at all) and its exit behavior is driven by a marker string embedded in
/// the remote command: `remove.rs` builds `docker rm -f '<container>'`, so a container named
/// with one of these markers picks the corresponding canned response.
fn fake_ssh_bin(dir: &Path, log_path: &Path) {
    let log = log_path.display();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log:?}\ncase \"$*\" in\n  *forcefail*) echo 'Error: permission denied' >&2; exit 1 ;;\n  *) exit 0 ;;\nesac\n"
    );
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

fn project_with_state(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();
    dir
}

const SEED_SCRIPT: &str = r#"
state_set("app.version", "v42");
state_set("app.image", "ghcr.io/org/app:v42");
state_set("app.deployed_at", "2026-07-10T00:00:00Z");
state_set("app.target.web1", "localhost:13000");
"#;

#[test]
fn without_yes_only_previews_and_never_invokes_ssh() {
    let dir = project_with_state(SEED_SCRIPT);
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["remove", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Would remove"))
        .stdout(predicates::str::contains("app-web"))
        .stdout(predicates::str::contains("web1"))
        .stdout(predicates::str::contains("--yes"));

    assert!(!log.exists(), "ssh must never run without --yes: {:?}", fs::read_to_string(&log));
}

#[test]
fn with_yes_removes_the_container_on_every_recorded_host() {
    let dir = project_with_state(SEED_SCRIPT);
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["remove", "app", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("web1: removed"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(invoked.contains("web1"), "got: {invoked}");
    assert!(invoked.contains("docker rm -f 'app-web'"), "got: {invoked}");
}

#[test]
fn host_flag_overrides_state_and_skips_lookup_entirely() {
    let dir = project_with_state(SEED_SCRIPT);
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["remove", "app", "--host", "web9", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("web9: removed"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(invoked.contains("web9"), "got: {invoked}");
}

#[test]
fn purge_state_clears_the_services_keys_after_a_successful_removal() {
    let dir = project_with_state(SEED_SCRIPT);
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["remove", "app", "--yes", "--purge-state"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Purged state"));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "app", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("none — no deploy recorded"));
}

#[test]
fn a_failed_host_reports_nonzero_and_purge_state_is_skipped() {
    let dir = project_with_state(&format!(
        "{SEED_SCRIPT}\nstate_set(\"app.target.forcefail\", \"localhost:13001\");"
    ));
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["remove", "app", "--yes", "--purge-state"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("permission denied"))
        .stdout(predicates::str::contains("web1: removed"));

    // --purge-state must NOT have run: a real failure means state no longer matches reality, so
    // wiping the record of it would hide that the host still needs manual attention.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["status", "app", "--offline"])
        .assert()
        .success()
        .stdout(predicates::str::contains("v42"));
}

#[test]
fn no_hosts_recorded_is_a_no_op_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["remove", "ghost", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No hosts recorded"));
}
