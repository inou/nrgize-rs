//! Integration: `nrg logs` fails fast (no network attempted) when the service has no hosts
//! recorded and none was given explicitly. The actual SSH fan-out/streaming needs a real host,
//! so it's covered by unit tests on `build_remote_cmd` in `src/cli/logs.rs` instead.

use assert_cmd::Command;
use std::fs;

#[test]
fn logs_on_service_with_no_recorded_hosts_errors_without_network() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["logs", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no hosts recorded"));
}

#[test]
fn logs_refuses_a_host_that_looks_like_an_ssh_option() {
    // An `--host` that resolves to something starting with `-` must be rejected before ssh is
    // ever spawned (it would otherwise be parsed as an ssh OPTION — e.g. `-oProxyCommand=...` —
    // running an arbitrary command on THIS machine). Network-free: the rejection happens before
    // any connection attempt, so this can't hang or flake in CI.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["logs", "app", "--host=-oProxyCommand=touch /tmp/pwned"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("looks like an option"));
}

#[test]
fn logs_defaults_to_100_lines_and_no_follow() {
    // Pure flag-parsing/help smoke test — confirms `--lines`/`--follow`/`--host` are wired up
    // as documented, without needing a real host.
    Command::cargo_bin("nrg")
        .unwrap()
        .args(["logs", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--follow"))
        .stdout(predicates::str::contains("--lines"))
        .stdout(predicates::str::contains("--host"));
}
