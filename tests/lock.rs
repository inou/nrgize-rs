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
fn multiple_hosts_recorded_and_no_host_flag_refuses_to_guess() {
    // Opus review, round 6: StateStore::hosts_for returns EVERY host ever recorded for the
    // service, sorted alphabetically — not scoped to the specific `hosts` array the real lock
    // actually lives on (`lock_host_for`, lib/deploy.rhai; Fable review, full-project pass).
    // Auto-picking from that unscoped list would silently target the wrong host whenever it
    // differs from the holding call's own array (e.g. a fleet that's grown or shrunk since).
    // With more than one host recorded, this must refuse and require --host, not guess.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        state_set("app.target.web1", "localhost:13000");
        state_set("app.target.web3", "localhost:13002");
        "#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["lock", "status", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("2 hosts recorded"))
        .stderr(predicates::str::contains("web1"))
        .stderr(predicates::str::contains("web3"))
        .stderr(predicates::str::contains("--host"));
}

#[test]
fn status_reports_a_spawn_failure_as_an_error_not_a_false_not_locked() {
    // Fable's final review (round 6): `RealRunner::run_ssh` reports exit code `-1` (not `255`)
    // when `ssh` can't even be spawned, or is rejected by its own option-injection guard — the
    // ORIGINAL "any nonzero code that isn't 255 means not locked" logic silently misreported
    // this as "not locked" instead of surfacing the real spawn failure. An empty PATH (no `ssh`
    // binary anywhere) reliably reproduces a spawn failure regardless of the test process's
    // privilege level, unlike trying to force a permission-denied error.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("app.target.web1", "localhost:13000");"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let empty_bin = tempfile::tempdir().unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", empty_bin.path())
        .args(["lock", "status", "app"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("not locked").not());
}

#[test]
fn release_preview_reports_a_spawn_failure_as_an_error_not_a_false_nothing_to_release() {
    // Fable's final review (round 6): same reasoning as `status`'s spawn-failure test above — the
    // preview path's `test -d` check must treat any non-1, non-255 exit code (e.g. `-1` for a
    // spawn failure) as a real failure to even check, not a negative "nothing to release" answer.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("app.target.web1", "localhost:13000");"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let empty_bin = tempfile::tempdir().unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", empty_bin.path())
        .args(["lock", "release", "app"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("nothing to release").not());
}

fn fake_ssh_unreachable_bin(bin_dir: &Path) {
    let script = "#!/bin/sh\necho 'ssh: connect to host web1 port 22: Connection refused' >&2\nexit 255\n";
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

#[test]
fn release_preview_reports_an_unreachable_host_instead_of_a_false_not_locked() {
    // Opus review, round 6: the --yes-less preview path didn't check for exit 255 (SSH
    // transport failure) before treating any nonzero exit as "not locked" — an operator running
    // the safe preview first on an unreachable host would wrongly conclude the lock was already
    // clear, when it was never actually checked.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("app.target.web1", "localhost:13000");"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let bin = tempfile::tempdir().unwrap();
    fake_ssh_unreachable_bin(bin.path());
    let path_env = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "release", "app"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unreachable"))
        .stdout(predicates::str::contains("nothing to release").not());
}

fn fake_ssh_rm_failure_bin(bin_dir: &Path) {
    let script = "#!/bin/sh\necho \"rm: cannot remove '/tmp/nrg-deploy-lock-app': Device or resource busy\"\nexit 1\n";
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

#[test]
fn release_with_yes_surfaces_the_real_failure_reason_from_combined_output() {
    // Fable's final review (round 6): `rm -rf ... 2>&1` redirects the remote command's OWN
    // stderr onto ITS OWN stdout, so the real failure reason lands in `stdout` — reading
    // `stderr` alone (the original bug) silently fell back to a generic "rm -rf failed" message
    // instead of surfacing this. A fake `ssh` that echoes the distinctive reason to stdout and
    // exits 1 reproduces this reliably, unlike a real permission/immutability-based `rm` failure
    // which is unreliable under the root privilege this test process runs with.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("app.target.web1", "localhost:13000");"#,
    )
    .unwrap();
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).arg("exec").assert().success();

    let bin = tempfile::tempdir().unwrap();
    fake_ssh_rm_failure_bin(bin.path());
    let path_env = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .args(["lock", "release", "app", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Device or resource busy"));
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
