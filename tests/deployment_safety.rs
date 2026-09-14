//! Real shell/filesystem tests. The SSH transport is replaced by a local shell only;
//! no test connects to a host. rsync tests below execute the actual installed binary.
use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::Path};

fn script(dir: &Path, text: &str) {
    fs::write(dir.join("Energize.rhai"), text).unwrap();
}
fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn local_ssh(dir: &Path) -> String {
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("ssh"),
        "#!/bin/sh\nfor arg do cmd=$arg; done\nexec /bin/sh -c \"$cmd\"\n",
    );
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}
fn nrg(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("nrg").unwrap();
    c.current_dir(dir);
    c
}
fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}
fn checks(dir: &Path, value: serde_json::Value) {
    fs::write(dir.join("checks.json"), serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn binary_transfers_replace_broad_destinations_and_preserve_bytes() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    let bytes: Vec<_> = (0..200_000).map(|i| (i % 256) as u8).collect();
    fs::write(p.join("source"), &bytes).unwrap();
    script(
        p,
        r#"upload_file("test", "source", "remote", "0600"); download_file("test", "remote", "download", "0640");"#,
    );
    for existing in [false, true] {
        if existing {
            for name in ["remote", "download"] {
                fs::write(p.join(name), "old").unwrap();
                fs::set_permissions(p.join(name), fs::Permissions::from_mode(0o777)).unwrap();
            }
        }
        nrg(p).env("PATH", &path).arg("exec").assert().success();
        assert_eq!(fs::read(p.join("remote")).unwrap(), bytes);
        assert_eq!(fs::read(p.join("download")).unwrap(), bytes);
        assert_eq!(mode(&p.join("remote")), 0o600);
        assert_eq!(mode(&p.join("download")), 0o640);
    }
}

#[test]
fn write_remote_corrects_existing_mode_and_failed_chmod_cleans_up() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    fs::write(p.join("dest"), "old").unwrap();
    fs::set_permissions(p.join("dest"), fs::Permissions::from_mode(0o666)).unwrap();
    script(
        p,
        r#"let r = write_remote("test", "private-body", "dest"); if !r.ok { throw "write failed"; }"#,
    );
    nrg(p).env("PATH", &path).arg("exec").assert().success();
    assert_eq!(mode(&p.join("dest")), 0o600);
    assert_eq!(fs::read_to_string(p.join("dest")).unwrap(), "private-body");
    executable(&p.join("bin/chmod"), "#!/bin/sh\nexit 47\n");
    fs::write(p.join("dest"), "preserve").unwrap();
    let out = nrg(p).env("PATH", &path).arg("exec").output().unwrap();
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(p.join("dest")).unwrap(), "preserve");
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("dest.")));
    assert!(!fs::read_to_string(p.join(".energize/audit.log"))
        .unwrap()
        .contains("private-body"));
}

#[test]
fn failed_download_keeps_original_and_removes_temp() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    fs::write(p.join("dest"), "preserve").unwrap();
    executable(
        &p.join("bin/ssh"),
        "#!/bin/sh\nprintf 'partial-secret-body'\nprintf 'partial-secret-body' >&2\nexit 42\n",
    );
    script(p, r#"download_file("test", "remote", "dest", "0600");"#);
    let out = nrg(p).env("PATH", &path).arg("exec").output().unwrap();
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(p.join("dest")).unwrap(), "preserve");
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".tmp")));
    for text in [
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        fs::read_to_string(p.join(".energize/audit.log")).unwrap(),
    ] {
        assert!(!text.contains("partial-secret"));
    }
}

