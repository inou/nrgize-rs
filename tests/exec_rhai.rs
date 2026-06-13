//! Integration test: `nrg exec` runs a Rhai orchestration module end-to-end.

use assert_cmd::Command;
use std::fs;

#[test]
fn exec_runs_a_local_rhai_script() {
    let dir = tempfile::tempdir().unwrap();
    // Mark this temp dir as the project root so `find_project_root` anchors HERE and the state
    // lock is created under THIS dir — never the repo (issue #26). Every other test does this;
    // without `.current_dir(...)` + a `.energize` marker, the spawned `nrg` would lock/write into
    // the developer's repo, serializing parallel tests and possibly mutating real state.
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
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
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicates::str::contains("hello-from-rhai"));
}
