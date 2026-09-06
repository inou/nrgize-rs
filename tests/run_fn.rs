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

/// `nrg run <fn> --dry-run` records the plan and executes nothing (issue #27): the function's
/// side effects are recorded, not run, and the plan is printed.
#[test]
fn run_dry_run_records_plan_without_executing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let sentinel = dir.path().join("should-not-exist");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"fn ship() {{ local_exec("touch {s}"); state_set("shipped", "yes"); }}"#,
            s = sentinel.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("ship")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains("PLAN (dry run"))
        // The plan names the state KEY and the value's size — never the value itself.
        .stdout(predicates::str::contains("shipped = <3 bytes>"))
        .stdout(predicates::str::contains("0 executed."));

    assert!(
        !sentinel.exists(),
        "dry-run must not execute the local_exec"
    );
    assert!(
        !dir.path().join(".energize/state.json").exists(),
        "dry-run must not write state.json"
    );
}

/// Numeric-looking CLI args stay STRINGS (the documented contract): `fn f(n)` receives "0042",
/// not the integer 42, so string ops work and there's no surprise numeric coercion.
#[test]
fn run_numeric_args_stay_strings() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn echo_len(n){ print("len:" + n.len() + ":" + n); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("echo_len")
        .arg("0042")
        .assert()
        .success()
        // .len() is a String method; if the arg were an int this would error. "0042" -> len 4.
        .stderr(predicates::str::contains("len:4:0042"));
}

/// Wrong arity fails WITHOUT running the top level (issue #18): the side effect must not happen.
#[test]
fn run_wrong_arity_does_not_run_top_level() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let sentinel = dir.path().join("top-level-ran");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"local_exec("touch {s}"); fn deploy(host){{ print(host); }}"#,
            s = sentinel.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("deploy") // expects 1 arg, given 0
        .assert()
        .failure();
    assert!(
        !sentinel.exists(),
        "the top level must not run on an arity mismatch"
    );
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
