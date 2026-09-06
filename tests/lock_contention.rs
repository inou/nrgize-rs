//! Integration: the advisory state lock SERIALIZES concurrent live runs, and a NESTED `nrg`
//! invocation (spawned by a script) inherits `NRG_STATE_LOCK` and reuses the lock instead of
//! self-deadlocking (issue #27).

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

/// Poll for `path` to exist, up to `timeout` — used instead of a fixed `sleep` guess at how long
/// a background process takes to reach some point, which is exactly the kind of wall-clock
/// assumption that flakes under CI load (robustness review: "Flaky patterns").
fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {path:?} to appear"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait for `child` to exit, up to `timeout` — never blocks forever. A hung lock-acquisition
/// regression must fail this test loudly instead of hanging CI (robustness review: "Flaky
/// patterns" — `a.wait()` previously had no timeout at all).
fn wait_bounded(child: &mut std::process::Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process A did not exit within {timeout:?} — possible lock hang/deadlock");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn concurrent_runs_serialize_on_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    // Process A writes a marker as its very FIRST statement (after the state lock is already
    // held — wire_run acquires it before running any script statement), then holds the lock for
    // ~2s (sleep is a real sleep in a live run). Polling for this marker replaces a fixed
    // `sleep(400ms)` guess at A's startup/scheduling delay before spawning B, which used to make
    // this test flaky on a loaded CI runner (A might not even reach the lock within 400ms).
    let marker = dir.path().join("a_holds_lock");
    let touch_cmd = format!("touch '{}'", marker.display());
    fs::write(
        dir.path().join("hold.rhai"),
        format!(
            r#"local_exec("{touch}"); sleep(2); state_set("a", "1");"#,
            touch = touch_cmd
        ),
    )
    .unwrap();
    fs::write(dir.path().join("quick.rhai"), r#"state_set("b", "2");"#).unwrap();

    let bin = cargo_bin("nrg");
    let mut a = Command::new(&bin)
        .current_dir(dir.path())
        .args(["exec", "hold.rhai"])
        .spawn()
        .unwrap();
    wait_for_file(&marker, Duration::from_secs(10));

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

    wait_bounded(&mut a, Duration::from_secs(30));
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
    assert!(
        state.contains("\"a\"") && state.contains("\"b\""),
        "both writes must persist: {state}"
    );
}

#[test]
fn nested_nrg_inherits_the_lock_and_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let bin = cargo_bin("nrg");

    // The inner run writes its own key; if it tried to re-acquire the exclusive flock the outer
    // already holds, it would deadlock (and the test would hang/time out).
    fs::write(
        dir.path().join("inner.rhai"),
        r#"state_set("inner", "ok");"#,
    )
    .unwrap();
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
    assert!(
        state.contains("\"inner\""),
        "nested write missing (deadlock or lost): {state}"
    );
}
