//! Integration: `--lock-timeout` bounds how long `nrg exec`/`nrg run` wait for a contended state
//! lock, instead of blocking forever (robustness review: "Blocking lock wait has no timeout").

use assert_cmd::Command;
use std::fs;
use std::process::Stdio;

#[test]
fn lock_timeout_gives_up_with_a_clear_error_when_another_run_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    // Holds the lock for well longer than the second run's timeout below.
    fs::write(dir.path().join("Energize.rhai"), "sleep(3);").unwrap();

    // Start a long-running live run in the background — it takes the lock in `wire_run` before
    // `sleep(3)` even starts, and holds it for the whole 3s. `assert_cmd::Command` has no public
    // `spawn`/stdio-config API, so use plain `std::process::Command` for this one, pointed at
    // the same test binary via the `CARGO_BIN_EXE_<name>` env var Cargo sets for integration
    // tests.
    let mut holder = std::process::Command::new(env!("CARGO_BIN_EXE_nrg"))
        .current_dir(dir.path())
        .arg("exec")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Give the holder a moment to actually acquire the lock before contending it.
    std::thread::sleep(std::time::Duration::from_millis(400));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--lock-timeout")
        .arg("1")
        .assert()
        .failure()
        .stderr(predicates::str::contains("timed out after 1s"))
        .stderr(predicates::str::contains("--lock-timeout"));

    holder.wait().unwrap();
}

#[test]
fn lock_timeout_actually_waits_out_a_shorter_lived_holder_instead_of_timing_out_instantly() {
    // Pins the property a "the timeout fires unconditionally" mutant would still pass: the
    // holder releases the lock partway through the contender's much longer budget, and the
    // contender must actually wait for it (not just succeed because it never contended, and
    // not just fail because *some* timeout check tripped without regard to elapsed time).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "sleep(1);").unwrap();

    let mut holder = std::process::Command::new(env!("CARGO_BIN_EXE_nrg"))
        .current_dir(dir.path())
        .arg("exec")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Give the holder a moment to actually acquire the lock before contending it.
    std::thread::sleep(std::time::Duration::from_millis(400));

    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("k", "v"); print(state_get("k"));"#,
    )
    .unwrap();

    let start = std::time::Instant::now();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--lock-timeout")
        .arg("10")
        .assert()
        .success()
        .stderr(predicates::str::contains("v"));
    // The holder released at ~1s; a contender that never actually waited (e.g. a broken
    // "always timed out" or "never timed out, just happened to succeed on the first poll"
    // implementation) would either fail above or return in well under this window.
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(300),
        "expected the contender to have waited for the holder's ~1s lock hold, not succeeded \
         immediately"
    );

    holder.wait().unwrap();
}

#[test]
fn lock_timeout_does_not_interfere_with_an_uncontended_run() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("k", "v"); print(state_get("k"));"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--lock-timeout")
        .arg("30")
        .assert()
        .success()
        .stderr(predicates::str::contains("v"));
}

#[test]
fn nrg_run_also_accepts_lock_timeout() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"fn deploy_it() { state_set("k", "v"); print(state_get("k")); }"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .arg("deploy_it")
        .arg("--lock-timeout")
        .arg("30")
        .assert()
        .success()
        .stderr(predicates::str::contains("v"));
}
