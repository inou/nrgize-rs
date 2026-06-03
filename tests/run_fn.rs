//! Integration: `nrg run <fn> [args...]` loads `Energize.rhai` into the engine (the same
//! `build_engine`/module-resolution path as `nrg exec`) and calls the named script function,
//! passing the trailing CLI args as strings.

use assert_cmd::Command;
use std::fs;

/// `nrg run greet world` calls `fn greet(who)` with "world". `print` routes to stderr
/// (redaction-wrapped), so the greeting lands there.
#[test]
fn run_calls_named_fn_with_string_arg() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn greet(who){ print("hi " + who); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("greet")
        .arg("world")
        .assert()
        .success()
        .stderr(predicates::str::contains("hi world"));
}

/// Multiple trailing args are passed positionally, in order.
#[test]
fn run_passes_multiple_args_in_order() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn pair(a, b){ print(a + "-" + b); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("pair")
        .arg("alpha")
        .arg("beta")
        .assert()
        .success()
        .stderr(predicates::str::contains("alpha-beta"));
}

/// The top level of the file runs first (so `import`s + config execute), then the fn is
/// called — a fn that uses a global builtin works end-to-end.
#[test]
fn run_fn_can_call_global_builtins() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn show(){ let r = local_exec("echo from-fn"); print(r.stdout); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("show")
        .assert()
        .success()
        .stderr(predicates::str::contains("from-fn"));
}

/// Calling a function that does not exist surfaces as a non-zero exit (redacted error).
#[test]
fn run_unknown_fn_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn greet(who){ print("hi " + who); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("nope")
        .assert()
        .failure();
}
