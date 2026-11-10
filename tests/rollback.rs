//! Integration: `nrg rollback` — the CLI wiring around
//! `engine::eval::run_rollback` (unit-tested directly in
//! `src/engine/eval/tests/rollback.rs`): host discovery
//! from state, `--host`/`--image` overrides, and the dry-run plan.
//!
//! All run against `--dry-run` (no real `ssh`/`docker` on PATH needed) using the real
//! `lib/deploy.rhai`, the same approach `tests/deploy_behaviors.rs` uses for `nrg exec`.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;
use std::path::Path;

/// A project with the real stdlib linked in and some state seeded via a throwaway `nrg exec`.
fn project_with_state(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success();
    dir
}

const SEED_SCRIPT: &str = r#"
state_set("app.image", "ghcr.io/org/app:v2");
state_set("app.prev", "ghcr.io/org/app:v1");
state_set("app.target.web1", "localhost:13000");
"#;

#[test]
fn dry_run_rolls_back_to_the_snapshotted_prev_image_on_every_recorded_host() {
    let dir = project_with_state(SEED_SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("pull 'ghcr.io/org/app:v1'"))
        .stdout(predicates::str::contains("web1"))
        // the CURRENT image (v2) must never be the one PULLED/deployed — only recorded as the
        // new rollback target (`app.prev = ...v2`), which is rollback()'s own expected side effect.
        .stdout(predicates::str::contains("pull 'ghcr.io/org/app:v2'").not());
}

#[test]
fn image_flag_overrides_the_snapshotted_prev() {
    let dir = project_with_state(SEED_SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "rollback",
            "app",
            "--image",
            "ghcr.io/org/app:v9",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("ghcr.io/org/app:v9"))
        .stdout(predicates::str::contains("ghcr.io/org/app:v1").not());
}

#[test]
fn host_flag_overrides_the_recorded_fleet() {
    let dir = project_with_state(&format!(
        "{SEED_SCRIPT}\nstate_set(\"app.target.web2\", \"localhost:13001\");"
    ));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--host", "web2", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("web2"))
        .stdout(predicates::str::contains("web1").not());
}

#[test]
fn no_hosts_recorded_is_a_clear_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "").unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "ghost", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no hosts recorded"))
        .stderr(predicates::str::contains("--host"));
}

#[test]
fn host_flag_rejects_an_empty_value() {
    let dir = project_with_state(SEED_SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--host", "", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be empty"));
}

#[test]
fn image_flag_rejects_an_empty_value() {
    // An unset shell variable (`--image "$TAG"` where $TAG is empty) must
    // be refused, not silently treated as "no override" (which would roll back to the
    // snapshotted .prev instead — a different, unintended target).
    let dir = project_with_state(SEED_SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--image", "", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--image cannot be empty"));
}

#[test]
fn file_flag_rejects_a_nonexistent_path() {
    // `nrg rollback` never reads --file's contents (only its directory),
    // so nothing else would ever catch a typo'd path — it would otherwise silently resolve
    // lib/deploy.rhai against whatever happens to sit in the typo'd parent directory instead.
    let dir = project_with_state(SEED_SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "rollback",
            "app",
            "--host",
            "web1",
            "--file",
            "totally-missing.rhai",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not exist"));
}

/// A fake `ssh` that fails instantly (standing in for an unreachable host) — just enough for a
/// LIVE (non-dry-run) `nrg rollback` to reach `execute_with`'s audit-trail write before exiting
/// nonzero, without a real network round trip.
fn fake_ssh_bin_fails_fast(dir: &Path) {
    let script = "#!/bin/sh\nexit 1\n";
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
fn audit_trail_records_the_host_and_image_overrides() {
    // The audit trail used to pass `args: &[]` for every rollback,
    // regardless of --host/--image — indistinguishable from the all-hosts/.prev default. A LIVE
    // run (dry-run never appends to the audit log) that fails fast against a fake, always-failing
    // ssh is enough: `execute_with` writes the audit entry on failure too.
    let dir = project_with_state(SEED_SCRIPT);
    let bin = tempfile::tempdir().unwrap();
    fake_ssh_bin_fails_fast(bin.path());
    let path_env = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args([
            "rollback",
            "app",
            "--host",
            "web1",
            "--image",
            "ghcr.io/org/app:v9",
        ])
        .assert()
        .failure();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("--host=web1"))
        .stdout(predicates::str::contains("--image=ghcr.io/org/app:v9"));
}
