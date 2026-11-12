//! Integration: `pre_deploy_undo` — the compensation for `pre_deploy` (a DB migration).
//!
//! Real-run tests put a fake `ssh` on PATH that logs every remote command to a file and answers
//! from a few pattern rules (no real host is ever contacted, no docker is ever run). Dry-run
//! tests read the printed plan.

use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::{fs, process::Output};

/// Fake `ssh`: appends `<host> | <command>` to `ssh.log` and answers by pattern. Knobs are plain
/// files in the project dir: `migrate_fails`, `undo_fails`, `web2_unhealthy`.
const FAKE_SSH: &str = r#"#!/bin/sh
for arg do prev=$cur; cur=$arg; done
host=$prev; cmd=$cur
echo "$host | $cmd" >> "$FAKE_LOG"
# Drain stdin (write_remote pipes a payload in) so the sender never hits EPIPE.
cat > /dev/null
case "$cmd" in
  *"-X PATCH 'http://localhost:2019/id/app'"*'"dial":"localhost:3000"'*)
    if [ -e "$FAKE_DIR/restore_fails" ] && [ "$host" = web1 ]; then echo "caddy unreachable" >&2; exit 1; fi
    printf 200 ;;
  *"docker rm -f 'app-web-v9-"*)
    if [ -e "$FAKE_DIR/rm_fails" ] && [ "$host" = web1 ]; then echo "docker is stuck" >&2; exit 1; fi ;;
  *"mktemp -d"*)
    if [ -e "$FAKE_DIR/mktemp_fails" ]; then echo "no space left" >&2; exit 1; fi
    echo /tmp/.nrg-env-fake ;;
  *"docker run --rm"*"db:migrate"*)
    if [ -e "$FAKE_DIR/migrate_sleeps" ]; then sleep 3; fi
    if [ -e "$FAKE_DIR/migrate_fails" ]; then echo "migrate exploded" >&2; exit 1; fi ;;
  *"docker run --rm"*"db:rollback"*)
    if [ -e "$FAKE_DIR/undo_fails" ]; then echo "rollback exploded" >&2; exit 1; fi ;;
  *"http_code"*)
    if [ "$host" = web2 ] && [ -e "$FAKE_DIR/web2_unhealthy" ]; then printf 503; else printf 200; fi ;;
  *"install -d -m 700 /etc/caddy"*)
    if [ -e "$FAKE_DIR/proxy_fails" ]; then echo "disk full" >&2; exit 1; fi ;;
  *"nc -z"*) exit 1 ;;
esac
exit 0
"#;

struct Project {
    dir: tempfile::TempDir,
    path_env: String,
}

impl Project {
    fn new(script: &str) -> Project {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".energize")).unwrap();
        fs::write(dir.path().join("Energize.rhai"), script).unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(&ssh, FAKE_SSH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let path_env = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        Project { dir, path_env }
    }
    fn knob(&self, name: &str) {
        fs::write(self.dir.path().join(name), "").unwrap();
    }
    fn nrg(&self) -> Command {
        let mut c = Command::cargo_bin("nrg").unwrap();
        c.current_dir(self.dir.path())
            .env("PATH", &self.path_env)
            .env("FAKE_DIR", self.dir.path())
            .env("FAKE_LOG", self.dir.path().join("ssh.log"));
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.nrg().args(args).output().unwrap()
    }
    /// The logged remote commands, in order.
    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("ssh.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn deploy_script(extra: &str, image: &str) -> String {
    format!(
        r#"
import "std/deploy" as deploy;
deploy::deploy(["web1", "web2"], "{image}", "app", #{{
    container_port: 3000, skip_build: true, skip_push: true, skip_lock: true,
    health_attempts: 1, health_interval: 0,
    {extra}
}});
"#
    )
}

const BOTH: &str =
    r#"pre_deploy: "bin/rails db:migrate", pre_deploy_undo: "bin/rails db:rollback","#;

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn positions(log: &[String], needle: &str) -> Vec<usize> {
    log.iter()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(i, _)| i)
        .collect()
}

const IMAGE: &str = "ghcr.io/org/app:v9";

