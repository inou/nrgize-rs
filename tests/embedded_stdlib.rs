//! Integration: the embedded stdlib (roadmap 3.2) — `import "std/X"` resolves from the binary
//! with zero `lib/` vendoring, `import "lib/X"` keeps requiring a real on-disk file exactly as
//! before, and `nrg vendor` materializes the embedded modules for local customization.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn project(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join(".energize")).unwrap();
    fs::write(dir.join("Energize.rhai"), script).unwrap();
}

#[test]
fn std_import_works_with_zero_vendored_lib_directory() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        r#"
        import "std/runtime" as rt;
        print("runtime:" + rt::runtime_name());
        "#,
    );
    assert!(!dir.path().join("lib").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("runtime:docker"));
}

#[test]
fn lib_import_still_fails_without_vendoring_no_silent_fallback() {
    // "lib/X" must NEVER silently fall back to the embedded copy — only "std/X" does. Proves the
    // two namespaces stay genuinely disjoint for a project's own engine.
    let dir = tempfile::tempdir().unwrap();
    project(dir.path(), r#"import "lib/runtime" as rt;"#);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("lib/runtime"));
}

#[test]
fn nrg_vendor_materializes_every_embedded_module() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("vendor")
        .assert()
        .success()
        .stdout(predicates::str::contains("wrote"));

    for name in ["runtime", "docker", "proxy", "caddy", "healthcheck", "registry", "deploy", "recipe"] {
        let path = dir.path().join("lib").join(format!("{name}.rhai"));
        assert!(path.exists(), "expected {path:?} to have been written");
        let repo_original =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("lib").join(format!("{name}.rhai")))
                .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            repo_original,
            "vendored {name}.rhai must match the repo's own lib/{name}.rhai byte-for-byte"
        );
    }

    // A vendored project can now use "lib/X" (or "std/X" — either works, but "lib/X" no longer
    // errors, proving the vendored files are real and complete/self-consistent).
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"import "lib/runtime" as rt; print("runtime:" + rt::runtime_name());"#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .assert()
        .success()
        .stderr(predicates::str::contains("runtime:docker"));
}

#[test]
fn nrg_vendor_refuses_to_overwrite_a_customized_file_without_force() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("vendor").assert().success();

    fs::write(dir.path().join("lib/runtime.rhai"), "// customized by hand\n").unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("vendor")
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped"));
    assert_eq!(
        fs::read_to_string(dir.path().join("lib/runtime.rhai")).unwrap(),
        "// customized by hand\n",
        "the customized file must survive a re-run without --force"
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["vendor", "--force"])
        .assert()
        .success();
    assert_ne!(
        fs::read_to_string(dir.path().join("lib/runtime.rhai")).unwrap(),
        "// customized by hand\n",
        "--force must overwrite the customized file"
    );
}

#[test]
fn nrg_rollback_uses_the_embedded_stdlib_with_zero_vendored_lib() {
    let dir = tempfile::tempdir().unwrap();
    project(
        dir.path(),
        r#"
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        state_set("app.target.web1", "localhost:13000");
        "#,
    );
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();
    assert!(!dir.path().join("lib").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["rollback", "app", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("pull 'ghcr.io/org/app:v1'"));
}
