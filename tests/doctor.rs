//! Integration: `nrg doctor`'s host preflight (`--host`, or auto-discovered from state). Only
//! the NETWORK-FREE path — "nothing to check, skip the section entirely" — is covered here; the
//! actual per-host SSH-reachability/runtime-detection logic is unit-tested via `FakeRunner` in
//! `src/cli/doctor.rs`, since a real integration test would need a real reachable host and would
//! be slow/flaky in CI (same reasoning already applied to `nrg logs`/`nrg app exec`).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Write a no-op stub executable named `name` into `dir` (accepts any args, exits 0) — stands
/// in for a tool `doctor` checks for on PATH, without depending on what's actually installed in
/// whatever environment the test suite happens to run in.
fn stub_bin(dir: &Path, name: &str) {
    let bin = dir.join(name);
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

#[test]
fn doctor_skips_the_hosts_section_when_no_state_and_no_explicit_host() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();

    let out = Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("doctor").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Hosts:"),
        "must not print a Hosts section when there's nothing to check:\n{stdout}"
    );
}

#[test]
fn doctor_reports_a_corrupt_state_file_as_a_failure_not_a_silent_skip() {
    // Regression: a state.json that EXISTS but fails to parse must surface as a doctor failure
    // (the exact class of problem doctor exists to catch), not be silently treated the same as
    // "no state yet" and skipped — that would hide the corruption from the one command whose
    // job is to report it.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/state.json"), "{ not valid json").unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicates::str::contains("Hosts:"))
        .stdout(predicates::str::contains("CORRUPT"));
}

#[test]
fn doctor_reports_a_corrupt_state_file_even_with_an_explicit_host() {
    // Opus review: unlike auto-discovered hosts (which go through `resolve_hosts`, and so hit
    // the regression test above), an explicit `--host` never even looks at state via
    // `resolve_hosts` — so the registry-auth check's own `images_by_host` lookup was the ONLY
    // place left that would ever see this corrupt file, and an earlier version silently
    // swallowed the load failure into an empty map. That combination — corrupt state.json PLUS
    // an explicit --host — used to print "All checks passed!" and exit 0, hiding the corruption
    // and silently disabling the registry check with no indication at all.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/state.json"), "{ not valid json").unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();

    let bin = tempfile::tempdir().unwrap();
    for tool in ["age", "ssh", "rsync", "docker"] {
        stub_bin(bin.path(), tool);
    }
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["doctor", "--host", "web1"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("CORRUPT"));
}

#[test]
fn doctor_fails_when_the_orchestration_file_does_not_compile() {
    // Robustness review: `execute()`'s `all_ok` accumulator could invert (e.g. a stray
    // `all_ok = true` or a dropped `= false`) and ship unnoticed with zero test coverage
    // catching it end-to-end. This proves the FULL path — a real compile failure must flip
    // `all_ok` and produce a nonzero exit code, not just print a "✗" line while still exiting 0.
    // Every OTHER check must be stubbed to pass, so this test is sensitive to exactly the
    // compile-failure branch's `all_ok = false` and not incidentally "passing" because some
    // unrelated tool check also fails in whatever environment the suite happens to run in.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy( {").unwrap(); // unbalanced paren

    let bin = tempfile::tempdir().unwrap();
    for tool in ["age", "ssh", "rsync", "docker"] {
        stub_bin(bin.path(), tool);
    }
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicates::str::contains("Some checks failed"));
}

#[test]
fn doctor_succeeds_when_the_orchestration_file_compiles_and_nothing_is_deployed() {
    // The success-path counterpart to the failure test above: a real, valid file with no
    // deployed state must exit 0 and report all-clear, proving `all_ok` isn't just biased
    // toward failure either. Stub every tool `doctor` checks for on PATH rather than relying on
    // what's actually installed wherever the suite happens to run — this repo's own CI/sandboxes
    // aren't guaranteed to have `ssh`/`rsync`/`scp` present (only `age` and `docker` are, per the
    // "Install age" CI step and the docker-in-docker test environment).
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();

    let bin = tempfile::tempdir().unwrap();
    for tool in ["age", "ssh", "rsync", "docker"] {
        stub_bin(bin.path(), tool);
    }
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicates::str::contains("All checks passed!"));
}

/// A fake `ssh` on PATH for the registry-auth check: reachable, `docker` on PATH, and
/// `docker manifest inspect` succeeds unless the ssh destination contains `badregistryhost`
/// (same host-marker idiom `tests/setup.rs`'s `fake_ssh_bin` uses).
fn fake_ssh_bin_for_registry(dir: &Path) {
    let bin = dir.join("ssh");
    fs::write(
        &bin,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         \x20\x20*\"command -v\"*) echo \"/usr/bin/docker\"; exit 0 ;;\n\
         \x20\x20*badregistryhost*\"manifest inspect\"*) echo 'Error: unauthorized' >&2; exit 1 ;;\n\
         \x20\x20*) exit 0 ;;\n\
         esac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

#[test]
fn doctor_reports_registry_auth_success_for_a_deployed_image() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();
    fs::write(
        dir.path().join(".energize/state.json"),
        r#"{"version": 1, "data": {"app.version": "v1", "app.image": "ghcr.io/org/app:v1", "app.target.goodhost": "localhost:1"}}"#,
    )
    .unwrap();

    let bin = tempfile::tempdir().unwrap();
    fake_ssh_bin_for_registry(bin.path());
    for tool in ["age", "rsync"] {
        stub_bin(bin.path(), tool);
    }
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicates::str::contains("registry auth OK for ghcr.io/org/app:v1"));
}

#[test]
fn doctor_reports_registry_auth_failure_for_a_deployed_image() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy() {}").unwrap();
    fs::write(
        dir.path().join(".energize/state.json"),
        r#"{"version": 1, "data": {"app.version": "v1", "app.image": "ghcr.io/org/app:v1", "app.target.badregistryhost": "localhost:1"}}"#,
    )
    .unwrap();

    let bin = tempfile::tempdir().unwrap();
    fake_ssh_bin_for_registry(bin.path());
    for tool in ["age", "rsync"] {
        stub_bin(bin.path(), tool);
    }
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicates::str::contains("registry auth failed for ghcr.io/org/app:v1"))
        .stdout(predicates::str::contains("unauthorized"));
}

#[test]
fn doctor_help_documents_the_host_flag() {
    Command::cargo_bin("nrg")
        .unwrap()
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--host"));
}
