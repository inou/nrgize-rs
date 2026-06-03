//! Integration test: `nrg exec` runs a Rhai orchestration module end-to-end.

use assert_cmd::Command;
use std::fs;

#[test]
fn exec_runs_a_local_rhai_script() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("Energize.rhai");
    // local_exec runs `sh -c` for real; echo is safe and host-independent.
    // NOTE: Rhai's String::trim() mutates in place and returns (), so we print
    // r.stdout directly (the trailing newline is harmless for a `contains` check).
    fs::write(
        &script,
        r#"let r = local_exec("echo hello-from-rhai"); print(r.stdout);"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .arg("exec")
        .arg(script.to_str().unwrap())
        .assert()
        .success()
        .stderr(predicates::str::contains("hello-from-rhai"));
}
