//! Contract orchestration with actual local shells, files, signals and loopback HTTP.
use assert_cmd::Command;
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};

fn nrg(root: &Path) -> Command {
    let mut c = Command::cargo_bin("nrg").unwrap();
    c.current_dir(root).timeout(Duration::from_secs(30));
    c
}
fn write(root: &Path, source: &str) {
    fs::write(root.join("Rehearsal.rhai"), source).unwrap();
}
fn contract(action: &str, check: &str, cleanup: &str) -> String {
    format!(
        r#"import "std/contracts" as c;
        #{{ name: "fixture", scenarios: [c::scenario("install", [c::step("install", {action})], [c::step("verify", {check})])], cleanup: [c::step("cleanup", {cleanup})] }}"#,
        action = json!(action),
        check = json!(check),
        cleanup = json!(cleanup)
    )
}
fn report(root: &Path, live: bool, faults: bool) -> (bool, Value) {
    let mut c = nrg(root);
    c.args(["rehearse", "--json"]);
    if live {
        c.arg("--execute");
    }
    if faults {
        c.arg("--faults");
    }
    let out = c.output().unwrap();
    let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.success(), value)
}
fn removed(value: &Value) {
    assert_eq!(value["workspace_removed"], true, "{value}");
    assert!(!Path::new(value["workspace"].as_str().unwrap()).exists());
}

#[test]
fn default_and_explicit_dry_runs_have_no_commands_workspace_locks_or_state() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        &contract("touch forbidden", "false", "touch cleanup-forbidden"),
    );
    for flag in [None, Some("--dry-run")] {
        let mut c = nrg(d.path());
        c.args(["rehearse", "--json"]);
        c.env("TMPDIR", d.path().join("must-not-be-created"));
        if let Some(flag) = flag {
            c.arg(flag);
        }
        let out = c.assert().success().get_output().clone();
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["status"], "execution-unverified");
        assert!(value["workspace"].is_null());
        for step in value["steps"].as_array().unwrap() {
            assert!(step["exit_code"].is_null());
            assert_eq!(step["status"], "planned; execution-unverified");
        }
        assert!(!String::from_utf8_lossy(&out.stdout).contains("touch"));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }
}

#[test]
fn declaration_engine_rejects_execution_file_imports_and_unbounded_loops() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("other.rhai"), "fn exists() { true }").unwrap();
    for source in [
        "local_exec(\"touch forbidden\"); #{}",
        "ssh_exec(\"production\", \"true\"); #{}",
        "import \"./other\" as m; #{}",
        "loop { }",
    ] {
        write(d.path(), source);
        nrg(d.path())
            .args(["rehearse", "--execute"])
            .assert()
            .code(2)
            .stderr(predicates::str::contains("contract evaluation failed"));
        assert!(!d.path().join("forbidden").exists());
        assert!(!d.path().join(".energize").exists());
    }
}

#[test]
fn validates_whole_contract_before_starting_any_setup() {
    let d = tempfile::tempdir().unwrap();
    let base = contract("true", "true", "true").replace(
        "scenarios:",
        r#"setup: [c::step("setup", "touch \"$NRG_SOURCE/forbidden\"")], scenarios:"#,
    );
    for source in [
        base.replace("name: \"fixture\"", "name: \"fixture\", unknown: true"),
        base.replace("scenarios:", "env: #{ HOME: \"/production\" }, scenarios:"),
        base.replace(
            "c::step(\"verify\", \"true\")",
            "#{ name: \"verify\", run: \"true\", expect_exit: 1 }",
        ),
        base.replace(
            "c::step(\"install\", \"true\")",
            "#{ name: \"install\", run: \"true\", timeout_secs: 0 }",
        ),
        base.replace("[c::step(\"verify\", \"true\")]", "[]"),
        base.replace(
            "c::step(\"cleanup\", \"true\")",
            "#{ name: \"cleanup\", run: \"true\", expect_exit: 1 }",
        ),
    ] {
        write(d.path(), &source);
        assert_ne!(source, base, "test mutation must change the declaration");
        nrg(d.path())
            .args(["rehearse", "--execute"])
            .assert()
            .code(2);
        assert!(!d.path().join(".energize").exists());
        assert!(!d.path().join("forbidden").exists());
    }
    write(d.path(), &base);
    nrg(d.path())
        .args(["rehearse", "--dest", "production", "--execute"])
        .assert()
        .code(2);
    nrg(d.path())
        .args(["rehearse", "--execute", "--dry-run"])
        .assert()
        .code(2);
}

#[test]
fn service_helper_runs_install_repeat_restart_and_observable_checks_in_order() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        r#"import "std/contracts" as c;
        c::service(#{ name: "service", setup: [c::step("setup", "printf s > order")],
          deploy: c::step("deploy", "printf d >> order"), restart: c::step("restart", "printf r >> order"),
          checks: [c::step("check", "printf c >> order")],
          cleanup: [c::step("assert order", "test \"$(cat order)\" = sdcdcrc")] })"#,
    );
    for _ in 0..2 {
        let (ok, value) = report(d.path(), true, false);
        assert!(ok, "{value}");
        assert_eq!(value["status"], "passed");
        assert_eq!(value["steps"].as_array().unwrap().len(), 8);
        removed(&value);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }
}

