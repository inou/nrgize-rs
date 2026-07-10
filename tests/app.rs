//! Integration: `nrg app exec` fails fast (no network attempted) on host-selection ambiguity —
//! no hosts recorded, or multiple hosts without `--host`. The actual exec-into-ssh handoff
//! replaces the process and needs a real host, so it's covered by unit tests on `pick_host`/
//! `build_remote_cmd` in `src/cli/app.rs` instead.

use assert_cmd::Command;
use std::fs;

#[test]
fn app_exec_on_service_with_no_recorded_hosts_errors_without_network() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["app", "exec", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no hosts recorded"));
}

#[test]
fn app_exec_on_service_with_multiple_hosts_requires_explicit_host() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
state_set("app.target.web1", "localhost:13000");
state_set("app.target.web2", "localhost:13010");
"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["app", "exec", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("web1"))
        .stderr(predicates::str::contains("web2"))
        .stderr(predicates::str::contains("--host"));
}

#[test]
fn app_exec_refuses_a_host_that_looks_like_an_ssh_option() {
    // Same option-injection guard as `nrg ssh`/`nrg logs`: an explicit `--host` starting with
    // `-` must be rejected before ssh is ever spawned. Network-free (rejected pre-connection).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["app", "exec", "app", "--host=-oProxyCommand=touch /tmp/pwned"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("looks like an option"));
}

#[test]
fn app_exec_help_documents_interactive_flag() {
    Command::cargo_bin("nrg")
        .unwrap()
        .args(["app", "exec", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--interactive"))
        .stdout(predicates::str::contains("--host"));
}
