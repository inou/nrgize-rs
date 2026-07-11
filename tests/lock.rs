//! Integration: `nrg lock status|acquire|release` (roadmap 2.1) — manual control of the R15
//! cross-machine deploy lock. A fake `ssh` on PATH sandboxes the REAL `/tmp/nrg-deploy-lock-*`
//! path prefix into a per-test temp directory (via a `sed` rewrite before `eval`-ing the actual
//! remote command), so these tests exercise genuine `mkdir`/`test -d`/`rm -rf`/`cat` semantics
//! across multiple separate `nrg` invocations without ever touching the real host's `/tmp`.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;
use std::path::Path;

fn fake_ssh_bin(bin_dir: &Path, sandbox_dir: &Path) {
    let script = format!(
        "#!/bin/sh\nfor last; do :; done\nsandboxed=$(printf '%s' \"$last\" | sed \"s#/tmp/nrg-deploy-lock-#{}/nrg-deploy-lock-#g\")\neval \"$sandboxed\"\n",
        sandbox_dir.display()
    );
    let bin = bin_dir.join("ssh");
    fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

/// A project with `app` recorded as deployed to `web1`, plus a fake `ssh` on PATH sandboxing the
/// lock directory. Returns `(project_dir, bin_dir, sandbox_dir, path_env)` — all four temp dirs
/// must stay alive (bound to a variable) for the test's duration, and `path_env` must be passed
/// as the `PATH` for every `nrg lock` invocation so they all share the same sandboxed lock dir.
fn project_with_sandboxed_lock()
-> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("app.target.web1", "localhost:13000");"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let bin = tempfile::tempdir().unwrap();
    let sandbox = tempfile::tempdir().unwrap();
    fake_ssh_bin(bin.path(), sandbox.path());
    let path_env = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    (dir, bin, sandbox, path_env)
}

#[test]
fn status_reports_not_locked_when_the_directory_does_not_exist() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "status", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not locked"))
        .stdout(predicates::str::contains("web1"));
}

#[test]
fn acquire_then_status_reports_locked_by_the_correct_holder() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "acquire", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("locked on web1"));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "status", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCKED on web1"))
        .stdout(predicates::str::contains("via nrg lock acquire"));
}

#[test]
fn acquire_refuses_when_already_locked() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "acquire", "app"])
        .assert()
        .success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "acquire", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already locked"))
        .stderr(predicates::str::contains("via nrg lock acquire"));
}

#[test]
fn release_without_yes_only_previews_and_does_not_touch_the_lock() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "acquire", "app"])
        .assert()
        .success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "release", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Would release"))
        .stdout(predicates::str::contains("--yes"));

    // Still locked — the preview must not have released it.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "status", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("LOCKED"));
}

#[test]
fn release_with_yes_actually_removes_the_lock() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "acquire", "app"])
        .assert()
        .success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "release", "app", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("released on web1"));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "status", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not locked"));
}

#[test]
fn release_on_an_unlocked_service_is_a_clear_no_op_preview() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "release", "app"])
        .assert()
        .success()
        .stdout(predicates::str::contains("nothing to release"));
}

#[test]
fn host_flag_overrides_the_state_derived_default() {
    let (dir, _bin, _sandbox, path_env) = project_with_sandboxed_lock();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "status", "app", "--host", "web9"])
        .assert()
        .success()
        .stdout(predicates::str::contains("web9"))
        .stdout(predicates::str::contains("web1").not());
}

#[test]
fn no_hosts_recorded_and_no_host_flag_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["lock", "status", "ghost"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no hosts recorded"))
        .stderr(predicates::str::contains("--host"));
}