#[test]
fn faults_are_opt_in_and_require_exact_exit_plus_successful_postconditions() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        r#"import "std/contracts" as c;
      #{ name: "failure", scenarios: [
        c::scenario("install", [c::step("deploy", "echo original > current")], [c::step("exists", "test -f current")]),
        c::rejects("rejected upgrade", "touch attempted; exit 42", 42, [c::step("old release", "test \"$(cat current)\" = original")])
      ], cleanup: [c::step("stop", "true")] }"#,
    );
    let (ok, skipped) = report(d.path(), true, false);
    assert!(ok);
    assert_eq!(skipped["steps"][2]["status"], "skipped");
    assert!(skipped["steps"][2]["exit_code"].is_null());
    removed(&skipped);
    let (ok, passed) = report(d.path(), true, true);
    assert!(ok, "{passed}");
    assert_eq!(passed["steps"][2]["exit_code"], 42);
    assert_eq!(passed["steps"][3]["status"], "passed");
    removed(&passed);
    let original = fs::read_to_string(d.path().join("Rehearsal.rhai")).unwrap();
    for action in [
        "true",
        "exit 11",
        "nrg_missing_test_tool",
        "echo broken > current; exit 42",
    ] {
        write(
            d.path(),
            &original.replace("touch attempted; exit 42", action),
        );
        let (ok, failed) = report(d.path(), true, true);
        assert!(!ok, "{failed}");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["steps"][4]["status"], "passed", "cleanup must run");
        removed(&failed);
    }
}

#[test]
fn failure_skips_later_work_but_attempts_every_cleanup_step() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        r#"import "std/contracts" as c;
      #{ name: "failed setup", setup: [c::step("setup", "exit 7")],
         scenarios: [c::scenario("never", [c::step("action", "touch forbidden")], [c::step("check", "true")])],
         cleanup: [c::step("failed cleanup", "exit 8"), c::step("remaining cleanup", "test ! -e forbidden")] }"#,
    );
    let (ok, value) = report(d.path(), true, true);
    assert!(!ok);
    assert_eq!(value["steps"][0]["exit_code"], 7);
    assert_eq!(value["steps"][1]["status"], "skipped");
    assert_eq!(value["steps"][3]["exit_code"], 8);
    assert_eq!(value["steps"][4]["status"], "passed");
    removed(&value);
}

#[test]
fn timeout_is_failure_and_cannot_satisfy_an_expected_fault() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        r#"import "std/contracts" as c;
      #{ name: "timeout", scenarios: [c::fault("timeout", [#{ name: "blocked", run: "sleep 30", timeout_secs: 1, expect_exit: 42 }],
        [c::step("not reached", "true")])], cleanup: [c::step("stop", "true")] }"#,
    );
    let (ok, value) = report(d.path(), true, true);
    assert!(!ok);
    assert_eq!(value["steps"][0]["exit_code"], -1);
    assert!(value["steps"][0]["excerpt"]
        .as_str()
        .unwrap()
        .contains("deadline exceeded"));
    assert_eq!(value["steps"][1]["status"], "skipped");
    removed(&value);
}

#[test]
fn missing_selection_and_invalid_fault_expectations_fail_before_execution() {
    let d = tempfile::tempdir().unwrap();
    let source = r#"import "std/contracts" as c;
        #{ name: "fault only", scenarios: [c::rejects("reject", "exit 42", 42, [c::step("check", "true")])],
           cleanup: [c::step("cleanup", "true")] }"#;
    write(d.path(), source);
    nrg(d.path())
        .args(["rehearse", "--execute"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no scenarios selected"));
    for expected in ["0", "126", "127", "130", "-1"] {
        write(
            d.path(),
            &source.replace(", 42,", &format!(", {expected},")),
        );
        nrg(d.path())
            .args(["rehearse", "--execute", "--faults"])
            .assert()
            .code(2);
    }
    write(
        d.path(),
        &source.replace("cleanup:", "private_declaration_value:"),
    );
    let out = nrg(d.path())
        .args(["rehearse"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid contract schema"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private_declaration_value"));
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
}

#[test]
fn large_concurrent_streams_have_bounded_redacted_failure_evidence() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("loud.py"),
        r#"import os,sys,threading
def emit(stream):
    for _ in range(256): stream.write('x' * 4096)
    stream.write(os.environ['TOKEN'] + ' end'); stream.flush()
t = threading.Thread(target=emit, args=(sys.stdout,)); t.start()
emit(sys.stderr); t.join()
sys.exit(15)
"#,
    )
    .unwrap();
    write(
        d.path(),
        &contract("python3 \"$NRG_SOURCE/loud.py\"", "true", "true"),
    );
    let out = nrg(d.path())
        .args(["rehearse", "--execute", "--json", "--env", "TOKEN"])
        .env("TOKEN", "unique-private-value")
        .assert()
        .code(1)
        .get_output()
        .clone();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["steps"][0]["exit_code"], 15);
    let excerpt = value["steps"][0]["excerpt"].as_str().unwrap();
    assert!(excerpt.chars().count() <= 2048);
    assert!(excerpt.contains("*** end"));
    assert!(out.stdout.len() < 5000);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("unique-private-value"));
    removed(&value);
}