#[test]
fn dry_run_does_not_read_files_run_steps_probe_writes_or_create_state() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    script(
        p,
        r#"
        upload_file("invalid", "missing", "/missing", "0600");
        download_file("invalid", "/missing", "missing", "0600");
        local_step("unverified", "touch mutation");
        remote_lock_acquire("invalid", "/lock");
        preflight([#{ name: "permissions", kind: "file-permissions" }], true);
        state_set("sample", "value");
    "#,
    );
    let out = nrg(p).args(["exec", "--dry-run"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("planned; execution-unverified"));
    assert!(!p.join(".energize").exists());
    assert!(!p.join("mutation").exists());
}

#[test]
fn named_streams_redact_split_secrets_and_audit_survives_wrapper_error() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    // Enough output on both pipes to deadlock a serial reader. Separate writes split each secret.
    executable(&p.join("output.sh"), "#!/bin/sh\ni=0; while [ $i -lt 5000 ]; do printf 'stdout padding\\n'; printf 'stderr padding\\n' >&2; i=$((i+1)); done\nprintf 'ultra'; sleep 0.1; printf 'secret-value\\n'; printf 'ultra' >&2; sleep 0.1; printf 'secret-value\\n' >&2; printf 'erl: not found\\n' >&2; exit 23\n");
    script(
        p,
        r#"let s = secret("TOKEN"); try { local_step("Install Elixir", "./output.sh"); } catch (e) { throw "generic wrapper error"; }"#,
    );
    let out = nrg(p)
        .env("NRG_SECRET_TOKEN", "ultrasecret-value")
        .env("NRG_COMMAND_TIMEOUT_SECS", "10")
        .arg("exec")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let audit = fs::read_to_string(p.join(".energize/audit.log")).unwrap();
    for text in [
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        audit.clone(),
    ] {
        assert!(!text.contains("ultrasecret-value"));
    }
    assert!(String::from_utf8_lossy(&out.stdout).len() > 60_000);
    let entry: serde_json::Value = serde_json::from_str(audit.trim()).unwrap();
    assert!(entry["outcome"]
        .as_str()
        .unwrap()
        .contains("generic wrapper"));
    assert_eq!(entry["steps"][0]["name"], "Install Elixir");
    assert_eq!(entry["steps"][0]["exit_code"], 23);
    assert!(entry["steps"][0]["excerpt"]
        .as_str()
        .unwrap()
        .contains("erl: not found"));
    assert!(entry["steps"][0]["excerpt"].as_str().unwrap().len() <= 2048);
}

#[test]
fn preflight_distinguishes_syntax_capability_and_temporary_writes() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    script(p, "fn deploy() {}");
    checks(
        p,
        serde_json::json!([{"name":"syntax", "kind":"syntax", "command":"touch must-not-exist"}]),
    );
    nrg(p)
        .args(["doctor", "--checks", "checks.json"])
        .assert()
        .success();
    assert!(!p.join("must-not-exist").exists());
    checks(
        p,
        serde_json::json!([{"name":"unsupported flag", "kind":"read-only", "command":"/bin/sh --not-a-real-option"}]),
    );
    nrg(p)
        .args(["doctor", "--checks", "checks.json"])
        .assert()
        .failure();
    checks(
        p,
        serde_json::json!([{"name":"mode", "kind":"file-permissions"}]),
    );
    nrg(p)
        .args(["doctor", "--checks", "checks.json"])
        .assert()
        .failure();
    nrg(p)
        .args(["doctor", "--checks", "checks.json", "--allow-temporary"])
        .assert()
        .success();
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".nrg-check")));
    executable(&p.join("failing-rsync"), "#!/bin/sh\nexit 42\n");
    checks(
        p,
        serde_json::json!([{"name":"failure cleanup", "kind":"rsync-permissions", "tool":"./failing-rsync"}]),
    );
    nrg(p)
        .args(["doctor", "--checks", "checks.json", "--allow-temporary"])
        .assert()
        .failure();
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".nrg-check")));

    assert!(!p.join(".energize").exists());
}

