//! Integration: `nrg rollback` (roadmap 3.3) — the CLI wiring around
//! `engine::eval::run_rollback` (unit-tested directly in `src/engine/eval.rs`): host discovery
//! from state, `--host`/`--image` overrides, and the dry-run plan.
//!
//! All run against `--dry-run` (no real `ssh`/`docker` on PATH needed) using the real
//! `lib/deploy.rhai`, the same approach `tests/deploy_behaviors.rs` uses for `nrg exec`.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;
use std::path::Path;

fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
}

/// A project with the real stdlib linked in and some state seeded via a throwaway `nrg exec`.
fn project_with_state(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();
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
        .args(["rollback", "app", "--image", "ghcr.io/org/app:v9", "--dry-run"])
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
    link_lib(dir.path());
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
fn missing_lib_deploy_is_a_clear_error() {
    // A project with Energize.rhai + state but NO lib/ directory at all.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "").unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec"])
        .assert()
        .success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--host", "web1", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("lib/deploy.rhai"));
}
