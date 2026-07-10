//! Integration: SIGINT during a live `transaction()` triggers the compensation unwind
//! (robustness review R7) instead of just killing the process with zero cleanup.
//!
//! Spawns a REAL `nrg exec` child process running a script whose `transaction()` body loops
//! with bounded `sleep(1)` calls (so the engine's `on_progress` check gets a chance to fire
//! within about a second of the signal), sends it a real SIGINT shortly after starting via the
//! `kill` utility (no extra dependency needed for this one test), and asserts BOTH that the
//! process actually exits (not stuck) and that its `on_rollback` compensation ran (a marker file
//! the compensation touches exists) — proving the unwind happened, not just that the process died.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
#[test]
fn sigint_mid_transaction_runs_the_rollback_compensation() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let marker = dir.path().join("rolled-back.marker");
    let script = format!(
        r#"
        transaction(|| {{
            on_rollback(|| {{ local_exec("touch " + sh_quote("{marker}")); }});
            print("READY");
            for i in 0..60 {{
                sleep(1);
            }}
        }});
        "#,
        marker = marker.display(),
    );
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(dir.path())
        .arg("exec")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for the script to actually be running (past parsing/engine setup and into the
    // transaction, right before it enters the sleep loop) rather than assuming a fixed delay is
    // enough — `print()` goes to stderr, so a "READY" line proves the interpreter reached that
    // point. Avoids a fixed-sleep race on a loaded CI runner where startup could take longer.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        line.clear();
        let n = stderr.read_line(&mut line).unwrap();
        assert!(n > 0, "child exited before printing READY (stderr closed early)");
        if line.contains("READY") {
            break;
        }
        assert!(Instant::now() < ready_deadline, "child never printed READY within 10s");
    }

    let pid = child.id().to_string();
    let sent = Command::new("kill").args(["-INT", &pid]).status().unwrap();
    assert!(sent.success(), "failed to send SIGINT to the child (pid {pid})");

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("nrg did not exit within 10s of SIGINT — interrupt handling is not working");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(!status.success(), "an interrupted run must exit non-zero, got {status:?}");
    assert!(
        marker.exists(),
        "the on_rollback compensation must have run (marker file missing) — SIGINT killed the \
         process without unwinding the transaction"
    );
}