fn real_rsync(path: &str, expected: &str) {
    let required = std::env::var_os("CI").is_some()
        && (expected == "rsync  version 3" || cfg!(target_os = "macos"));
    let version = std::process::Command::new(path).arg("--version").output();
    let Ok(version) = version else {
        assert!(
            !required,
            "required platform tool missing: {expected} at {path}"
        );
        eprintln!("UNAVAILABLE: {expected} at {path}");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout);
    if !version.contains(expected) {
        assert!(
            !required,
            "required platform tool mismatch: {path}: {version}"
        );
        eprintln!("UNAVAILABLE: {path} is not {expected}: {version}");
        return;
    }
    eprintln!("REAL TOOL: {}", version.lines().next().unwrap());
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    script(p, "fn deploy() {}");
    checks(
        p,
        serde_json::json!([{"name":"real rsync new and existing mode", "kind":"rsync-permissions", "tool":path}]),
    );
    nrg(p)
        .args(["doctor", "--checks", "checks.json", "--allow-temporary"])
        .assert()
        .success();
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".nrg-check")));
}
#[test]
fn real_openrsync_permissions() {
    real_rsync("/usr/bin/rsync", "openrsync");
}
#[test]
fn real_gnu_rsync_permissions() {
    real_rsync(
        &std::env::var("NRG_TEST_GNU_RSYNC").unwrap_or_else(|_| {
            if cfg!(target_os = "macos") {
                "/opt/homebrew/bin/rsync".into()
            } else {
                "/usr/bin/rsync".into()
            }
        }),
        "rsync  version 3",
    );
}

#[test]
fn legacy_exec_failure_details_survive_generic_wrapper() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    script(
        p,
        r#"let s = secret("TOKEN"); let r = local_exec("printf 'erl: not found ' >&2; printf '%s' \"$NRG_SECRET_TOKEN\" >&2; exit 19"); if !r.ok { throw "wrapper failed"; }"#,
    );
    nrg(p)
        .env("NRG_SECRET_TOKEN", "private-secret")
        .arg("exec")
        .assert()
        .failure();
    let text = fs::read_to_string(p.join(".energize/audit.log")).unwrap();
    assert!(text.contains("erl: not found"));
    assert!(!text.contains("private-secret"));
    assert!(text.contains("\"exit_code\":19"));
}

#[test]
fn transfer_rejects_symlinks_and_invalid_modes() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    fs::write(p.join("original"), "old").unwrap();
    fs::write(p.join("source"), "new").unwrap();
    std::os::unix::fs::symlink("original", p.join("link")).unwrap();
    for call in [
        r#"upload_file("test", "source", "link", "0600");"#,
        r#"download_file("test", "source", "link", "0600");"#,
        r#"upload_file("test", "source", "original", "u=rw,go=");"#,
    ] {
        script(p, call);
        nrg(p).env("PATH", &path).arg("exec").assert().failure();
        assert_eq!(fs::read_to_string(p.join("original")).unwrap(), "old");
    }
}

#[test]
fn mise_recipe_installer_fixture_tests_persisted_order_and_repeat() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    // A tool-specific installer fixture, not a shell-semantics simulator. Elixir refuses to
    // install unless the preceding invocation registered Erlang in the actual config file.
    executable(
        &p.join("mise-fixture"),
        r#"#!/bin/sh
set -eu
case "$1" in
use)
    test "$2" = --pin; test "$3" = --path; config=$4; tool=$5
    case "$tool" in
      erlang@*)
        test "${FAIL_ERLANG:-0}" = 0 || exit 31
        if ! test -f "$config"; then printf '[tools]\nerlang = "28.2"\n' > "$config"; fi ;;
      elixir@*)
        grep -q erlang "$config" || { echo 'erl: not found' >&2; exit 32; }
        test "${FAIL_ELIXIR:-0}" = 0 || { echo 'installation failed' >&2; exit 33; }
        grep -q elixir "$config" || printf 'elixir = "1.19.4-otp-28"\n' >> "$config" ;;
    esac ;;
