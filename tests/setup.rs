//! Integration: `nrg setup` (roadmap 1.5 step 1) — the CLI wiring around the native SSH
//! preflight (unit-tested directly in `src/cli/setup.rs`) plus the Rhai-engine network-create/
//! proxy-boot half (`eval::run_setup`, unit-tested in `src/engine/eval.rs`): host reachability,
//! the `--yes` confirmation gate for installing Docker, `--dry-run`, and `--proxy`/`--network`.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// A fake `ssh` on PATH standing in for real hosts. Host behavior is driven by a marker
/// embedded in the hostname itself (`$*` includes the ssh destination, so `case` can match on
/// it directly) — same idiom `tests/remove.rs`'s `fake_ssh_bin` uses via its container-name
/// marker, just keyed on the host instead:
///   - `unreachablehost`: every command fails at the SSH-transport layer (exit 255).
///   - `freshhost`: reachable, no runtime found, but installing Docker succeeds.
///   - `installfailhost`: reachable, no runtime found, and installing Docker fails.
///   - `noruntimehost`: reachable, no runtime found (used where install is never attempted).
///   - anything else (e.g. `web1`): reachable, `docker` already on PATH.
fn fake_ssh_bin(dir: &Path, log_path: &Path) {
    let log = log_path.display();
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> {log:?}\n\
         case \"$*\" in\n\
         \x20\x20*unreachablehost*) exit 255 ;;\n\
         \x20\x20*installfailhost*\"get.docker.com\"*) echo 'E: Unable to install' >&2; exit 1 ;;\n\
         \x20\x20*installfailhost*\"command -v\"*) exit 1 ;;\n\
         \x20\x20*freshhost*\"command -v\"*) exit 1 ;;\n\
         \x20\x20*noruntimehost*\"command -v\"*) exit 1 ;;\n\
         \x20\x20*\"command -v\"*) echo \"/usr/bin/docker\"; exit 0 ;;\n\
         \x20\x20*) exit 0 ;;\n\
         esac\n"
    );
    let bin = dir.join("ssh");
    fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), "").unwrap();
    dir
}

/// Prepend a fake-ssh-bearing directory to `PATH`, returning the log file its invocations are
/// recorded to.
fn with_fake_ssh() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    fake_ssh_bin(bin.path(), &log);
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    (bin, log, path)
}

#[test]
fn requires_at_least_one_host() {
    let dir = project();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["setup"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--host"));
}

#[test]
fn rejects_an_unknown_proxy_value() {
    let dir = project();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["setup", "--host", "web1", "--proxy", "traefik"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("must be"));
}

#[test]
fn an_unreachable_host_fails_before_anything_else() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "unreachablehost"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Not reachable"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(
        !invoked.contains("get.docker.com") && !invoked.contains("network create"),
        "must not attempt install or engine work on an unreachable host: {invoked}"
    );
}

#[test]
fn missing_runtime_without_yes_is_informational_and_attempts_nothing_else() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "noruntimehost"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No container runtime found"))
        .stdout(predicates::str::contains("--yes"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(
        !invoked.contains("get.docker.com") && !invoked.contains("network create") && !invoked.contains("pull"),
        "without --yes, nothing beyond the preflight probe must run: {invoked}"
    );
}

#[test]
fn dry_run_reports_the_would_install_message_and_still_renders_the_engine_plan() {
    let dir = project();
    let (_bin, _log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "noruntimehost", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Would install Docker"))
        .stdout(predicates::str::contains("kamal-proxy"));
}

#[test]
fn yes_installs_docker_then_proceeds_to_boot_kamal_proxy() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "freshhost", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("freshhost: Docker installed"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(invoked.contains("get.docker.com"), "got: {invoked}");
    assert!(invoked.contains("kamal-proxy"), "got: {invoked}");
}

#[test]
fn install_failure_stops_before_network_or_proxy_boot() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "installfailhost", "--yes", "--network", "mynet"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Unable to install"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(
        !invoked.contains("network create") && !invoked.contains("kamal-proxy"),
        "a failed install must not fall through to network/proxy boot: {invoked}"
    );
}

#[test]
fn creates_the_network_when_the_runtime_is_already_present() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "web1", "--network", "mynet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("container runtime already present"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(invoked.contains("network create 'mynet'"), "got: {invoked}");
    assert!(invoked.contains("kamal-proxy"), "got: {invoked}");
}

#[test]
fn proxy_caddy_boots_caddy_instead_of_kamal_proxy() {
    let dir = project();
    let (_bin, log, path) = with_fake_ssh();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path)
        .args(["setup", "--host", "web1", "--proxy", "caddy"])
        .assert()
        .success();

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(invoked.contains("caddy"), "got: {invoked}");
    assert!(!invoked.contains("kamal-proxy"), "got: {invoked}");
}
