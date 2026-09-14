//! Real commands with an SSH-to-local-shell fixture; never connects to a deployment host.
use assert_cmd::Command;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

fn nrg(root: &Path) -> Command {
    let mut c = Command::cargo_bin("nrg").unwrap();
    c.current_dir(root).timeout(Duration::from_secs(20));
    c
}
fn script(root: &Path, source: &str) {
    fs::write(root.join("Energize.rhai"), source).unwrap();
}
fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn ssh(root: &Path) -> String {
    fs::create_dir(root.join("bin")).unwrap();
    fs::create_dir(root.join("tmp")).unwrap();
    executable(&root.join("bin/ssh"), "#!/bin/sh\nprintf '%s\\n' \"$@\" >> ssh-argv\nfor arg do cmd=$arg; done\nexec /bin/sh -c \"$cmd\"\n");
    format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap()
    )
}
fn kill_command_group(root: &Path) {
    if let Ok(pid) = fs::read_to_string(root.join("command-pid")) {
        unsafe {
            libc::kill(-pid.trim().parse::<i32>().unwrap(), libc::SIGTERM);
        }
    }
}
fn events(root: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(root.join(".energize/runs.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[test]
fn generated_generic_templates_can_call_functions_in_dry_run() {
    for template in [None, Some("release")] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let mut init = nrg(root);
        init.arg("init");
        if let Some(t) = template {
            init.args(["--template", t]);
        }
        init.assert().success();
        let mut run = nrg(root);
        run.args(["run", "deploy", "--dry-run"]);
        if template.is_some() {
            run.arg("v1");
        }
        run.assert()
            .success()
            .stdout(predicates::str::contains("execution-unverified"));
        assert!(!root.join(".energize").exists());
    }
}

#[test]
fn configured_local_and_remote_steps_preserve_input_and_hide_values() {
    for remote in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let path = ssh(root);
        fs::create_dir(root.join("work")).unwrap();
        executable(&root.join("work/check"), "#!/bin/sh\nprintf '%s' \"$TOKEN\" > token\ncat > input\nprintf '%s' \"$TOKEN\"\nprintf '%s' \"$TOKEN\" >&2\nexit 37\n");
        let call = if remote {
            "ssh_step(\"fixture\", "
        } else {
            "local_step("
        };
        script(
            root,
            &format!(
                r#"try {{ {call}"Validate input", "./check", #{{cwd: "work", env: #{{TOKEN: "quoted'private-value"}}, stdin: "private input\nwith binary\u0000tail", stream: true}}); }} catch(e) {{ throw "wrapper error"; }}"#
            ),
        );
        let out = nrg(root)
            .env("PATH", &path)
            .env("TMPDIR", root.join("tmp"))
            .arg("exec")
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read(root.join("work/token")).unwrap(),
            b"quoted'private-value"
        );
        assert_eq!(
            fs::read(root.join("work/input")).unwrap(),
            b"private input\nwith binary\0tail"
        );
        for content in [
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            fs::read_to_string(root.join(".energize/runs.jsonl")).unwrap(),
            fs::read_to_string(root.join(".energize/audit.log")).unwrap(),
            fs::read_to_string(root.join("ssh-argv")).unwrap_or_default(),
        ] {
            assert!(!content.contains("private-value"), "{content}");
            assert!(!content.contains("private input"), "{content}");
        }
        assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        nrg(root)
            .args(["audit", "Validate input"])
            .assert()
            .success()
            .stdout(predicates::str::contains("exited 37"));
        let ev = events(root);
        assert_eq!(ev[1]["event"], "step_started");
        assert!(ev[1]["step"]["exit_code"].is_null());
        assert_eq!(ev[2]["step"]["exit_code"], 37);
        let id = ev[0]["run_id"].as_str().unwrap();
        let json = nrg(root)
            .args(["audit", "--run", id, "--json"])
            .output()
            .unwrap();
        let read: Vec<serde_json::Value> = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(read.len(), 4);
    }
}