exec) test -f mise.toml ;;
*) exit 90 ;;
esac
"#,
    );
    script(
        p,
        &format!(
            r#"import "std/mise" as mise; mise::provision("test", {}, #{{mise:{}, erlang:"28.2", elixir:"1.19.4-otp-28"}});"#,
            serde_json::to_string(&p.to_str().unwrap()).unwrap(),
            serde_json::to_string(&p.join("mise-fixture").to_str().unwrap()).unwrap()
        ),
    );
    nrg(p).env("PATH", &path).arg("exec").assert().success();
    let first = fs::read(p.join("mise.toml")).unwrap();
    nrg(p).env("PATH", &path).arg("exec").assert().success();
    assert_eq!(fs::read(p.join("mise.toml")).unwrap(), first);
    for flag in ["FAIL_ERLANG", "FAIL_ELIXIR"] {
        nrg(p)
            .env("PATH", &path)
            .env(flag, "1")
            .arg("exec")
            .assert()
            .failure();
    }
}

#[test]
fn real_mise_verifies_preinstalled_toolchain_without_activation() {
    let (Ok(mise), Ok(erlang), Ok(elixir)) = (
        std::env::var("NRG_TEST_MISE"),
        std::env::var("NRG_TEST_ERLANG"),
        std::env::var("NRG_TEST_ELIXIR"),
    ) else {
        eprintln!("UNAVAILABLE: real mise verification requires NRG_TEST_MISE, NRG_TEST_ERLANG, NRG_TEST_ELIXIR (preinstalled versions only)");
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    script(
        p,
        &format!(
            r#"import "std/mise" as mise; let cfg = #{{mise:{}, erlang:{}, elixir:{}}}; preflight(mise::checks("test", cfg), false); mise::verify("test", {}, cfg);"#,
            serde_json::to_string(&mise).unwrap(),
            serde_json::to_string(&erlang).unwrap(),
            serde_json::to_string(&elixir).unwrap(),
            serde_json::to_string(p.to_str().unwrap()).unwrap()
        ),
    );
    nrg(p)
        .env("PATH", &path)
        .env("MISE_CACHE_DIR", p.join("cache"))
        .env("MISE_STATE_DIR", p.join("mise-state"))
        .env("MISE_CONFIG_DIR", p.join("config"))
        .env("MISE_GLOBAL_CONFIG_FILE", p.join("global.toml"))
        .arg("exec")
        .assert()
        .success();
    assert!(!p.join("mise.toml").exists());
    assert!(!p.join("global.toml").exists());
}

#[test]
fn streaming_is_visible_before_command_finishes() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    // The child cannot finish until the test has observed its output and opened the gate.
    script(
        p,
        r#"local_step("stream gate", "printf 'ready\\n'; i=0; while [ ! -f gate ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; test -f gate");"#,
    );
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(p)
        .arg("exec")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = send.send(line);
    });
    let received = recv.recv_timeout(Duration::from_secs(5));
    fs::write(p.join("gate"), "go").unwrap();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert_eq!(received.unwrap(), "ready\n");
    assert!(status.success());
}

#[test]
fn doctor_does_not_require_unused_tools() {
    let d = tempfile::tempdir().unwrap();
    script(d.path(), "fn deploy() {}");
    nrg(d.path())
        .env("PATH", d.path().join("no-tools"))
        .arg("doctor")
        .assert()
        .success();
}

#[test]
fn incomplete_upload_never_replaces_existing_destination() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let path = local_ssh(p);
    fs::write(p.join("source"), "complete payload").unwrap();
    fs::write(p.join("dest"), "preserve").unwrap();
    executable(
        &p.join("bin/ssh"),
        "#!/bin/sh\nfor arg do cmd=$arg; done\nhead -c 3 | /bin/sh -c \"$cmd\"\n",
    );
    script(p, r#"upload_file("test", "source", "dest", "0600");"#);
    nrg(p).env("PATH", &path).arg("exec").assert().failure();
    assert_eq!(fs::read_to_string(p.join("dest")).unwrap(), "preserve");
    assert!(!fs::read_dir(p).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("dest.")));
}
