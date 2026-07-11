//! Integration: `--dest <name>` on `nrg exec`/`nrg run`/`nrg rollback` (roadmap 2.2) — namespacing
//! `.energize/state.json` by destination so two environments deployed from the same directory
//! (e.g. staging vs. production) don't share one state keyspace, plus the `nrg_dest()` builtin
//! and the per-destination `.energize/secrets.<dest>` file convention.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
}

fn project(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join(".energize")).unwrap();
    fs::write(dir.join("Energize.rhai"), script).unwrap();
}

#[test]
fn without_dest_state_is_unnamespaced_exactly_as_before() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"state_set("app.version", "v1");"#);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let raw = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(raw.contains("\"app.version\": \"v1\""), "got: {raw}");
    assert!(!raw.contains('/'), "no dest was given, so no key should ever be namespaced: {raw}");
}

#[test]
fn dest_namespaces_state_so_two_destinations_never_share_a_version() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"state_set("app.version", "should-not-run-twice");"#);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "production"])
        .assert()
        .success();
    // A plain (no --dest) run must see NEITHER destination's key.
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let raw = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(raw.contains("\"staging/app.version\""), "got: {raw}");
    assert!(raw.contains("\"production/app.version\""), "got: {raw}");
    assert!(raw.contains("\"app.version\": \"should-not-run-twice\""), "got: {raw}");
}

#[test]
fn dest_scoped_state_get_set_all_do_not_cross_destinations() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        state_set("app.version", "staging-version");
        print("all_len:" + state_all().len());
        print("has_default:" + has_state("app.version").to_string());
        "#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success()
        .stderr(predicates::str::contains("all_len:1"))
        .stderr(predicates::str::contains("has_default:true")); // true WITHIN staging's own namespace

    // Now run the SAME script with no --dest — state_all()/has_state() must not see staging's key.
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        print("all_len:" + state_all().len());
        print("has_default:" + has_state("app.version").to_string());
        "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("all_len:0"))
        .stderr(predicates::str::contains("has_default:false"));
}

#[test]
fn nrg_dest_reports_default_without_the_flag_and_the_name_with_it() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"print("dest:" + nrg_dest());"#);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("dest:default"));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success()
        .stderr(predicates::str::contains("dest:staging"));

    // "--dest default" is explicitly documented as identical to omitting the flag.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "default"])
        .assert()
        .success()
        .stderr(predicates::str::contains("dest:default"));
}

#[test]
fn invalid_dest_name_is_rejected_with_a_clear_error_before_touching_state() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"state_set("app.version", "v1");"#);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "../etc"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid --dest"));

    // Nothing was written — state.json must not even exist.
    assert!(!dir.path().join(".energize/state.json").exists());
}

#[test]
fn dest_scoped_secrets_file_is_preferred_over_the_shared_one() {
    // A revealed secret is redacted in nrg's own output (by design — see tests/secrets.rs), so
    // prove WHICH value was resolved by writing it to a file via a real command instead of
    // printing it directly.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/secrets"), "DB_URL=shared-value\n").unwrap();
    fs::write(dir.path().join(".energize/secrets.staging"), "DB_URL=staging-value\n").unwrap();
    let outfile = dir.path().join("captured.txt");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"local_exec("printf %s " + sh_quote(secret("DB_URL")) + " > {out}");"#,
            out = outfile.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&outfile).unwrap(), "staging-value");

    // No --dest: falls back to the shared file, exactly as before this feature existed.
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();
    assert_eq!(fs::read_to_string(&outfile).unwrap(), "shared-value");
}

#[test]
fn dest_scoped_run_falls_back_to_the_shared_secrets_file_for_a_key_its_own_file_lacks() {
    // Opus review, round 7: the unit-level `lookup_secret` test for this fallback exists, but
    // nothing proved it through the REAL CLI with an active --dest. A destination's secrets file
    // only needs to hold the keys that actually differ per environment — SHARED_KEY here is
    // absent from .energize/secrets.staging entirely, not merely overridden.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/secrets"), "SHARED_KEY=shared-only-value\n").unwrap();
    fs::write(dir.path().join(".energize/secrets.staging"), "DB_URL=staging-value\n").unwrap();
    let outfile = dir.path().join("captured.txt");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"local_exec("printf %s " + sh_quote(secret("SHARED_KEY")) + " > {out}");"#,
            out = outfile.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&outfile).unwrap(), "shared-only-value");
}

#[test]
fn run_also_supports_dest() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn stamp() { state_set("app.version", "v-from-run"); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "stamp", "--dest", "staging"])
        .assert()
        .success();

    let raw = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(raw.contains("\"staging/app.version\": \"v-from-run\""), "got: {raw}");
}

#[test]
fn rollback_dest_finds_the_hosts_recorded_under_that_destination() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        state_set("app.target.web1", "localhost:13000");
        "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dest", "staging"])
        .assert()
        .success();

    // Without --dest, rollback must find NOTHING (the default namespace has no such state).
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no hosts recorded"));

    // With the matching --dest, rollback finds staging's own recorded host/target.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dest", "staging", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("web1"))
        .stdout(predicates::str::contains("pull 'ghcr.io/org/app:v1'"));
}

#[test]
fn rollback_no_hosts_error_names_the_destination_actually_checked() {
    // Opus review, round 7: a user who forgot --dest (the service was really deployed under a
    // named destination) previously got a generic "no hosts recorded" with no hint that only the
    // DEFAULT namespace was ever checked — confusing on the one command reached for during an
    // incident. The error must now name which destination it looked under.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(dir.path().join("Energize.rhai"), "").unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("\"default\""));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dest", "staging", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("\"staging\""));
}