#[test]
fn retries_require_explicit_idempotence_and_stop_after_success() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    executable(&root.join("flaky"), "#!/bin/sh\ni=$(cat attempts 2>/dev/null || echo 0); i=$((i+1)); echo $i > attempts; test $i -ge 2\n");
    script(
        root,
        r#"local_step("retry", "./flaky", #{retries: 2, retry_delay_ms: 1});"#,
    );
    nrg(root)
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("idempotent: true"));
    assert!(!root.join("attempts").exists());
    script(
        root,
        r#"local_step("retry", "./flaky", #{retries: 2, retry_delay_ms: 1, idempotent: true});"#,
    );
    nrg(root).arg("exec").assert().success();
    assert_eq!(
        fs::read_to_string(root.join("attempts")).unwrap().trim(),
        "2"
    );
    assert_eq!(
        events(root)
            .iter()
            .filter(|e| e["event"] == "step_started")
            .count(),
        2
    );
}

#[test]
fn fifo_upload_rejects_promptly_without_contacting_ssh() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    assert!(std::process::Command::new("mkfifo")
        .arg(root.join("fifo"))
        .status()
        .unwrap()
        .success());
    script(root, r#"upload_file("fixture", "fifo", "dest", "0600");"#);
    nrg(root)
        .env("PATH", path)
        .timeout(Duration::from_secs(3))
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("regular file"));
    assert!(!root.join("ssh-argv").exists());
}

#[test]
fn configured_timeout_cleans_remote_temps_and_stops_children() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    script(
        root,
        r#"ssh_step("fixture", "deadline", "sleep 3; touch late", #{timeout_secs: 1, stream: false});"#,
    );
    nrg(root)
        .env("PATH", path)
        .env("TMPDIR", root.join("tmp"))
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("deadline exceeded"));
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    assert!(!root.join("late").exists());
}

#[test]
fn streaming_flushes_without_newline_and_interrupting_final_step_fails_and_rolls_back() {
    for legacy in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let operation = if legacy {
            "local_exec(\"echo $$ > command-pid; printf ready; sleep 20\")"
        } else {
            "local_step(\"wait\", \"echo $$ > command-pid; printf ready; sleep 20\")"
        };
        script(root, &format!("transaction(|| {{ on_rollback(|| local_exec(\"touch rollback\")); {operation}; }});"));
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
            .current_dir(root)
            .arg("exec")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        if legacy {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !root.join(".energize/runs.jsonl").exists()
                || !fs::read_to_string(root.join(".energize/runs.jsonl"))
                    .unwrap()
                    .contains("step_started")
            {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut b = [0; 5];
                let _ = stdout.read_exact(&mut b);
                let _ = tx.send(b);
            });
            let received = rx.recv_timeout(Duration::from_secs(5));
            if received.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                kill_command_group(root);
            }
            assert_eq!(received.unwrap(), *b"ready");
        }
        // Allow the legacy child to launch after its durable step-start event.
        if legacy {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(std::process::Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                kill_command_group(root);
                panic!("interrupt did not cancel the command");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        assert!(root.join("rollback").exists());
        assert_eq!(events(root).last().unwrap()["exit_code"], 1);
    }
}

#[test]
fn incomplete_journal_is_visible_and_torn_lines_do_not_hide_it() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    script(root, "");
    fs::create_dir(root.join(".energize")).unwrap();
    fs::write(root.join(".energize/runs.jsonl"), "{\"run_id\":\"crashed\",\"timestamp_ms\":1,\"event\":\"run_started\",\"step_id\":null,\"step\":null,\"exit_code\":null}\n{torn").unwrap();
    let out = nrg(root)
        .args(["audit", "--incomplete", "--json"])
        .output()
        .unwrap();
    let e: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(e.len(), 1);
    assert_eq!(e[0]["run_id"], "crashed");
    nrg(root).arg("exec").assert().success();
    let out = nrg(root)
        .args(["audit", "--incomplete", "--json"])
        .output()
        .unwrap();
    let pending: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "new completed run must remain readable after the torn line"
    );
    let content = fs::read_to_string(root.join(".energize/runs.jsonl")).unwrap();
    assert_eq!(
        content
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["event"] == "run_started")
            .count(),
        2
    );
}

