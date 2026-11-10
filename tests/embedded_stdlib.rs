//! Integration: the embedded stdlib — `import "std/X"` resolves from the binary, and any other
//! import is a local file.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn project(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join(".energize")).unwrap();
    fs::write(dir.join("Energize.rhai"), script).unwrap();
}

#[test]
fn std_import_works_in_a_project_without_any_lib_directory() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        r#"
        import "std/runtime" as rt;
        print("runtime:" + rt::runtime_name());
        "#,
    );
    assert!(!dir.path().join("ops").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("runtime:docker"));
}

#[test]
fn a_relative_import_is_a_local_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("ops")).unwrap();
    fs::write(dir.path().join("ops/mine.rhai"), "fn n() { 42 }").unwrap();
    project(
        dir.path(),
        r#"import "ops/mine" as m; print("n:" + m::n());"#,
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("n:42"));
}

#[test]
fn a_lib_import_with_no_local_file_never_resolves_to_the_stdlib() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"import "lib/deploy" as d;"#);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Module not found: lib/deploy"));
}

#[test]
fn nrg_rollback_uses_the_embedded_stdlib() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        r#"
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        state_set("app.target.web1", "localhost:13000");
        "#,
    );
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success();
    assert!(!dir.path().join("ops").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("pull 'ghcr.io/org/app:v1'"));
}
