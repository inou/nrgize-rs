//! Integration: running `nrg` from a SUBDIRECTORY of a project still resolves secrets and the
//! default orchestration file against the PROJECT ROOT, not CWD (issue #19).

use assert_cmd::Command;
use std::fs;

#[test]
fn secrets_resolve_against_project_root_from_a_subdir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".energize")).unwrap();
    // Project-level secret file at the ROOT (not the subdir).
    fs::write(root.join(".energize/secrets"), "API_TOKEN=rootsecretvalue\n").unwrap();
    // The orchestration file lives at the root and reveals the secret onto a captured file.
    let captured = root.join("captured.txt");
    fs::write(
        root.join("Energize.rhai"),
        format!(
            r#"let t = secret("API_TOKEN");
               local_exec("printf %s " + sh_quote(t) + " > {out}");"#,
            out = captured.display()
        ),
    )
    .unwrap();

    // Run from a nested subdirectory with NO secrets/Energize of its own.
    let sub = root.join("services/web");
    fs::create_dir_all(&sub).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub) // CWD is the subdir; the root is discovered upward
        .arg("exec")
        .assert()
        .success();

    // The secret was found (resolved against the root) and delivered to the command.
    let got = fs::read_to_string(&captured).unwrap();
    assert_eq!(got, "rootsecretvalue", "secret must resolve against the project root, not CWD");
}

#[test]
fn default_file_is_found_from_a_subdir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".energize")).unwrap();
    fs::write(
        root.join("Energize.rhai"),
        r#"print("ran-from-root-file");"#,
    )
    .unwrap();
    let sub = root.join("a/b");
    fs::create_dir_all(&sub).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub)
        .arg("exec") // no file arg -> must discover Energize.rhai at the root
        .assert()
        .success()
        .stderr(predicates::str::contains("ran-from-root-file"));
}
