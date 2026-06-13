//! Integration: the advisory state lock SERIALIZES concurrent live runs, and a NESTED `nrg`
//! invocation (spawned by a script) inherits `NRG_STATE_LOCK` and reuses the lock instead of
//! self-deadlocking (issue #27).

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn concurrent_runs_serialize_on_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    // Process A holds the lock for ~2s (sleep is a real sleep in a live run).
    fs::write(
        dir.path().join("hold.rhai"),
        r#"sleep(2); state_set("a", "1");"#,
    )
    .unwrap();
    fs::write(dir.path().join("quick.rhai"), r#"state_set("b", "2");"#).unwrap();

    let bin = cargo_bin("nrg");
    // Spawn A in the background; give it a moment to acquire the lock.
    let mut a = Command::new(&bin)
        .current_dir(dir.path())
        .args(["exec", "hold.rhai"])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));

    // B should block until A releases (~2s total), and announce that it is waiting.
    let start = Instant::now();
    let out = AssertCommand::new(&bin)
        .current_dir(dir.path())
        .args(["exec", "quick.rhai"])
        .assert()
        .success()
        .get_output()
        .clone();
    let waited = start.elapsed();

    a.wait().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Waiting for the state lock"),
        "B should report waiting for the lock; stderr:\n{stderr}"
    );
    assert!(
        waited >= Duration::from_millis(800),
        "B should have blocked on A's lock (waited {waited:?})"
    );
    // Both writes landed (serialized, not lost).
    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(state.contains("\"a\"") && state.contains("\"b\""), "both writes must persist: {state}");
}

#[test]
fn nested_nrg_inherits_the_lock_and_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let bin = cargo_bin("nrg");

    // The inner run writes its own key; if it tried to re-acquire the exclusive flock the outer
    // already holds, it would deadlock (and the test would hang/time out).
    fs::write(dir.path().join("inner.rhai"), r#"state_set("inner", "ok");"#).unwrap();
    // The outer run holds the lock, then shells out to a NESTED nrg (inheriting NRG_STATE_LOCK).
    fs::write(
        dir.path().join("outer.rhai"),
        format!(
            r#"state_set("outer", "ok");
               let r = local_exec({bin:?} + " exec inner.rhai");
               if !r.ok {{ throw "nested nrg failed: " + r.stderr; }}"#,
            bin = bin.to_string_lossy()
        ),
    )
    .unwrap();

    AssertCommand::new(&bin)
        .current_dir(dir.path())
        .args(["exec", "outer.rhai"])
        .timeout(Duration::from_secs(20)) // a deadlock would hit this
        .assert()
        .success();

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(state.contains("\"outer\""), "outer write missing: {state}");
    assert!(state.contains("\"inner\""), "nested write missing (deadlock or lost): {state}");
}
