//! Characterization of the mutating builtins' dry-run plan text and NRG_TRACE output, pinned
//! end-to-end so the central dispatch helper can be refactored without behaviour drift.

use assert_cmd::Command;
use std::fs;

const SCRIPT: &str = r#"
    ssh_exec("h1", "c1");
    local_exec("c2");
    ssh_exec_stdin("h2", "c3", "stdinbody");
    local_exec_stdin("c4", "stdinbody");
    ssh_exec_all(["a", "b"], "c5");
    write_remote("h3", "WRITEBODY", "/etc/a");
    write_remote("h3", "WRITEBODY", "/etc/b", "0644");
    upload_file("h4", "/src", "/dst", "0640");
    download_file("h5", "/rsrc", "/ldst", "0600");
    remote_lock_acquire("h6", "/var/lock/x");
    remote_lock_release("h6", "/var/lock/x");
    http_post("http://127.0.0.1:1/p", "{}", #{"X": "1"});
    state_set("k", "value");
    state_update(#{"b": "12"});
    state_del("k");
    on_rollback(|| {});
    local_step("build", "make", #{retries: 1, idempotent: true});
    ssh_step("h7", "deploy", "ls");
"#;

fn run() -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Energize.rhai"), SCRIPT).unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_TRACE", "1")
        .args(["exec", "--dry-run", "Energize.rhai"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn dry_run_plan_text_of_every_mutating_builtin_is_stable() {
    let (stdout, _) = run();
    for line in [
        "c1",
        "c2",
        "c3",
        "c4",
        "c5",
        "write 9 bytes -> /etc/a",
        "write 9 bytes -> /etc/b (mode 0644)",
        "/src -> /dst mode 640",
        "/rsrc -> /ldst mode 600",
        "mkdir /var/lock/x (owned deploy lock)",
        "rmdir /var/lock/x (owned deploy lock)",
        "[assumed ok] POST http://127.0.0.1:1/p (1 header(s))",
        "k = <5 bytes>",
        "b = <2 bytes> (atomic batch)",
        "del k",
        "register compensation",
        "build: make (cwd=inherited, timeout=",
        "deploy: ls (cwd=inherited, timeout=",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    }
    // Order is the call order.
    let pos = |needle: &str| stdout.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let order = [
        "c1", "c2", "c3", "c4", "c5", "/etc/a", "/src ->", "/rsrc ->", "mkdir", "rmdir",
    ];
    let idx: Vec<_> = order.iter().map(|n| pos(n)).collect();
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "{idx:?}\n{stdout}");
    assert!(stdout.contains("0 executed."), "{stdout}");
}

#[test]
fn trace_lines_never_leak_bodies_and_transfers_stay_untraced() {
    let (_, stderr) = run();
    for line in [
        "[nrg] ssh_exec h1 -> c1",
        "[nrg] local_exec -> c2",
        "[nrg] ssh_exec_stdin h2 (stdin 9 bytes) -> c3",
        "[nrg] local_exec_stdin (stdin 9 bytes) -> c4",
        "[nrg] ssh_exec_all -> c5",
        "[nrg] write_remote h3 -> /etc/a (9 bytes)",
        "[nrg] write_remote h3 -> /etc/b (9 bytes)",
    ] {
        assert!(stderr.contains(line), "missing {line:?} in:\n{stderr}");
    }
    assert!(
        !stderr.contains("WRITEBODY") && !stderr.contains("stdinbody"),
        "{stderr}"
    );
    // upload_file/download_file/locks/http/state emit no trace line.
    for absent in [
        "upload_file",
        "download_file",
        "/src",
        "/rsrc",
        "mkdir",
        "POST",
    ] {
        assert!(
            !stderr.contains(absent),
            "unexpected {absent:?} in:\n{stderr}"
        );
    }
    assert_eq!(
        stderr.matches("[nrg] ").count(),
        7,
        "no step trace line in dry-run, no extra lines:\n{stderr}"
    );
}