#[test]
fn local_environment_is_fresh_and_declared_secrets_are_redacted_across_chunks() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("emit.py"),
        r#"import os,sys,time
assert 'AWS_SECRET_ACCESS_KEY' not in os.environ
assert 'SSH_AUTH_SOCK' not in os.environ
assert 'NRG_SECRET_PASSWORD' not in os.environ
assert os.environ['HOME'].startswith(os.environ['NRG_WORKSPACE'] + '/')
assert os.environ['TMPDIR'].startswith(os.environ['NRG_WORKSPACE'] + '/')
secret = os.environ['TOKEN']
assert sys.stdin.read() == 'stdin-only-value'
for pipe in [sys.stdout, sys.stderr]:
    pipe.write('prefix ' + secret[:7]); pipe.flush(); time.sleep(.03)
    pipe.write(secret[7:] + ' suffix\n'); pipe.flush()
sys.exit(12)
"#,
    )
    .unwrap();
    write(
        d.path(),
        r#"import "std/contracts" as c;
      #{ name: "secrets", scenarios: [c::scenario("emit", [#{ name: "emit", run: "python3 \"$NRG_SOURCE/emit.py\"", stdin: "stdin-only-value" }],
        [c::step("not reached", "true")])], cleanup: [c::step("cleanup", "true")] }"#,
    );
    for json_mode in [false, true] {
        let mut c = nrg(d.path());
        c.args(["rehearse", "--execute", "--env", "TOKEN"])
            .env("TOKEN", "registered-private-token")
            .env("AWS_SECRET_ACCESS_KEY", "never-inherit")
            .env("SSH_AUTH_SOCK", "never-inherit")
            .env("NRG_SECRET_PASSWORD", "never-inherit");
        if json_mode {
            c.arg("--json");
        }
        let out = c.assert().code(1).get_output().clone();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for hidden in [
            "registered-private-token",
            "registered",
            "stdin-only-value",
            "never-inherit",
        ] {
            assert!(!combined.contains(hidden), "{combined}");
        }
        assert!(combined.contains("prefix *** suffix"), "{combined}");
        if json_mode {
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["steps"][0]["exit_code"], 12);
            removed(&value);
        }
    }
}

fn run_http_recipe(directory: &Path, block_reverse_dns: bool) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/rehearsal");
    let mut command = nrg(directory);
    command
        .arg("rehearse")
        .arg(root.join("Rehearsal.rhai"))
        .args(["--execute", "--faults", "--json"]);
    if block_reverse_dns {
        fs::write(directory.join("sitecustomize.py"), "import socket\ndef no_reverse_dns(*args, **kwargs):\n    raise RuntimeError('unexpected reverse-DNS lookup')\nsocket.getfqdn = no_reverse_dns\n").unwrap();
        command
            .args(["--env", "PYTHONPATH"])
            .env("PYTHONPATH", directory);
    }
    let out = command.output().unwrap();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "invalid report: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let failures: Vec<_> = value["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["status"] == "failed")
        .collect();
    assert!(
        out.status.success(),
        "{}\nfailed steps: {}",
        String::from_utf8_lossy(&out.stderr),
        serde_json::to_string_pretty(&failures).unwrap()
    );
    assert!(
        value["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "passed"),
        "{value}"
    );
    assert!(value["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["exit_code"] == 42));
    assert!(value["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["exit_code"] == 43));
    removed(&value);
}

#[test]
fn real_http_recipe_exercises_restart_unhealthy_rollback_interrupted_upload_and_kill() {
    let d = tempfile::tempdir().unwrap();
    run_http_recipe(d.path(), false);
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
}

#[test]
fn real_http_recipe_does_not_require_reverse_dns() {
    let d = tempfile::tempdir().unwrap();
    run_http_recipe(d.path(), true);
}

#[test]
fn ctrl_c_cancels_work_runs_cleanup_and_does_not_pass_as_a_fault() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        &contract(
            "echo ready > \"$NRG_SOURCE/ready\"; sleep 30",
            "true",
            "echo cleaned > \"$NRG_SOURCE/cleaned\"",
        ),
    );
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_nrg"))
        .current_dir(d.path())
        .args(["rehearse", "--execute", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !d.path().join("ready").exists() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!("never ready: {}", String::from_utf8_lossy(&out.stderr));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("interrupt did not complete");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130));
    assert_eq!(
        fs::read_to_string(d.path().join("cleaned")).unwrap(),
        "cleaned\n"
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["status"], "interrupted");
    assert_eq!(value["steps"][1]["status"], "skipped");
    assert_eq!(value["steps"][2]["status"], "passed");
    removed(&value);
}
