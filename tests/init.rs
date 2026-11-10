//! Integration: `nrg init` scaffolds `Energize.rhai`. Zero test coverage
//! existed for this command — in particular its refuse-to-overwrite branch, which exists
//! specifically to avoid clobbering a project's real orchestration file with the starter
//! template.

use assert_cmd::Command;
use std::fs;

#[test]
fn init_creates_the_default_energize_rhai_file() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("Created Energize.rhai"));

    let contents = fs::read_to_string(dir.path().join("Energize.rhai")).unwrap();
    assert!(
        contents.contains("fn deploy()"),
        "template must define a starter deploy() fn:\n{contents}"
    );
    assert!(
        contents.contains("fn uptime()"),
        "template must define a starter uptime() fn:\n{contents}"
    );
}

#[test]
fn init_refuses_to_overwrite_an_existing_energize_rhai() {
    let dir = tempfile::tempdir().unwrap();
    let original = "// a real, hand-written orchestration file\nfn deploy() { print(\"real\"); }\n";
    fs::write(dir.path().join("Energize.rhai"), original).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));

    // The refusal must be real, not just a printed warning — the existing file must survive
    // completely unchanged, not get clobbered by the starter template.
    let contents = fs::read_to_string(dir.path().join("Energize.rhai")).unwrap();
    assert_eq!(
        contents, original,
        "an existing Energize.rhai must not be overwritten by init"
    );
}

#[test]
fn init_template_scaffolds_a_framework_starter_using_the_embedded_stdlib() {
    for (template, needle) in [
        ("rails", "RAILS_ENV"),
        ("django", "DJANGO_SETTINGS_MODULE"),
        ("nextjs", "NODE_ENV"),
        ("phoenix", "PHX_SERVER"),
        ("laravel", "APP_ENV"),
    ] {
        let dir = tempfile::tempdir().unwrap();

        Command::cargo_bin("nrg")
            .unwrap()
            .current_dir(dir.path())
            .args(["init", "--template", template])
            .assert()
            .success()
            .stdout(predicates::str::contains("Created Energize.rhai"));

        let contents = fs::read_to_string(dir.path().join("Energize.rhai")).unwrap();
        assert!(
            contents.contains(needle),
            "{template} starter missing its own marker:\n{contents}"
        );
        assert!(
            contents.contains("import \"std/recipe\" as recipe;"),
            "{template} starter must import the embedded stdlib:\n{contents}"
        );
    }
}

#[test]
fn init_template_rails_actually_runs() {
    // Prove the scaffolded file runs as written, in a directory with no other files.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--template", "rails"])
        .assert()
        .success();

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("DEPLOY_TAG", "v1.2.3")
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_tokenvalue123")
        .env("NRG_SECRET_DATABASE_URL", "postgres://u:secretpw@db/x")
        .env("NRG_SECRET_SECRET_KEY_BASE", "keybasevalue123")
        .env("NRG_SECRET_DB_PASSWORD", "dbpassvalue123")
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();
    let plan = String::from_utf8_lossy(&out.stdout);

    assert!(
        plan.contains("docker run --rm") && plan.contains("bin/rails db:migrate"),
        "the templated rails starter must run its migration in a throwaway new-image container:\n{plan}"
    );
}

#[test]
fn init_template_rejects_an_unknown_framework_name() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--template", "cobol-on-cogs"])
        .assert()
        .failure();

    assert!(
        !dir.path().join("Energize.rhai").exists(),
        "an invalid --template value must not scaffold anything"
    );
}

#[test]
fn init_template_also_refuses_to_overwrite_an_existing_energize_rhai() {
    let dir = tempfile::tempdir().unwrap();
    let original = "// a real, hand-written orchestration file\nfn deploy() { print(\"real\"); }\n";
    fs::write(dir.path().join("Energize.rhai"), original).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--template", "rails"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));

    let contents = fs::read_to_string(dir.path().join("Energize.rhai")).unwrap();
    assert_eq!(
        contents, original,
        "an existing Energize.rhai must not be overwritten by init --template"
    );
}