#[test]
fn killed_process_leaves_a_durable_step_start_without_claiming_completion() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    script(
        root,
        r#"local_step("in flight", "echo $$ > command-pid; printf ready; sleep 20");"#,
    );
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(root)
        .arg("exec")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = [0; 5];
        let _ = stdout.read_exact(&mut b);
        let _ = tx.send(b);
    });
    let ready = rx.recv_timeout(Duration::from_secs(5));
    child.kill().unwrap();
    child.wait().unwrap();
    if let Ok(pid) = fs::read_to_string(root.join("command-pid")) {
        // Only the process group created by this test; never a deployment process.
        unsafe {
            libc::kill(-pid.trim().parse::<i32>().unwrap(), libc::SIGTERM);
        }
    }
    assert_eq!(ready.unwrap(), *b"ready");
    let ev = events(root);
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[1]["event"], "step_started");
    assert!(ev[1]["step"]["exit_code"].is_null());
    assert!(!root.join(".energize/audit.log").exists());
    nrg(root)
        .args(["audit", "--incomplete"])
        .assert()
        .success()
        .stdout(predicates::str::contains("in flight"));
}

#[test]
fn option_values_are_hidden_in_plans_and_short_values_stay_redacted_after_execution() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    script(
        root,
        r#"local_step("options", "printf '%s' \"$TOKEN\"; cat", #{ env: #{TOKEN: "tiny"}, stdin: "private-input" }); print("tiny");"#,
    );
    for args in [vec!["exec", "--dry-run"], vec!["exec"]] {
        let out = nrg(root).args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for content in [&out.stdout, &out.stderr] {
            let content = String::from_utf8_lossy(content);
            assert!(!content.contains("tiny"));
            assert!(!content.contains("private-input"));
        }
        if args.len() > 1 {
            assert!(!root.join(".energize").exists());
        }
    }
}

#[test]
fn configured_remote_command_keeps_shell_semantics() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    script(
        root,
        r#"ssh_step("fixture", "shell", "false; printf reached > reached; umask > received-umask", #{env: #{CUSTOM: "value"}});"#,
    );
    nrg(root).env("PATH", path).arg("exec").assert().success();
    assert_eq!(fs::read_to_string(root.join("reached")).unwrap(), "reached");
    let expected = std::process::Command::new("sh")
        .args(["-c", "umask"])
        .output()
        .unwrap();
    assert_eq!(
        fs::read(root.join("received-umask")).unwrap(),
        expected.stdout
    );
}

#[test]
fn status_distinguishes_missing_runtime_from_absent_container_and_reports_json() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    script(
        root,
        r#"state_set("app.version", "v1"); state_set("app.target.fixture", "localhost:8080"); state_set("app.runtime.cmd", "fixture-runtime");"#,
    );
    nrg(root).arg("exec").assert().success();
    for (body, state, success) in [
        ("echo 'runtime unavailable' >&2; exit 127", "failed", false),
        (
            "echo 'No such object: app-web' >&2; exit 1",
            "not_deployed",
            false,
        ),
        ("echo 'true|healthy'", "running", true),
        ("echo nonsense", "failed", false),
        ("echo true", "failed", false),
    ] {
        executable(
            &root.join("bin/fixture-runtime"),
            &format!("#!/bin/sh\n{body}\n"),
        );
        let out = nrg(root)
            .env("PATH", &path)
            .args(["status", "app", "--check", "--json"])
            .output()
            .unwrap();
        assert_eq!(out.status.success(), success);
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report[0]["hosts"][0]["probe"]["state"], state);
    }
}

#[test]
fn releases_preserve_previous_on_build_or_health_failure_and_allow_explicit_rollback() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    let app = root.join("app");
    let make = |version: &str, build: &str, health: &str, rollback: bool| {
        format!(
            r#"
        import "std/release" as release;
        release::{}("fixture", {}, "{version}", #{{
            prepare: "printf artifact > artifact", build: "{build}",
            activate: "readlink current >> activations", health: "{health}"
        }});
    "#,
            if rollback { "rollback" } else { "deploy" },
            serde_json::to_string(&app.to_string_lossy()).unwrap()
        )
    };
    for (version, build, health, ok, current) in [
        ("v1", "true", "test -f current/artifact", true, "v1"),
        ("v2", "exit 42", "true", false, "v1"),
        ("v3", "true", "test ! -f current/reject", false, "v1"),
        ("v4", "true", "true", true, "v4"),
    ] {
        let build = if version == "v3" {
            "touch reject"
        } else {
            build
        };
        script(root, &make(version, build, health, false));
        let out = nrg(root)
            .env("PATH", &path)
            .env("TMPDIR", root.join("tmp"))
            .arg("exec")
            .output()
            .unwrap();
        assert_eq!(
            out.status.success(),
            ok,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read_link(app.join("current")).unwrap(),
            app.join("releases").join(current)
        );
        assert!(!app.join(".nrg-release-lock").exists());
        assert!(!fs::read_dir(&app).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nrg-link.")));
    }
    script(root, &make("v1", "true", "true", true));
    nrg(root).env("PATH", &path).arg("exec").assert().success();
    assert_eq!(
        fs::read_link(app.join("current")).unwrap(),
        app.join("releases/v1")
    );
    script(root, &make("v1", "touch should-not-rebuild", "true", false));
    nrg(root).env("PATH", &path).arg("exec").assert().failure();
    assert!(!app.join("releases/v1/should-not-rebuild").exists());
}

