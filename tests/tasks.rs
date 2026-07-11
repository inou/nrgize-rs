//! Integration: `nrg tasks` lists the functions defined in the orchestration file. Robustness
//! review: zero test coverage existed for this command.

use assert_cmd::Command;
use std::fs;

#[test]
fn tasks_lists_every_defined_function_with_its_arg_count() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        "fn deploy() {}\nfn rollback(service) {}\nfn scale(service, count) {}\n",
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("tasks")
        .assert()
        .success()
        .stdout(predicates::str::contains("deploy"))
        .stdout(predicates::str::contains("rollback"))
        .stdout(predicates::str::contains("(1 arg)"))
        .stdout(predicates::str::contains("scale"))
        .stdout(predicates::str::contains("(2 args)"));
}

#[test]
fn tasks_reports_no_functions_defined_on_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Energize.rhai"), "// no functions here\n").unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("tasks")
        .assert()
        .success()
        .stdout(predicates::str::contains("No functions defined"));
}

#[test]
fn tasks_reports_an_error_when_no_orchestration_file_is_found() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("tasks")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Error"));
}

#[test]
fn tasks_reports_a_compile_error_instead_of_crashing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Energize.rhai"), "fn deploy( {").unwrap(); // unbalanced paren

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("tasks")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Error"));
}
