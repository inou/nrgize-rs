//! scripts tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn runs_a_script_that_imports_a_lib_module() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("lib")).unwrap();
    // lib/docker.rhai defines pull() which calls the GLOBAL ssh_exec builtin.
    fs::write(
        dir.path().join("lib/docker.rhai"),
        r#"fn pull(host, img) { ssh_exec(host, "docker pull " + img); }"#,
    )
    .unwrap();
    // main calls the imported module fn.
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker; docker::pull("web1", "nginx:latest");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    run_file(&main, shared(fake.clone())).unwrap();
    // PROVES the global builtin executed from inside the imported module fn.
    assert_eq!(
        fake.calls(),
        vec!["ssh web1: docker pull nginx:latest".to_string()]
    );
}

#[test]
fn run_fn_resolves_calls_into_a_module_that_itself_imports() {
    // Regression: `nrg run ship` where ship calls a module fn (`mid::build`) and that module
    // itself `import`s another (`base`). Rhai's `call_fn` cannot resolve this nested-import
    // case (it fails "Function not found: ship"); run_fn appends the call + run_ast instead.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("lib")).unwrap();
    fs::write(dir.path().join("lib/base.rhai"), r#"fn tag() { "v1" }"#).unwrap();
    fs::write(
        dir.path().join("lib/mid.rhai"),
        r#"import "lib/base" as base; fn build(host) { ssh_exec(host, "build " + base::tag()); }"#,
    )
    .unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/mid" as mid; fn ship(host) { mid::build(host); }"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    run_fn(&main, "ship", &["web1".to_string()], shared(fake.clone())).unwrap();
    assert_eq!(fake.calls(), vec!["ssh web1: build v1".to_string()]);
}

#[test]
fn run_fn_refuses_missing_function_without_running_top_level() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    // A top-level script with NO function definitions and a side effect that must NOT run.
    fs::write(&main, r#"local_exec("touch should-not-run");"#).unwrap();
    let fake = FakeRunner::shared();
    let err = run_fn(&main, "deploy", &[], shared(fake.clone())).unwrap_err();
    assert!(err.contains("no function"), "got: {err}");
    assert!(
        fake.calls().is_empty(),
        "the top-level must NOT run when the named function is missing"
    );
}

#[test]
fn list_functions_lifts_the_expression_depth_cap() {
    // A long `+` chain exceeds Rhai's default function-body expression-depth cap (32) — the
    // real stdlib hits this with its command/message string concatenations. `list_functions`
    // (backing `nrg tasks`/`nrg doctor`) must lift the cap like `build_engine` does.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    let chain: String = (0..50)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(" + ");
    fs::write(&main, format!("fn total() {{ {chain} }}")).unwrap();
    let fns = list_functions(&main).unwrap();
    assert!(fns.iter().any(|f| f.name == "total"));
}

#[test]
fn run_fn_wrong_arity_does_not_run_top_level() {
    // Regression (#18): `nrg run deploy` with the wrong arg count must fail on arity WITHOUT
    // evaluating the top level (which here has a side effect that must NOT happen).
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"local_exec("touch should-not-run"); fn deploy(host) { ssh_exec(host, "x"); }"#,
    )
    .unwrap();
    let fake = FakeRunner::shared();
    // deploy takes 1 arg; call with 0.
    let err = run_fn(&main, "deploy", &[], shared(fake.clone())).unwrap_err();
    assert!(
        err.contains("expects 1") && err.contains("Nothing was run"),
        "got: {err}"
    );
    assert!(
        fake.calls().is_empty(),
        "the top level must NOT run on an arity mismatch"
    );
}

#[test]
fn run_fn_rejects_non_identifier_name() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(&main, r#"fn ok() {}"#).unwrap();
    let fake = FakeRunner::shared();
    let err = run_fn(&main, "ok(); evil_call(", &[], shared(fake.clone())).unwrap_err();
    assert!(err.contains("not a valid function name"), "got: {err}");
    assert!(fake.calls().is_empty());
}

#[test]
fn parse_error_is_reported_with_path() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("bad.rhai");
    fs::write(&main, "let x = ;").unwrap();
    let err = run_file(&main, shared(FakeRunner::shared())).unwrap_err();
    assert!(err.contains("parse error"));
}