fn new_image_run(log: &[String], cmd: &str) -> Vec<usize> {
    log.iter()
        .enumerate()
        .filter(|(_, l)| l.contains("docker run --rm") && l.contains(cmd))
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn successful_deploy_never_runs_the_undo() {
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    let o = p.run(&["exec", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    let log = p.log();
    assert_eq!(new_image_run(&log, "db:migrate").len(), 1, "{log:#?}");
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
    assert!(!text(&o).contains("Undo pre-deploy"), "{}", text(&o));
}

#[test]
fn a_later_host_failing_health_runs_the_undo_once_on_hosts0_after_the_fleet_is_restored() {
    let p = Project::new(&deploy_script(
        &format!("{BOTH} network: \"appnet\", envs: #{{ \"RAILS_ENV\": \"production\" }},"),
        IMAGE,
    ));
    p.knob("web2_unhealthy");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    // The ORIGINAL error is what the process reports.
    assert!(
        out.contains("Health check failed on web2"),
        "original health-check error missing:\n{out}"
    );
    assert!(out.contains("--- Undo pre-deploy on web1 ---"), "{out}");
    assert!(out.contains("bin/rails db:rollback"), "{out}");
    assert!(
        !out.contains("FAILED"),
        "undo should have succeeded:\n{out}"
    );

    let log = p.log();
    let undo = new_image_run(&log, "db:rollback");
    assert_eq!(undo.len(), 1, "undo must run exactly once:\n{log:#?}");
    let line = &log[undo[0]];
    // hosts[0], from the NEW image, with the same network as pre_deploy.
    assert!(line.starts_with("web1 |"), "{line}");
    assert!(line.contains(&format!("'{IMAGE}'")), "{line}");
    assert!(line.contains("--network 'appnet'"), "{line}");
    assert!(
        line.contains("--env-file"),
        "undo must carry the envs: {line}"
    );
    let migrate = &log[new_image_run(&log, "db:migrate")[0]];
    assert!(migrate.contains("--network 'appnet'"), "{migrate}");
    assert!(migrate.contains("--env-file"), "{migrate}");

    // LIFO: it is the very LAST remote command, after the proxy restore (pointing web1 back at
    // the old target, port 3000) and after every new container was removed.
    assert_eq!(undo[0], log.len() - 1, "undo must run last:\n{log:#?}");
    let restore = log
        .iter()
        .rposition(|l| l.starts_with("web1 |") && l.contains("localhost:3000"))
        .expect("proxy restore on web1");
    let removals = positions(&log, "docker rm -f 'app-web-v9-");
    assert_eq!(removals.len(), 2, "{log:#?}");
    assert!(
        restore < undo[0] && removals.iter().all(|r| *r < undo[0]),
        "{log:#?}"
    );
}

#[test]
fn a_proxy_boot_failure_also_runs_the_undo() {
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("proxy_fails");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("disk full"),
        "original proxy error missing:\n{out}"
    );
    assert_eq!(new_image_run(&p.log(), "db:rollback").len(), 1, "{out}");
}

#[test]
fn a_failed_pre_deploy_is_not_compensated() {
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("migrate_fails");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("Pre-deploy release command failed on web1"),
        "{out}"
    );
    assert!(out.contains("migrate exploded"), "{out}");
    let log = p.log();
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
    assert!(!out.contains("Undo pre-deploy"), "{out}");
}

#[test]
fn a_failing_undo_keeps_the_original_error_and_says_the_schema_may_be_new() {
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("web2_unhealthy");
    p.knob("undo_fails");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("Health check failed on web2"),
        "the undo failure must not mask the original error:\n{out}"
    );
    assert!(
        out.contains("pre_deploy_undo command FAILED on web1: bin/rails db:rollback"),
        "{out}"
    );
    assert!(
        out.contains("rollback exploded"),
        "undo stderr missing:\n{out}"
    );
    assert!(out.contains("STILL HAVE THE NEW SCHEMA"), "{out}");
    // The other compensations still ran (the undo is last, but nothing before it was skipped).
    assert_eq!(positions(&p.log(), "docker rm -f 'app-web-v9-").len(), 2);
}

#[test]
fn undo_without_pre_deploy_is_an_early_config_error_touching_no_host() {
    let p = Project::new(&deploy_script(
        r#"pre_deploy_undo: "bin/rails db:rollback","#,
        IMAGE,
    ));
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("cfg.pre_deploy_undo is set") && out.contains("cfg.pre_deploy is not"),
        "{out}"
    );
    assert!(p.log().is_empty(), "no host may be touched: {:#?}", p.log());
    assert!(
        !out.contains("==> Deploying"),
        "must fail before deploying:\n{out}"
    );
}

