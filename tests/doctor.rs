//! Integration: `nrg doctor`'s host preflight (`--host`, or auto-discovered from state). Only
//! the NETWORK-FREE path — "nothing to check, skip the section entirely" — is covered here; the
//! actual per-host SSH-reachability/runtime-detection logic is unit-tested via `FakeRunner` in
//! `src/cli/doctor.rs`, since a real integration test would need a real reachable host and would
//! be slow/flaky in CI (same reasoning already applied to `nrg logs`/`nrg app exec`).

use assert_cmd::Command;
use std::fs;

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
fn doctor_help_documents_the_host_flag() {
    Command::cargo_bin("nrg")
        .unwrap()
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--host"));
}