#[test]
fn framework_recipes_are_optional_overridable_maps() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    script(
        root,
        r#"
        import "std/release_recipes" as recipes;
        let all = [recipes::rails(#{}), recipes::django(#{}), recipes::nextjs(#{}), recipes::phoenix(#{}), recipes::laravel(#{})];
        for cfg in all { if cfg.build == "" || cfg.contains("activate") || cfg.contains("migrate") { throw "unexpected policy in recipe"; } }
        let cfg = recipes::phoenix(#{build: "custom build", env: #{CUSTOM: "value"}, prepare: "artifact", activate: "supervisor", health: "healthcheck"});
        if cfg.build != "custom build" || cfg.env.MIX_ENV != "prod" || cfg.env.CUSTOM != "value" { throw "override lost"; }
    "#,
    );
    nrg(root).arg("exec").assert().success();
}

#[test]
fn release_rejects_unsafe_paths_before_work_and_deactivates_a_failed_first_release() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    for (base, version) in [
        ("/", "v1"),
        ("/srv/app/../other", "v1"),
        ("/srv/app", "../escape"),
    ] {
        script(
            root,
            &format!(
                r#"import "std/release" as r; r::deploy("fixture", "{base}", "{version}", #{{prepare: "true", activate: "true", health: "true"}});"#
            ),
        );
        nrg(root).env("PATH", &path).arg("exec").assert().failure();
        assert!(!root.join("ssh-argv").exists());
    }
    let app = serde_json::to_string(&root.join("app").to_string_lossy()).unwrap();
    script(
        root,
        &format!(
            r#"import "std/release" as r; r::deploy("fixture", {app}, "v1", #{{prepare: "true", activate: "touch active", health: "false", deactivate: "rm active"}});"#
        ),
    );
    nrg(root).env("PATH", &path).arg("exec").assert().failure();
    assert!(root.join("app/current").symlink_metadata().is_err());
    assert!(!root.join("app/active").exists());
    assert!(!root.join("app/.nrg-release-lock").exists());
}

#[test]
fn invalid_step_options_fail_before_any_command_and_journal_corrects_permissions() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for options in [
        "#{timeout_secs: 0}",
        "#{retries: 11, idempotent: true}",
        "#{retry_delay_ms: 60001}",
        "#{env: #{\"BAD-NAME\": \"value\"}}",
        "#{unknown: true}",
    ] {
        script(
            root,
            &format!("local_step(\"invalid\", \"touch mutation\", {options});"),
        );
        nrg(root).arg("exec").assert().failure();
        assert!(!root.join("mutation").exists());
    }
    let journal = root.join(".energize/runs.jsonl");
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o666)).unwrap();
    script(root, "");
    nrg(root).arg("exec").assert().success();
    assert_eq!(
        fs::metadata(journal).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn interrupt_during_lock_acquisition_still_cleans_the_owned_lock() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let path = ssh(root);
    executable(&root.join("bin/ssh"), "#!/bin/sh\nfor arg do cmd=$arg; done\ncase \"$cmd\" in 'umask 077; mkdir '*) /bin/sh -c \"$cmd\" || exit; touch acquired; sleep 20;; *) exec /bin/sh -c \"$cmd\";; esac\n");
    script(root, r#"remote_lock_acquire("fixture", "owned-lock");"#);
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(root)
        .env("PATH", path)
        .arg("exec")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("acquired").exists() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fixture did not acquire the lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            assert!(!s.success());
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("interrupted lock acquisition did not return");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!root.join("owned-lock").exists());
}