fn dry_run_plan(script: &str) -> String {
    let p = Project::new(script);
    let o = p.run(&["exec", "--dry-run", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    text(&o)
}

#[test]
fn dry_run_states_which_command_would_undo_pre_deploy() {
    let out = dry_run_plan(&deploy_script(BOTH, IMAGE));
    let migrate = out.find("bin/rails db:migrate").expect("pre_deploy line");
    let note = out
        .find("if a later step fails, this will run on web1 to undo it: bin/rails db:rollback")
        .unwrap_or_else(|| panic!("undo note missing:\n{out}"));
    assert!(
        migrate < note && note < out.find("--- Proxy ---").unwrap(),
        "{out}"
    );
    // Informational only: the undo is never planned as an executed step.
    assert!(
        !out.lines()
            .any(|l| l.contains("docker run --rm") && l.contains("db:rollback")),
        "{out}"
    );
}

#[test]
fn dry_run_without_an_undo_prints_no_note() {
    let out = dry_run_plan(&deploy_script(
        r#"pre_deploy: "bin/rails db:migrate","#,
        IMAGE,
    ));
    assert!(!out.contains("if a later step fails"), "{out}");
}

#[test]
fn rollback_never_runs_the_undo() {
    let p = Project::new(&format!(
        "{}\n{}",
        deploy_script(BOTH, "ghcr.io/org/app:v1"),
        deploy_script(BOTH, "ghcr.io/org/app:v2").replace("import \"std/deploy\" as deploy;", "")
    ));
    let o = p.run(&["exec", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));

    let o = p.run(&["rollback", "app"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("Rolling back app to ghcr.io/org/app:v1"),
        "{out}"
    );
    let log = p.log();
    // Two forward deploys ran the migration; neither the deploys nor the rollback ran the undo.
    assert_eq!(new_image_run(&log, "db:migrate").len(), 2, "{log:#?}");
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_the_roll_runs_the_undo_after_the_fleet_compensations() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    // web2 never gets healthy and is polled for a minute, so the run is still in the roll when
    // the SIGINT arrives.
    let p = Project::new(&deploy_script(BOTH, IMAGE).replace(
        "health_attempts: 1, health_interval: 0",
        "health_attempts: 60, health_interval: 1",
    ));
    p.knob("web2_unhealthy");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(p.dir.path())
        .env("PATH", &p.path_env)
        .env("FAKE_DIR", p.dir.path())
        .env("FAKE_LOG", p.dir.path().join("ssh.log"))
        .args(["exec", "Energize.rhai"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while !p
        .log()
        .iter()
        .any(|l| l.starts_with("web2 |") && l.contains("http_code"))
    {
        assert!(
            Instant::now() < deadline,
            "web2 health polling never started"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let sent = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("nrg did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success());
    let log = p.log();
    let undo = new_image_run(&log, "db:rollback");
    assert_eq!(undo.len(), 1, "the undo must run on Ctrl-C:\n{log:#?}");
    assert_eq!(undo[0], log.len() - 1, "and last:\n{log:#?}");
    assert_eq!(
        positions(&log, "docker rm -f 'app-web-v9-").len(),
        2,
        "{log:#?}"
    );
}

#[test]
fn sigint_while_pre_deploy_is_running_warns_and_does_not_run_the_undo() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("migrate_sleeps");
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("nrg"))
        .current_dir(p.dir.path())
        .env("PATH", &p.path_env)
        .env("FAKE_DIR", p.dir.path())
        .env("FAKE_LOG", p.dir.path().join("ssh.log"))
        .args(["exec", "Energize.rhai"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while new_image_run(&p.log(), "db:migrate").is_empty() {
        assert!(Instant::now() < deadline, "pre_deploy never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Signal nrg only (the fake ssh child keeps running its sleep).
    let sent = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let o = child.wait_with_output().unwrap();
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("pre_deploy was interrupted on web1")
            && out.contains("undo it by hand: bin/rails db:rollback"),
        "{out}"
    );
    assert!(new_image_run(&p.log(), "db:rollback").is_empty(), "{out}");
}

fn rollback_script(extra: &str) -> String {
    format!(
        r#"
import "std/deploy" as deploy;
deploy::rollback(["web1", "web2"], "app", #{{
    health_attempts: 1, health_interval: 0, skip_lock: true,
    {extra}
}});
"#
    )
}

/// Two healthy deploys (v1, v2) so `app.prev` exists, then a second script for the rollback.
fn deployed_twice() -> Project {
    let p = Project::new(&format!(
        "{}\n{}",
        deploy_script(BOTH, "ghcr.io/org/app:v1"),
        deploy_script(BOTH, "ghcr.io/org/app:v2").replace("import \"std/deploy\" as deploy;", "")
    ));
    let o = p.run(&["exec", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    p
}

#[test]
fn script_rollback_given_only_an_undo_is_not_a_config_error_and_ignores_it() {
    let p = deployed_twice();
    fs::write(
        p.dir.path().join("Rb.rhai"),
        rollback_script(r#"pre_deploy_undo: "bin/rails db:rollback","#),
    )
    .unwrap();
    let o = p.run(&["exec", "Rb.rhai"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("ignoring cfg.pre_deploy_undo"), "{out}");
    assert!(new_image_run(&p.log(), "db:rollback").is_empty(), "{out}");
}

#[test]
fn a_failing_script_rollback_with_pre_deploy_and_undo_never_runs_the_undo() {
    let p = deployed_twice();
    p.knob("web2_unhealthy");
    fs::write(p.dir.path().join("Rb.rhai"), rollback_script(BOTH)).unwrap();
    let o = p.run(&["exec", "Rb.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("Health check failed on web2"), "{out}");
    let log = p.log();
    // The caller asked rollback to run pre_deploy, so it ran (2 deploys + the rollback)...
    assert_eq!(new_image_run(&log, "db:migrate").len(), 3, "{log:#?}");
    // ...but the undo never does, even though the rollback failed after it.
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
}

// ---------------------------------------------------------------------------
// Opt-in: `nrg rollback --undo-pre-deploy` / rollback(.., #{ undo_pre_deploy: true })
// ---------------------------------------------------------------------------

/// v1 deployed plainly (its own undo is `db:v1_undo`), then v2 with envs, a network and the
/// `db:rollback` undo. Rolling back therefore goes v2 -> v1, and the undo must be v2's.
fn v1_then_v2_with_undo() -> Project {
    let v1 = deploy_script(
        r#"pre_deploy: "bin/rails db:migrate", pre_deploy_undo: "bin/rails db:v1_undo","#,
        "ghcr.io/org/app:v1",
    );
    let v2 = deploy_script(
        &format!("{BOTH} network: \"appnet\", envs: #{{ \"RAILS_ENV\": \"production\" }},"),
        "ghcr.io/org/app:v2",
    )
    .replace("import \"std/deploy\" as deploy;", "");
    let p = Project::new(&format!("{v1}\n{v2}"));
    let o = p.run(&["exec", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    p
}

fn show_script(p: &Project) {
    fs::write(
        p.dir.path().join("Show.rhai"),
        "print(\"CFG=\" + state_get(\"app.config\"));\nprint(\"IMG=\" + state_get(\"app.image\"));",
    )
    .unwrap();
}

#[test]
fn rollback_without_the_flag_never_runs_the_undo() {
    let p = v1_then_v2_with_undo();
    let o = p.run(&["rollback", "app"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(new_image_run(&p.log(), "db:rollback").is_empty());
    assert!(!text(&o).contains("Undo pre-deploy"), "{}", text(&o));
}

#[test]
fn rollback_with_the_flag_undoes_the_rolled_back_release_after_the_fleet_is_on_the_old_image() {
    let p = v1_then_v2_with_undo();
    let before = p.log().len();
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("--- Undo pre-deploy of v2 (ghcr.io/org/app:v2) on web1 ---"),
        "{out}"
    );
    let log = p.log();
    let undo = new_image_run(&log, "db:rollback");
    assert_eq!(undo.len(), 1, "{log:#?}");
    let line = &log[undo[0]];
    assert!(line.starts_with("web1 |"), "{line}");
    assert!(
        line.contains("'ghcr.io/org/app:v2'"),
        "image N, not N-1: {line}"
    );
    assert!(!line.contains("app:v1"), "{line}");
    assert!(
        line.contains("--network 'appnet'") && line.contains("--env-file"),
        "{line}"
    );
    // After v1 was started, switched in and the old containers retired; only the lock release
    // (rollback holds the lock until the undo is done) comes later.
    let releases = positions(&log[undo[0] + 1..], "rmdir '/tmp/nrg-deploy-lock-app'");
    assert_eq!(
        releases.len(),
        2,
        "lock must be released after the undo, on both hosts:\n{log:#?}"
    );
    assert!(
        log[undo[0] + 1..]
            .iter()
            .all(|l| l.contains("nrg-deploy-lock")),
        "{log:#?}"
    );
    let started_v1 = log[before..]
        .iter()
        .rposition(|l| l.contains("docker run -d") && l.contains("app:v1"))
        .expect("v1 containers started");
    assert!(before + started_v1 < undo[0]);
    assert_eq!(
        positions(&log[before..], "docker rm -f 'app-web-old'").len(),
        2
    );
    // v1's own undo (db:v1_undo) is never what runs.
    assert!(new_image_run(&log, "db:v1_undo").is_empty(), "{log:#?}");
}

#[test]
fn a_failed_container_rollback_does_not_run_the_undo() {
    let p = v1_then_v2_with_undo();
    p.knob("web2_unhealthy");
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("Health check failed on web2"), "{out}");
    assert!(new_image_run(&p.log(), "db:rollback").is_empty(), "{out}");
    assert!(!out.contains("Undo pre-deploy of"), "{out}");
}

#[test]
fn a_failing_rollback_undo_keeps_the_containers_rolled_back_and_exits_non_zero() {
    let p = v1_then_v2_with_undo();
    p.knob("undo_fails");
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("rollback exploded"), "{out}");
    assert!(out.contains("STILL HAVE THE NEW SCHEMA"), "{out}");
    assert!(
        out.contains(
            "Containers were rolled back to ghcr.io/org/app:v1, but the migration undo failed"
        ),
        "{out}"
    );
    show_script(&p);
    let shown = text(&p.run(&["exec", "Show.rhai"]));
    assert!(
        shown.contains("IMG=ghcr.io/org/app:v1"),
        "state must say v1:\n{shown}"
    );
    assert!(
        p.log()
            .last()
            .unwrap()
            .contains("rmdir '/tmp/nrg-deploy-lock-app'"),
        "lock must be released after a failed undo: {:#?}",
        p.log()
    );
    // Nothing re-deployed v2.
    let log = p.log();
    assert!(
        !log.iter()
            .rev()
            .take(3)
            .any(|l| l.contains("docker run -d") && l.contains("app:v2")),
        "{log:#?}"
    );
}

fn assert_nothing_to_undo(p: &Project, o: &Output, log_len_before: usize) {
    let out = text(o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("nothing to undo"), "{out}");
    assert!(out.contains("containers only"), "{out}");
    assert_eq!(p.log().len(), log_len_before, "no host may be touched");
}

fn two_plain_deploys() -> Project {
    let p = Project::new(&format!(
        "{}\n{}",
        deploy_script(
            r#"pre_deploy: "bin/rails db:migrate","#,
            "ghcr.io/org/app:v1"
        ),
        deploy_script(
            r#"pre_deploy: "bin/rails db:migrate","#,
            "ghcr.io/org/app:v2"
        )
        .replace("import \"std/deploy\" as deploy;", "")
    ));
    let o = p.run(&["exec", "Energize.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    p
}

#[test]
fn undo_flag_without_a_saved_undo_is_an_early_error() {
    let p = two_plain_deploys();
    let n = p.log().len();
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    assert_nothing_to_undo(&p, &o, n);
    // Dry run refuses too.
    let o = p.run(&["rollback", "app", "--undo-pre-deploy", "--dry-run"]);
    assert_nothing_to_undo(&p, &o, n);
    // Without the flag it still rolls back.
    assert!(p.run(&["rollback", "app"]).status.success());
}

#[test]
fn undo_flag_after_a_rollback_has_nothing_to_undo() {
    let p = v1_then_v2_with_undo();
    assert!(p.run(&["rollback", "app"]).status.success());
    let n = p.log().len();
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    assert_nothing_to_undo(&p, &o, n);
}

#[test]
fn stdlib_undo_pre_deploy_with_nothing_saved_is_an_early_error() {
    let p = two_plain_deploys();
    fs::write(
        p.dir.path().join("Rb.rhai"),
        rollback_script("undo_pre_deploy: true,"),
    )
    .unwrap();
    let n = p.log().len();
    let o = p.run(&["exec", "Rb.rhai"]);
    assert_nothing_to_undo(&p, &o, n);
}

#[test]
fn rollback_dry_run_plans_the_undo_after_the_container_rollback() {
    let p = v1_then_v2_with_undo();
    let o = p.run(&["rollback", "app", "--undo-pre-deploy", "--dry-run"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("--- Undo pre-deploy of v2 (ghcr.io/org/app:v2) on web1 ---"),
        "{out}"
    );
    let plan = out
        .lines()
        .filter(|l| l.starts_with("  "))
        .collect::<Vec<_>>();
    let undo = plan
        .iter()
        // The command itself is redacted like pre_deploy's (the saved config feeds the list).
        .position(|l| l.contains("docker run --rm") && l.contains("'ghcr.io/org/app:v2'"))
        .unwrap_or_else(|| panic!("undo step missing from plan:\n{out}"));
    let promote = plan
        .iter()
        .rposition(|l| l.contains("docker rename") && l.contains("app-web'"))
        .unwrap();
    assert!(promote < undo, "{out}");
    // And the real log is untouched by a dry run.
    assert!(new_image_run(&p.log(), "db:rollback").is_empty());
}

#[test]
fn a_callers_pre_deploy_undo_in_rollback_cfg_is_not_what_runs() {
    let p = v1_then_v2_with_undo();
    fs::write(
        p.dir.path().join("Rb.rhai"),
        rollback_script(r#"undo_pre_deploy: true, pre_deploy_undo: "bin/rails db:evil","#),
    )
    .unwrap();
    let o = p.run(&["exec", "Rb.rhai"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("ignoring cfg.pre_deploy_undo"), "{out}");
    let log = p.log();
    assert!(positions(&log, "db:evil").is_empty(), "{log:#?}");
    assert_eq!(new_image_run(&log, "db:rollback").len(), 1, "{log:#?}");
}

#[test]
fn saved_config_has_the_undo_after_a_deploy_that_ran_pre_deploy_and_not_after_a_rollback() {
    let p = v1_then_v2_with_undo();
    show_script(&p);
    let shown = text(&p.run(&["exec", "Show.rhai"]));
    assert!(
        shown.contains("\"pre_deploy_undo\":\"bin/rails db:rollback\""),
        "{shown}"
    );
    assert!(p.run(&["rollback", "app"]).status.success());
    let shown = text(&p.run(&["exec", "Show.rhai"]));
    assert!(shown.contains("IMG=ghcr.io/org/app:v1"), "{shown}");
    assert!(
        !shown.contains("pre_deploy_undo"),
        "a rollback release must not carry one:\n{shown}"
    );
}

#[test]
fn hook_post_rollback_fires_after_the_undo() {
    let p = v1_then_v2_with_undo();
    fs::write(
        p.dir.path().join("Rb.rhai"),
        format!(
            "{}\nfn hook_post_rollback(service, image, hosts) {{ print(\"HOOK-FIRED\"); }}\n",
            rollback_script("undo_pre_deploy: true,")
        ),
    )
    .unwrap();
    let o = p.run(&["exec", "Rb.rhai"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    let undo = out.find("--- Undo pre-deploy of v2").expect("undo header");
    let hook = out.find("HOOK-FIRED").expect("hook output");
    assert!(undo < hook, "{out}");
}

#[test]
fn an_undo_whose_docker_run_throws_is_caught_and_reported() {
    let p = v1_then_v2_with_undo();
    p.knob("mktemp_fails"); // the undo needs an env dir (v2 has envs); v1's rollback needs none
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("no space left"), "{out}");
    assert!(out.contains("STILL HAVE THE NEW SCHEMA"), "{out}");
    assert!(
        out.contains("Containers were rolled back to ghcr.io/org/app:v1"),
        "{out}"
    );
    assert!(p.log().last().unwrap().contains("rmdir"), "lock released");
}

#[test]
fn a_secret_in_the_undo_command_never_appears_on_the_rollback_path() {
    let v1 = deploy_script(
        r#"pre_deploy: "bin/rails db:migrate","#,
        "ghcr.io/org/app:v1",
    );
    let v2 = deploy_script(
        r#"pre_deploy: "bin/rails db:migrate",
           pre_deploy_undo: "bin/rails db:rollback PGPASS=" + reveal(secret("DBPW")),"#,
        "ghcr.io/org/app:v2",
    )
    .replace("import \"std/deploy\" as deploy;", "");
    let p = Project::new(&format!("{v1}\n{v2}"));
    let secret = "hunter2-very-secret";
    let run = |args: &[&str]| {
        let o = p
            .nrg()
            .env("NRG_SECRET_DBPW", secret)
            .args(args)
            .output()
            .unwrap();
        text(&o)
    };
    let deployed = run(&["exec", "Energize.rhai"]);
    assert!(!deployed.contains(secret), "deploy leaked:\n{deployed}");

    let dry = run(&["rollback", "app", "--undo-pre-deploy", "--dry-run"]);
    assert!(dry.contains("Undo pre-deploy of v2"), "{dry}");
    assert!(!dry.contains(secret), "dry run leaked:\n{dry}");

    p.knob("undo_fails");
    let failed = run(&["rollback", "app", "--undo-pre-deploy"]);
    assert!(failed.contains("migration undo failed"), "{failed}");
    assert!(!failed.contains(secret), "failure leaked:\n{failed}");
    let audit = fs::read_to_string(p.dir.path().join(".energize/audit.log")).expect("audit.log");
    assert!(!audit.contains(secret), "audit.log leaked:\n{audit}");
    // The command itself did get the real value.
    assert!(
        p.log()
            .iter()
            .any(|l| l.contains("db:rollback PGPASS=hunter2-very-secret")),
        "the undo must run with the real command"
    );
}

#[test]
fn undo_is_refused_for_any_target_but_the_saved_previous_release() {
    let p = v1_then_v2_with_undo();
    let n = p.log().len();
    for image in ["ghcr.io/org/app:v2", "ghcr.io/org/app:v0"] {
        let o = p.run(&["rollback", "app", "--image", image, "--undo-pre-deploy"]);
        let out = text(&o);
        assert!(!o.status.success(), "{out}");
        assert!(out.contains("saved previous release"), "{out}");
        assert_eq!(p.log().len(), n, "no host may be touched");
    }
    // --image naming the saved previous release itself is fine.
    let o = p.run(&[
        "rollback",
        "app",
        "--image",
        "ghcr.io/org/app:v1",
        "--undo-pre-deploy",
    ]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn undo_is_refused_for_a_partial_host_rollback() {
    let p = v1_then_v2_with_undo();
    let n = p.log().len();
    let o = p.run(&["rollback", "app", "--host", "web2", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("web1") && out.contains("every host"), "{out}");
    assert_eq!(p.log().len(), n, "no host may be touched");
    // Naming every host is fine.
    let o = p.run(&[
        "rollback",
        "app",
        "--host",
        "web1",
        "--host",
        "web2",
        "--undo-pre-deploy",
    ]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn with_no_previous_release_the_missing_rollback_target_is_what_is_reported() {
    let p = Project::new(&deploy_script(BOTH, "ghcr.io/org/app:v1"));
    assert!(p.run(&["exec", "Energize.rhai"]).status.success());
    let o = p.run(&["rollback", "app", "--undo-pre-deploy"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("No rollback image found"), "{out}");
}

#[test]
fn a_release_whose_migration_was_undone_cannot_be_rolled_forward_until_redeployed() {
    let p = v1_then_v2_with_undo();
    assert!(p
        .run(&["rollback", "app", "--undo-pre-deploy"])
        .status
        .success());
    // A plain rollback would now go to v2 (the saved previous image) without its migration.
    let n = p.log().len();
    let o = p.run(&["rollback", "app"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("migration was undone") && out.contains("Deploy this version again"),
        "{out}"
    );
    assert_eq!(p.log().len(), n, "no host may be touched");

    // Deploying v2 again re-runs its pre_deploy and clears the marker...
    fs::write(
        p.dir.path().join("Again.rhai"),
        deploy_script(
            &format!("{BOTH} network: \"appnet\", envs: #{{ \"RAILS_ENV\": \"production\" }},"),
            "ghcr.io/org/app:v2",
        ),
    )
    .unwrap();
    let o = p.run(&["exec", "Again.rhai"]);
    assert!(o.status.success(), "{}", text(&o));
    // ...so a plain rollback works again (back to v1, the previous release).
    let o = p.run(&["rollback", "app"]);
    assert!(o.status.success(), "{}", text(&o));
}
    // The caller asked rollback to run pre_deploy, so it ran (2 deploys + the rollback)...
    assert_eq!(new_image_run(&log, "db:migrate").len(), 3, "{log:#?}");
    // ...but the undo never does, even though the rollback failed after it.
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
}

#[test]
fn undo_is_skipped_when_a_hosts_proxy_restore_fails_and_the_new_container_keeps_serving() {
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("web2_unhealthy");
    p.knob("restore_fails");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    // The original error is still what the deploy reports.
    assert!(out.contains("Health check failed on web2"), "{out}");
    // Existing behaviour: the new container is left running on web1 (not removed).
    assert!(out.contains("leaving app-web-v9-13000 running"), "{out}");
    let log = p.log();
    assert!(
        !log.iter()
            .any(|l| l.starts_with("web1 |") && l.contains("docker rm -f 'app-web-v9-")),
        "web1's new container must be left alone:\n{log:#?}"
    );
    // The undo is NOT run, and the warning names web1 and the command.
    assert!(new_image_run(&log, "db:rollback").is_empty(), "{log:#?}");
    assert!(
        out.contains("pre_deploy_undo was NOT run: web1 could not be put back")
            && out.contains("undo it by hand with: bin/rails db:rollback"),
        "{out}"
    );
    assert!(!out.contains("--- Undo pre-deploy on"), "{out}");
}

#[test]
fn undo_is_skipped_when_removing_a_new_container_fails() {
    // The old container is serving again here, so the undo would arguably be safe, but the
    // guard cannot tell "removal failed" from "restore failed" and errs on not touching the schema.
    let p = Project::new(&deploy_script(BOTH, IMAGE));
    p.knob("web2_unhealthy");
    p.knob("rm_fails");
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("Health check failed on web2"), "{out}");
    assert!(new_image_run(&p.log(), "db:rollback").is_empty(), "{out}");
    assert!(out.contains("pre_deploy_undo was NOT run: web1"), "{out}");
}

#[test]
fn a_flag_left_by_an_earlier_deploy_in_the_same_run_does_not_suppress_the_next_undo() {
    // First deploy: web1's restore fails (flag set, no undo). Second deploy in the SAME script
    // run: the flags are reset first, so its undo still runs.
    let call = "deploy::deploy([\"web1\", \"web2\"], \"ghcr.io/org/app:v9\", \"app\", #{ container_port: 3000, skip_build: true, skip_push: true, skip_lock: true, health_attempts: 1, health_interval: 0, pre_deploy: \"bin/rails db:migrate\", pre_deploy_undo: \"bin/rails db:rollback\" });";
    let script = format!(
        "import \"std/deploy\" as deploy;\nfn once() {{ {call} }}\ntry {{ once(); }} catch (e) {{ print(\"FIRST-FAILED\"); }}\ntry {{ once(); }} catch (e) {{ print(\"SECOND-FAILED\"); }}\n"
    );
    let p = Project::new(&script);
    p.knob("web2_unhealthy");
    p.knob("restore_fails");
    // Make the restore failure one-shot: the fake removes the knob when it fires.
    let ssh = p.dir.path().join("bin/ssh");
    let patched = fs::read_to_string(&ssh).unwrap().replace(
        "echo \"caddy unreachable\" >&2; exit 1;",
        "rm -f \"$FAKE_DIR/restore_fails\"; echo \"caddy unreachable\" >&2; exit 1;",
    );
    fs::write(&ssh, patched).unwrap();
    let o = p.run(&["exec", "Energize.rhai"]);
    let out = text(&o);
    assert!(
        out.contains("FIRST-FAILED") && out.contains("SECOND-FAILED"),
        "{out}"
    );
    assert_eq!(
        new_image_run(&p.log(), "db:rollback").len(),
        1,
        "only the second deploy (clean unwind) runs the undo:\n{out}"
    );
}
