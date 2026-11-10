//! Live-mode golden tests: each scenario runs a stdlib entry point against a scripted runner in
//! live mode and pins the ordered runner calls plus the final script result. The expectations
//! are the observable contract of the live command sequences, so they are compared literally.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::{CommandRunner, RawOutput};
use std::collections::VecDeque;
use std::fs;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn out(exit_code: i64, stdout: &str, stderr: &str) -> RawOutput {
    RawOutput {
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        exit_code,
    }
}

/// (command needle, restrict to host, queued answers)
type Rule = (String, Option<String>, VecDeque<RawOutput>);

/// A runner that records calls in the same format as `FakeRunner` and answers by ordered rules.
/// The first rule whose needle occurs in the command answers; a rule with several queued
/// answers pops them front to back and repeats the last one.
struct Scripted {
    calls: Mutex<Vec<String>>,
    rules: Mutex<Vec<Rule>>,
}

impl Scripted {
    fn new() -> Self {
        Scripted {
            calls: Mutex::new(Vec::new()),
            rules: Mutex::new(Vec::new()),
        }
    }
    /// Answer every command containing `needle` with `answers` in order (last one repeats).
    fn rule(self, needle: &str, answers: Vec<RawOutput>) -> Self {
        self.rules
            .lock()
            .unwrap()
            .push((needle.to_string(), None, answers.into()));
        self
    }
    /// Like `rule`, restricted to one host.
    fn host_rule(self, host: &str, needle: &str, answers: Vec<RawOutput>) -> Self {
        self.rules.lock().unwrap().push((
            needle.to_string(),
            Some(host.to_string()),
            answers.into(),
        ));
        self
    }
    fn answer(&self, host: Option<&str>, cmd: &str) -> RawOutput {
        for (needle, only, queue) in self.rules.lock().unwrap().iter_mut() {
            if !cmd.contains(needle.as_str()) {
                continue;
            }
            if let (Some(want), Some(got)) = (only.as_deref(), host) {
                if want != got {
                    continue;
                }
            }
            return if queue.len() > 1 {
                queue.pop_front().unwrap()
            } else {
                queue.front().cloned().unwrap_or_else(|| out(0, "", ""))
            };
        }
        out(0, "", "")
    }
    fn record(&self, line: String) {
        self.calls.lock().unwrap().push(mask_temp_names(&line));
    }
}

impl CommandRunner for Scripted {
    fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
        self.record(format!("ssh {host}: {cmd}"));
        self.answer(Some(host), cmd)
    }
    fn run_local(&self, cmd: &str) -> RawOutput {
        self.record(format!("local: {cmd}"));
        self.answer(None, cmd)
    }
    fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
        self.record(format!("ssh-stdin {host}: {cmd} <<< {stdin}"));
        self.answer(Some(host), cmd)
    }
    fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
        self.record(format!("local-stdin: {cmd} <<< {stdin}"));
        self.answer(None, cmd)
    }
}

/// Replace the random suffix of `mktemp`-style `.tmpXXXXXX` names (the deploy lock's holder
/// token) so recorded calls are reproducible.
fn mask_temp_names(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find(".tmp") {
        let (head, tail) = rest.split_at(i + 4);
        out.push_str(head);
        let n = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if n == 6 {
            out.push_str("XXXXXX");
            rest = &tail[n..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

struct Outcome {
    result: Result<(), String>,
    calls: Vec<String>,
}

/// Run `script` live against `runner`.
fn run(script: &str, runner: Scripted) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(&main, script).unwrap();
    let runner = Arc::new(runner);
    let result = run_file(&main, shared(runner.clone()));
    let calls = runner.calls.lock().unwrap().clone();
    Outcome { result, calls }
}

/// The script error without the engine's source-position suffix (`@ 'std/x' (line N, ...)` or
/// `(line N, position M)`), which moves whenever a script or stdlib file is edited.
fn error_message(err: &str) -> &str {
    let cut = [" @ '", " (line "]
        .iter()
        .filter_map(|m| err.find(m))
        .min()
        .unwrap_or(err.len());
    &err[..cut]
}

/// Compare an outcome with the pinned expectation for `name` (see `expected` below). With
/// `GOLDEN_DUMP=<file>` set, append the outcome as a JSON line instead of comparing.
fn golden(name: &str, o: &Outcome) {
    let err = o.result.as_ref().err().map(|e| error_message(e));
    if let Ok(path) = std::env::var("GOLDEN_DUMP") {
        use std::io::Write;
        let line = serde_json::json!({ "name": name, "err": err, "calls": o.calls });
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(format!("{line}\n").as_bytes()).unwrap();
        return;
    }
    let (calls, want) = expected(name);
    let calls: Vec<String> = calls.iter().map(|c| c.to_string()).collect();
    assert_eq!(
        order_fan_out(&o.calls),
        order_fan_out(&calls),
        "{name}: runner calls differ"
    );
    assert_eq!(err, want, "{name}: script result differs");
}

/// Split an `ssh`/`ssh-stdin` call line into (kind, host, command-and-rest).
fn split_remote(line: &str) -> Option<(&str, &str, &str)> {
    let (kind, rest) = line.split_once(' ')?;
    if kind != "ssh" && kind != "ssh-stdin" {
        return None;
    }
    let (host, cmd) = rest.split_once(": ")?;
    Some((kind, host, cmd))
}

/// `ssh_exec_all` runs its hosts in parallel, so the per-host calls of one fan-out arrive in
/// any order. Within every maximal run of consecutive remote calls that carry the same command
/// text, sort by host; the order of everything else stays pinned.
fn order_fan_out(calls: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(calls.len());
    let mut i = 0;
    while i < calls.len() {
        let Some((kind, _, cmd)) = split_remote(&calls[i]) else {
            result.push(calls[i].clone());
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < calls.len()
            && split_remote(&calls[j]).is_some_and(|(k, _, c)| k == kind && c == cmd)
        {
            j += 1;
        }
        let mut group: Vec<&String> = calls[i..j].iter().collect();
        group.sort_by_key(|l| split_remote(l).map(|(_, h, _)| h.to_string()));
        result.extend(group.into_iter().cloned());
        i = j;
    }
    result
}

/// A stray `GOLDEN_DUMP` makes `golden` skip every comparison, so the suite must fail on it.
#[test]
fn golden_dump_is_not_set() {
    assert!(
        std::env::var_os("GOLDEN_DUMP").is_none(),
        "GOLDEN_DUMP is set: goldens are being dumped, not compared"
    );
}

fn no_such() -> RawOutput {
    out(1, "", "Error: No such container: x")
}

/// A one-host deploy script; `extra` is spliced into the cfg map.
fn deploy_script(hosts: &str, extra: &str) -> String {
    format!(
        r#"import "std/deploy" as deploy;
           deploy::deploy({hosts}, "ghcr.io/org/app:v9", "app", #{{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1, health_interval: 0, {extra}
           }});"#
    )
}

// ---------------------------------------------------------------------------
// Docker wrappers
// ---------------------------------------------------------------------------

#[test]
fn docker_run_plain_and_with_every_option() {
    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_run("h1", "img:1", "web");
           throw "ok=" + r.ok + " code=" + r.exit_code;"#,
        Scripted::new(),
    );
    golden("docker_run_plain", &o);

    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_run("h1", "img:1", "web", #{
               ports: #{"127.0.0.1:8080": "80"}, envs: #{A: "1"},
               volumes: #{"/data": "/d"}, network: "net1", extra: "--cpus 2",
           });
           throw "ok=" + r.ok;"#,
        Scripted::new().rule("mktemp", vec![out(0, "/tmp/.nrg-env-abc\n", "")]),
    );
    golden("docker_run_all_options", &o);
}

#[test]
fn docker_run_env_file_failures_throw_before_running() {
    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_run("h1", "img:1", "web", #{ envs: #{A: "1"} });"#,
        Scripted::new().rule("mktemp", vec![out(1, "", "mktemp: denied")]),
    );
    golden("docker_run_mktemp_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_run("h1", "img:1", "web", #{ envs: #{A: "1"} });"#,
        Scripted::new(),
    );
    golden("docker_run_mktemp_empty", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_run("h1", "img:1", "web", #{ envs: #{A: "1"} });"#,
        Scripted::new()
            .rule("mktemp -d", vec![out(0, "/tmp/.nrg-env-abc", "")])
            .rule("umask 077; dest=", vec![out(1, "", "disk full")]),
    );
    golden("docker_run_env_write_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_run("h1", "img:1", "web", #{ envs: #{A: "x\ny"} });"#,
        Scripted::new().rule("mktemp", vec![out(0, "/tmp/.nrg-env-abc", "")]),
    );
    golden("docker_run_env_newline_rejected", &o);
}

#[test]
fn docker_run_once_runs_a_throwaway_container() {
    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_run_once("h1", "img:1", "bin/migrate", #{ network: "n" });
           throw "ok=" + r.ok;"#,
        Scripted::new(),
    );
    golden("docker_run_once_plain", &o);

    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_run_once("h1", "img:1", "bin/migrate", #{ envs: #{K: "v"} });
           throw "ok=" + r.ok;"#,
        Scripted::new().rule("mktemp", vec![out(0, "/tmp/.nrg-env-abc", "")]),
    );
    golden("docker_run_once_env", &o);
}

#[test]
fn docker_stop_and_remove_treat_only_a_missing_container_as_success() {
    for (name, call, answer) in [
        (
            "docker_stop_ok",
            "docker_stop(\"h1\", \"web\")",
            out(0, "web", ""),
        ),
        (
            "docker_stop_absent",
            "docker_stop(\"h1\", \"web\")",
            no_such(),
        ),
        (
            "docker_stop_timeout_cfg",
            "docker_stop(\"h1\", \"web\", #{ timeout: 5 })",
            out(0, "", ""),
        ),
        (
            "docker_stop_other_failure",
            "docker_stop(\"h1\", \"web\")",
            out(1, "", "permission denied"),
        ),
        (
            "docker_stop_ssh_failure",
            "docker_stop(\"h1\", \"web\")",
            out(255, "", "No such container (ssh)"),
        ),
        (
            "docker_remove_ok",
            "docker_remove(\"h1\", \"web\")",
            out(0, "", ""),
        ),
        (
            "docker_remove_absent",
            "docker_remove(\"h1\", \"web\")",
            no_such(),
        ),
        (
            "docker_remove_other_failure",
            "docker_remove(\"h1\", \"web\")",
            out(1, "", "device busy"),
        ),
    ] {
        let o = run(
            &format!(
                r#"import "std/docker" as docker;
                   let r = docker::{call};
                   throw "ok=" + r.ok + " code=" + r.exit_code;"#
            ),
            Scripted::new()
                .rule(" stop ", vec![answer.clone()])
                .rule(" rm ", vec![answer]),
        );
        golden(name, &o);
    }
}

#[test]
fn docker_rename_and_promote_classify_failures() {
    for (name, call, answer) in [
        (
            "docker_rename_ok",
            "docker_rename(\"h1\", \"a\", \"b\")",
            out(0, "", ""),
        ),
        (
            "docker_rename_absent_is_ok",
            "docker_rename(\"h1\", \"a\", \"b\")",
            out(1, "", "Error: No such container: a"),
        ),
        (
            "docker_rename_podman_absent_is_ok",
            "docker_rename(\"h1\", \"a\", \"b\")",
            out(
                125,
                "",
                "no container with name or ID \"a\" found: no such container",
            ),
        ),
        (
            "docker_rename_conflict_is_failure",
            "docker_rename(\"h1\", \"a\", \"b\")",
            out(1, "", "Error: name \"b\" is already in use"),
        ),
        (
            "docker_rename_transport_failure",
            "docker_rename(\"h1\", \"a\", \"b\")",
            out(-1, "", "No such container"),
        ),
        (
            "docker_promote_ok",
            "docker_promote(\"h1\", \"a\", \"b\")",
            out(0, "", ""),
        ),
        (
            "docker_promote_absent_is_failure",
            "docker_promote(\"h1\", \"a\", \"b\")",
            out(1, "", "Error: No such container: a"),
        ),
    ] {
        let o = run(
            &format!(
                r#"import "std/docker" as docker;
                   let r = docker::{call};
                   throw "ok=" + r.ok + " code=" + r.exit_code;"#
            ),
            Scripted::new().rule("rename", vec![answer]),
        );
        golden(name, &o);
    }
}

#[test]
fn docker_restart_exec_logs_and_cleanup_commands() {
    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_restart("h1", "web");
           docker::docker_exec("h1", "web", "ls /");
           docker::docker_logs("h1", "web");
           docker::docker_logs("h1", "web", #{ tail: 5 });
           docker::docker_cleanup("h1");"#,
        Scripted::new(),
    );
    golden("docker_misc_commands", &o);

    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_restart("h1", "web");
           throw "ok=" + r.ok + " err=" + r.stderr;"#,
        Scripted::new().rule("restart", vec![out(1, "", "no such container")]),
    );
    golden("docker_restart_absent_is_failure", &o);
}

#[test]
fn docker_network_create_is_idempotent_only_for_already_exists() {
    for (name, answer) in [
        ("docker_network_ok", out(0, "id", "")),
        (
            "docker_network_exists",
            out(1, "", "network with name n already exists"),
        ),
        ("docker_network_other_failure", out(1, "", "daemon down")),
        (
            "docker_network_transport_failure",
            out(-1, "", "already exists"),
        ),
    ] {
        let o = run(
            r#"import "std/docker" as docker;
               let r = docker::docker_network_create("h1", "n");
               throw "ok=" + r.ok + " code=" + r.exit_code;"#,
            Scripted::new().rule("network create", vec![answer]),
        );
        golden(name, &o);
    }

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_network_create_all(["h1", "h2", "h3"], "n");"#,
        Scripted::new().host_rule("h2", "network create", vec![out(1, "", "daemon down")]),
    );
    golden("docker_network_create_all_stops_at_first_failure", &o);
}

#[test]
fn docker_pull_push_and_pull_all() {
    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_pull("h1", "img:1");
           docker::docker_push("h1", "img:1");
           docker::docker_pull_all(["h1", "h2"], "img:1");"#,
        Scripted::new(),
    );
    golden("docker_pull_push_ok", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_pull("h1", "img:1");"#,
        Scripted::new().rule("pull", vec![out(1, "", "denied")]),
    );
    golden("docker_pull_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_push("h1", "img:1");"#,
        Scripted::new().rule("push", vec![out(1, "", "denied")]),
    );
    golden("docker_push_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_pull_all(["h1", "h2"], "img:1");"#,
        Scripted::new().host_rule("h2", "pull", vec![out(1, "", "denied")]),
    );
    golden("docker_pull_all_names_failed_hosts", &o);
}

#[test]
fn docker_container_running_and_image_id_reads() {
    for (name, answer) in [
        ("running_true", out(0, "true\n", "")),
        ("running_false", out(0, "false\n", "")),
        ("running_absent", no_such()),
        ("running_ssh_down", out(255, "", "ssh: connect refused")),
        ("running_cli_missing", out(127, "", "sh: docker: not found")),
        (
            "running_spawn_failure",
            out(-1, "", "No such file or directory"),
        ),
        ("running_signal_killed", out(137, "", "")),
        ("running_other_failure", out(1, "", "permission denied")),
    ] {
        let o = run(
            r#"import "std/docker" as docker;
               throw "running=" + docker::docker_container_running("h1", "web");"#,
            Scripted::new().rule("inspect", vec![answer]),
        );
        golden(&format!("docker_container_{name}"), &o);
    }

    for (name, answer) in [
        ("found", out(0, "sha256:abc\n", "")),
        ("absent", no_such()),
        ("podman_absent", out(125, "", "Error: image not known")),
        ("ssh_down", out(255, "", "ssh: connect refused")),
        ("other_failure", out(1, "", "permission denied")),
    ] {
        let o = run(
            r#"import "std/docker" as docker;
               throw "id=[" + docker::docker_image_id("h1", "img:1") + "]";"#,
            Scripted::new().rule("image inspect", vec![answer]),
        );
        golden(&format!("docker_image_id_{name}"), &o);
    }
}

#[test]
fn docker_commands_follow_the_configured_runtime() {
    let o = run(
        r#"import "std/runtime" as rt;
           import "std/docker" as docker;
           rt::set_runtime("podman");
           docker::docker_run("h1", "img:1", "web");
           docker::docker_stop("h1", "web");
           docker::docker_container_running("h1", "web");
           throw "id=[" + docker::docker_image_id("h1", "img:1") + "]";"#,
        Scripted::new().rule("image inspect", vec![out(125, "", "image not known")]),
    );
    golden("docker_podman_runtime_commands", &o);
}

// ---------------------------------------------------------------------------
// Waits
// ---------------------------------------------------------------------------

#[test]
fn wait_port_polls_until_open_and_throws_on_exhaustion() {
    let o = run(
        r#"import "std/healthcheck" as health;
           throw "r=" + health::wait_port("h1", 5432, #{ attempts: 5, interval: 0 });"#,
        Scripted::new().rule(
            "nc -z",
            vec![out(1, "", ""), out(1, "", ""), out(0, "", "")],
        ),
    );
    golden("wait_port_opens_on_third_try", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_port("h1", 5432, #{ attempts: 3, interval: 0 });"#,
        Scripted::new().rule("nc -z", vec![out(1, "", "")]),
    );
    golden("wait_port_exhausts", &o);

    for (name, answer) in [
        ("wait_port_ssh_down", out(255, "", "ssh: connect refused")),
        ("wait_port_nc_missing", out(127, "", "nc: not found")),
        ("wait_port_signal_killed", out(137, "", "")),
    ] {
        let o = run(
            r#"import "std/healthcheck" as health;
               health::wait_port("h1", 5432, #{ attempts: 3, interval: 0 });"#,
            Scripted::new().rule("nc -z", vec![answer]),
        );
        golden(name, &o);
    }

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_port("h1", 5432, #{ attempts: 0 });"#,
        Scripted::new(),
    );
    golden("wait_port_rejects_zero_attempts", &o);
}

#[test]
fn wait_container_healthy_polls_status_and_throws_on_exhaustion() {
    let o = run(
        r#"import "std/healthcheck" as health;
           throw "r=" + health::wait_container_healthy("h1", "db", #{ attempts: 5, interval: 0 });"#,
        Scripted::new().rule(
            "Health.Status",
            vec![
                out(0, "starting", ""),
                out(0, "<no value>", ""),
                out(0, "healthy", ""),
            ],
        ),
    );
    golden("wait_container_healthy_on_third_try", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_container_healthy("h1", "db", #{ attempts: 2, interval: 0 });"#,
        Scripted::new().rule("Health.Status", vec![out(0, "unhealthy", "")]),
    );
    golden("wait_container_healthy_exhausts", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_container_healthy("h1", "db", #{ attempts: 2, interval: 0 });"#,
        Scripted::new().rule("Health.Status", vec![no_such()]),
    );
    golden("wait_container_healthy_absent", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_container_healthy("h1", "db", #{ attempts: 2, interval: 0 });"#,
        Scripted::new().rule("Health.Status", vec![out(255, "", "ssh: connect refused")]),
    );
    golden("wait_container_healthy_probe_failure", &o);
}

#[test]
fn wait_healthy_on_host_curls_localhost_over_ssh() {
    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_healthy_on_host("h1", 3000, #{ attempts: 4, interval: 0, consecutive: 2 });"#,
        Scripted::new().rule(
            "curl",
            vec![out(0, "200", ""), out(0, "500", ""), out(0, "200", "")],
        ),
    );
    golden("wait_healthy_on_host_consecutive", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_healthy_on_host("h1", 3000, #{ attempts: 2, interval: 0, path: "/ready", timeout: 7 });"#,
        Scripted::new().rule("curl", vec![out(0, "000", "")]),
    );
    golden("wait_healthy_on_host_exhausts", &o);
}

// ---------------------------------------------------------------------------
// Accessories
// ---------------------------------------------------------------------------

const RUNNING: &str = "State.Running";
/// Matches the deploy health gate's curl (its `--max-time` value is quoted) and nothing else.
const HEALTH: &str = "--max-time '";

#[test]
fn accessory_run_starts_verifies_and_skips_when_running() {
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_run("h1", "redis", "redis:7", #{
               ports: #{"127.0.0.1:6379": "6379"}, volumes: #{"redis-data": "/data"},
           });"#,
        Scripted::new().rule(RUNNING, vec![out(0, "false", ""), out(0, "true", "")]),
    );
    golden("accessory_run_fresh", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_run("h1", "redis", "redis:7");"#,
        Scripted::new().rule(RUNNING, vec![out(0, "true", "")]),
    );
    golden("accessory_run_already_running", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_run("h1", "redis", "redis:7");"#,
        Scripted::new().rule(RUNNING, vec![no_such()]),
    );
    golden("accessory_run_exits_immediately", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_run("h1", "redis", "redis:7");"#,
        Scripted::new()
            .rule(RUNNING, vec![out(0, "false", "")])
            .rule("run -d", vec![out(125, "", "pull access denied")]),
    );
    golden("accessory_run_start_fails", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_run("h1", "redis", "redis:7");"#,
        Scripted::new().rule(RUNNING, vec![out(255, "", "ssh: connect refused")]),
    );
    golden("accessory_run_probe_failure", &o);
}

#[test]
fn accessory_stop_and_restart() {
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_stop("h1", "redis");"#,
        Scripted::new().rule(RUNNING, vec![out(0, "true", "")]),
    );
    golden("accessory_stop_running", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_stop("h1", "redis");"#,
        Scripted::new().rule(RUNNING, vec![no_such()]),
    );
    golden("accessory_stop_absent", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_restart("h1", "redis");"#,
        Scripted::new(),
    );
    golden("accessory_restart_ok", &o);

    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_restart("h1", "redis");"#,
        Scripted::new().rule("restart", vec![out(1, "", "no such container")]),
    );
    golden("accessory_restart_fails", &o);
}

#[test]
fn accessory_upgrade_pulls_then_replaces_the_container() {
    // Running before the upgrade: stop and rm succeed, the re-run probe sees it gone and then up.
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_upgrade("h1", "pg", "postgres:17", #{ volumes: #{"pgdata": "/v"} });"#,
        Scripted::new().rule(RUNNING, vec![no_such(), out(0, "true", "")]),
    );
    golden("accessory_upgrade_from_running", &o);

    // Absent before the upgrade: stop and rm report "No such container" and are tolerated.
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_upgrade("h1", "pg", "postgres:17");"#,
        Scripted::new()
            .rule(" stop ", vec![no_such()])
            .rule(" rm ", vec![no_such()])
            .rule(RUNNING, vec![no_such(), out(0, "true", "")]),
    );
    golden("accessory_upgrade_from_absent", &o);

    // A bad pull aborts before anything is stopped.
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_upgrade("h1", "pg", "postgres:17");"#,
        Scripted::new().rule("pull", vec![out(1, "", "manifest unknown")]),
    );
    golden("accessory_upgrade_pull_fails", &o);

    // The new image dies on start.
    let o = run(
        r#"import "std/deploy" as deploy;
           deploy::accessory_upgrade("h1", "pg", "postgres:17");"#,
        Scripted::new().rule(RUNNING, vec![no_such(), out(0, "false", "")]),
    );
    golden("accessory_upgrade_new_image_exits", &o);
}

// ---------------------------------------------------------------------------
// Caddy proxy
// ---------------------------------------------------------------------------

#[test]
fn proxy_boot_when_caddy_is_running_and_when_absent() {
    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_boot("h1");"#,
        Scripted::new().rule(RUNNING, vec![out(0, "true", "")]),
    );
    golden("proxy_boot_already_running", &o);

    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_boot("h1", #{ proxy_version: "2.8.4" });"#,
        Scripted::new().rule(RUNNING, vec![no_such()]),
    );
    golden("proxy_boot_fresh", &o);

    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_boot("h1");"#,
        Scripted::new()
            .rule(RUNNING, vec![no_such()])
            .rule(" pull ", vec![out(1, "", "toomanyrequests")]),
    );
    golden("proxy_boot_pull_fails", &o);

    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_boot("h1");"#,
        Scripted::new()
            .rule(RUNNING, vec![no_such()])
            .rule("localhost:2019/config", vec![out(1, "", "refused")]),
    );
    golden("proxy_boot_admin_never_ready", &o);

    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_boot("h1");"#,
        Scripted::new().rule(RUNNING, vec![out(255, "", "ssh: connect refused")]),
    );
    golden("proxy_boot_probe_failure", &o);
}

#[test]
fn proxy_deploy_patches_or_posts_and_throws_on_failure() {
    let o = run(
        r#"import "std/caddy" as proxy;
           let r = proxy::proxy_deploy("h1", "app", "localhost:13000", #{
               domain: "app.example.com", health_path: "/up",
           });
           throw "ok=" + r.ok;"#,
        Scripted::new(),
    );
    golden("proxy_deploy_with_domain", &o);

    let o = run(
        r#"import "std/caddy" as proxy;
           proxy::proxy_deploy("h1", "app", "localhost:13000");"#,
        Scripted::new().rule(
            "PATCH",
            vec![out(1, "", "Caddy PATCH failed with HTTP 500")],
        ),
    );
    golden("proxy_deploy_fails", &o);
}

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

fn deploy_runner() -> Scripted {
    Scripted::new()
        .rule("nc -z", vec![out(1, "", "")])
        .rule(HEALTH, vec![out(0, "200", "")])
        .rule(RUNNING, vec![no_such()])
}

#[test]
fn deploy_to_one_host() {
    let o = run(&deploy_script(r#"["h1"]"#, ""), deploy_runner());
    golden("deploy_single_host", &o);
}

#[test]
fn deploy_to_a_fleet() {
    let o = run(&deploy_script(r#"["h1", "h2"]"#, ""), deploy_runner());
    golden("deploy_fleet_of_two", &o);
}

#[test]
fn deploy_with_a_failing_health_gate_unwinds() {
    let o = run(
        &deploy_script(r#"["h1"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(1, "", "")])
            .rule(HEALTH, vec![out(0, "503", "")])
            .rule(RUNNING, vec![no_such()]),
    );
    golden("deploy_health_gate_fails_single_host", &o);
}

#[test]
fn deploy_failing_on_the_second_host_restores_the_first() {
    let o = run(
        &deploy_script(r#"["h1", "h2"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(1, "", "")])
            .host_rule("h2", HEALTH, vec![out(0, "000", "")])
            .rule(HEALTH, vec![out(0, "200", "")])
            .rule(RUNNING, vec![no_such()]),
    );
    golden("deploy_fleet_second_host_unhealthy", &o);
}

#[test]
fn deploy_with_an_unrecorded_running_old_container_throws() {
    let o = run(
        &deploy_script(r#"["h1"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(1, "", "")])
            .rule(HEALTH, vec![out(0, "200", "")])
            .rule("'app-web'", vec![out(0, "true\n", "")])
            .rule(RUNNING, vec![out(0, "true\n", "")]),
    );
    golden("deploy_old_container_without_state", &o);

    let o = run(
        &deploy_script(r#"["h1"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(1, "", "")])
            .rule(HEALTH, vec![out(0, "200", "")])
            .rule("'app-web'", vec![out(255, "", "ssh: connect refused")])
            .rule(RUNNING, vec![out(0, "true\n", "")]),
    );
    golden("deploy_old_container_probe_fails", &o);
}

#[test]
fn docker_build_push_and_prune_old_images() {
    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_build("img:1");
           docker::docker_push("img:1");"#,
        Scripted::new(),
    );
    golden("docker_build_and_local_push_ok", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_build("img:1");"#,
        Scripted::new().rule("build", vec![out(1, "", "no space")]),
    );
    golden("docker_build_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           docker::docker_push("img:1");"#,
        Scripted::new().rule("push", vec![out(1, "", "denied")]),
    );
    golden("docker_local_push_fails", &o);

    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_prune_old_images("h1", "app", 1, ["v0"]);
           throw "ok=" + r.ok + " removed=" + r.removed;"#,
        Scripted::new().rule(
            "images",
            vec![out(
                0,
                "v1|2026-01-03 10:00:00\nv2|2026-01-02 10:00:00\nv0|2026-01-01 10:00:00\n<none>|2026-01-04 10:00:00\nv3|2026-01-01 09:00:00\n",
                "",
            )],
        ),
    );
    golden("docker_prune_old_images_removes_older", &o);

    let o = run(
        r#"import "std/docker" as docker;
           let r = docker::docker_prune_old_images("h1", "app", 1, []);
           throw "ok=" + r.ok + " err=" + r.stderr;"#,
        Scripted::new().rule("images", vec![out(255, "", "ssh: connect refused")]),
    );
    golden("docker_prune_old_images_list_fails", &o);
}

#[test]
fn wait_healthy_all_checks_each_host_in_order() {
    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_healthy_all(["h1", "h2"], 8080, #{ attempts: 1, interval: 0 });"#,
        Scripted::new().rule(HEALTH, vec![out(0, "200", "")]),
    );
    golden("wait_healthy_all_ok", &o);

    let o = run(
        r#"import "std/healthcheck" as health;
           health::wait_healthy_all(["h1", "h2"], 8080, #{ attempts: 1, interval: 0 });"#,
        Scripted::new()
            .host_rule("h1", HEALTH, vec![out(0, "503", "")])
            .rule(HEALTH, vec![out(0, "200", "")]),
    );
    golden("wait_healthy_all_first_host_unhealthy", &o);
}

#[test]
fn proxy_stop_remove_and_boot_all() {
    let o = run(
        r#"import "std/caddy" as caddy;
           caddy::proxy_stop("h1");
           caddy::proxy_remove("h1", "app");"#,
        Scripted::new(),
    );
    golden("proxy_stop_and_remove", &o);

    let o = run(
        r#"import "std/caddy" as caddy;
           let r = caddy::proxy_remove("h1", "app");
           throw "ok=" + r.ok + " err=" + r.stderr;"#,
        Scripted::new().rule(
            "DELETE",
            vec![out(1, "", "Caddy DELETE failed with HTTP 500")],
        ),
    );
    golden("proxy_remove_fails", &o);

    let o = run(
        r#"import "std/caddy" as caddy;
           caddy::proxy_boot_all(["h1", "h2"]);"#,
        Scripted::new().rule(RUNNING, vec![out(0, "true\n", "")]),
    );
    golden("proxy_boot_all_running", &o);
}

#[test]
fn deploy_with_no_free_port_throws() {
    let o = run(
        &deploy_script(r#"["h1"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(0, "", "")])
            .rule(RUNNING, vec![no_such()]),
    );
    golden("deploy_port_exhaustion", &o);

    let o = run(
        &deploy_script(r#"["h1"]"#, ""),
        Scripted::new()
            .rule("nc -z", vec![out(127, "", "nc: not found")])
            .rule(RUNNING, vec![no_such()]),
    );
    golden("deploy_port_probe_nc_missing", &o);
}

#[test]
fn deploy_with_extra_options_and_an_existing_old_container() {
    let o = run(
        &deploy_script(
            r#"["h1"]"#,
            r#"envs: #{A: "1"}, volumes: #{"/d": "/v"}, network: "n", domain: "app.example.com",
               health_path: "/ready", pre_deploy_cmd: "echo hi", publish_all_interfaces: true,
               keep_images: 2,"#,
        ),
        deploy_runner()
            .rule("mktemp", vec![out(0, "/tmp/.nrg-env-abc", "")])
            .rule("images", vec![out(0, "", "")]),
    );
    golden("deploy_with_options", &o);

    let o = run(
        &format!(
            r#"state_set("app.port.h1", "13005");{}"#,
            deploy_script(r#"["h1"]"#, "")
        ),
        deploy_runner(),
    );
    golden("deploy_with_recorded_previous_port", &o);
}

#[test]
fn rollback_redeploys_the_previous_image() {
    let o = run(
        r#"import "std/deploy" as deploy;
           state_set("app.image", "ghcr.io/org/app:v8");
           state_set("app.prev", "ghcr.io/org/app:v7");
           state_set("app.port.h1", "13005");
           deploy::rollback(["h1"], "app", #{ health_attempts: 2, health_interval: 0 });"#,
        deploy_runner(),
    );
    golden("rollback_to_previous_image", &o);
}

// ---------------------------------------------------------------------------
// HTTP (control-machine GETs)
// ---------------------------------------------------------------------------

/// A local HTTP server answering each request with the next entry of `bodies` (status 200, last
/// repeats) and recording every request line. Returns the port and the shared request log.
fn spawn_logging_server(bodies: Vec<&'static str>) -> (u16, std::sync::Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = std::sync::Arc::new(Mutex::new(Vec::new()));
    let log_thread = log.clone();
    std::thread::spawn(move || {
        for (n, mut stream) in listener.incoming().flatten().enumerate() {
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            log_thread
                .lock()
                .unwrap()
                .push(text.lines().next().unwrap_or("").to_string());
            let body = bodies[n.min(bodies.len() - 1)];
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (port, log)
}

#[test]
fn wait_healthy_returns_the_last_response_and_counts_requests() {
    let (port, log) = spawn_logging_server(vec!["ready"]);
    let o = run(
        &format!(
            r#"import "std/healthcheck" as health;
               let r = health::wait_healthy("http://127.0.0.1:{port}/up", #{{ attempts: 3, interval: 0 }});
               throw "status=" + r.status + " body=" + r.body + " ok=" + r.ok + " type=" + type_of(r);"#
        ),
        Scripted::new(),
    );
    let err = o.result.unwrap_err();
    assert!(
        err.contains("status=200 body=ready ok=true type=HttpResponse"),
        "{err}"
    );
    assert_eq!(*log.lock().unwrap(), vec!["GET /up HTTP/1.1".to_string()]);
}

#[test]
fn wait_healthy_throws_after_exhausting_attempts() {
    let (port, log) = spawn_logging_server(vec!["x"]);
    let o = run(
        &format!(
            r#"import "std/healthcheck" as health;
               health::wait_healthy("http://127.0.0.1:{port}/", #{{
                   attempts: 3, interval: 0, expected_status: 204,
               }});"#
        ),
        Scripted::new(),
    );
    let err = o.result.unwrap_err();
    assert!(
        err.contains(&format!(
            "Health check failed after 3 attempts: http://127.0.0.1:{port}/ \
             (last status: 200, needed 1 consecutive pass(es))"
        )),
        "{err}"
    );
    assert_eq!(log.lock().unwrap().len(), 3);
}

#[test]
fn wait_healthy_validates_its_arguments_before_any_request() {
    for (cfg, want) in [
        (
            "attempts: 0",
            "wait_healthy: cfg.attempts must be >= 1 (got 0).",
        ),
        (
            "consecutive: 0",
            "wait_healthy: cfg.consecutive must be >= 1 (got 0).",
        ),
        (
            "timeout: 0",
            "wait_healthy: cfg.timeout must be > 0 (got 0).",
        ),
        (
            "timeout: \"5; x\"",
            "wait_healthy: cfg.timeout must be a positive number (got string).",
        ),
    ] {
        let o = run(
            &format!(
                r#"import "std/healthcheck" as health;
                   health::wait_healthy("http://127.0.0.1:1/", #{{ {cfg} }});"#
            ),
            Scripted::new(),
        );
        let err = o.result.unwrap_err();
        assert!(err.contains(want), "{cfg}: {err}");
        assert!(o.calls.is_empty());
    }
}

#[test]
fn wait_healthy_plumbs_the_timeout_to_each_request() {
    // A server that accepts and never answers: each request must give up after the configured
    // timeout (not the 30s default) and surface as status 0.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepted_thread = accepted.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            accepted_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held.push(stream);
        }
    });
    let start = std::time::Instant::now();
    let o = run(
        &format!(
            r#"import "std/healthcheck" as health;
               health::wait_healthy("http://127.0.0.1:{port}/", #{{
                   attempts: 2, interval: 0, timeout: 1,
               }});"#
        ),
        Scripted::new(),
    );
    let elapsed = start.elapsed();
    let err = o.result.unwrap_err();
    assert!(
        err.contains("(last status: 0, needed 1 consecutive pass(es))"),
        "{err}"
    );
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        elapsed >= std::time::Duration::from_secs(2)
            && elapsed < std::time::Duration::from_secs(15),
        "two 1s timeouts expected, took {elapsed:?}"
    );
}

#[test]
fn wait_healthy_rounds_a_fractional_timeout_up() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let start = std::time::Instant::now();
    let o = run(
        &format!(
            r#"import "std/healthcheck" as health;
               health::wait_healthy("http://127.0.0.1:{port}/", #{{
                   attempts: 1, interval: 0, timeout: 1.2,
               }});"#
        ),
        Scripted::new(),
    );
    assert!(o.result.is_err());
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(1900)
            && elapsed < std::time::Duration::from_secs(15),
        "1.2 rounds up to 2s, took {elapsed:?}"
    );
}

#[test]
fn bunny_version_gate_polls_the_health_url_until_the_revision_matches() {
    // One server plays both the Bunny API and the app's health endpoint, routed by request line.
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let log_thread = log.clone();
    std::thread::spawn(move || {
        let mut health_hits = 0;
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            let line = text.lines().next().unwrap_or("").to_string();
            log_thread.lock().unwrap().push(line.clone());
            let body = if line.starts_with("GET /health") {
                health_hits += 1;
                if health_hits < 3 { "v1" } else { "v2" }.to_string()
            } else if line.starts_with("PATCH") {
                "{\"ok\":true}".to_string()
            } else {
                "{\"id\":\"app1\",\"containerTemplates\":[{\"id\":\"c-web\",\"name\":\"web\",\"imageTag\":\"v2\"}]}".to_string()
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let o = run(
        &format!(
            r#"import "std/bunny" as b;
               state_set("bunny.app1.web.prev", "v0");
               let result = b::deploy_fleet([#{{
                   app_id: "app1", container: "web", image_tag: "v2",
                   base_url: "http://127.0.0.1:{port}", health_url: "http://127.0.0.1:{port}/health",
               }}], #{{
                   api_key: "dummy-key", require_version_health: true,
                   health_attempts: 5, health_interval: 0, max_failures: 0,
               }});
               throw "succeeded=" + result.succeeded.len() + " failed=" + result.failed.len();"#
        ),
        Scripted::new(),
    );
    let err = o.result.unwrap_err();
    assert!(err.contains("succeeded=1 failed=0"), "{err}");
    let log = log.lock().unwrap().clone();
    let health: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("GET /health"))
        .collect();
    // Two stale revisions, one match, then wait_healthy's own request.
    assert_eq!(health.len(), 4, "{log:?}");
    assert!(o.calls.is_empty());
}

#[test]
fn bunny_version_gate_gives_up_after_its_attempts() {
    let (port, log) = spawn_logging_server(vec!["{\"id\":\"app1\",\"containerTemplates\":[{\"id\":\"c-web\",\"name\":\"web\",\"imageTag\":\"v2\"}]}"]);
    // The same body answers every request, so the health body is never the expected revision.
    let o = run(
        &format!(
            r#"import "std/bunny" as b;
               let result = b::deploy_fleet([#{{
                   app_id: "app1", container: "web", image_tag: "v2",
                   base_url: "http://127.0.0.1:{port}", health_url: "http://127.0.0.1:{port}/health",
               }}], #{{
                   api_key: "dummy-key", require_version_health: true,
                   health_attempts: 3, health_interval: 0, max_failures: 1,
               }});
               throw "failed=" + result.failed.len() + " error=" + result.failed[0].error;"#
        ),
        Scripted::new(),
    );
    let err = o.result.unwrap_err();
    assert!(
        err.contains("failed=1 error=New application revision did not become ready"),
        "{err}"
    );
    let health = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("GET /health"))
        .count();
    assert_eq!(health, 3);
}

// ---------------------------------------------------------------------------
// Pinned expectations
// ---------------------------------------------------------------------------

fn expected(name: &str) -> (Vec<&'static str>, Option<&'static str>) {
    match name {
        "accessory_restart_fails" => (
            vec![r#"ssh h1: docker restart 'redis'"#],
            Some(
                r#"Runtime error: Failed to restart accessory redis on h1:
no such container"#,
            ),
        ),
        "accessory_restart_ok" => (vec![r#"ssh h1: docker restart 'redis'"#], None),
        "accessory_run_already_running" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#],
            None,
        ),
        "accessory_run_exits_immediately" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
                r#"ssh h1: docker rm -f 'redis'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'redis' 'redis:7'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
            ],
            Some(
                r#"Runtime error: Accessory redis on h1 exited immediately after starting — check its logs (`docker logs redis` on h1, e.g. via `nrg ssh h1`) for why."#,
            ),
        ),
        "accessory_run_fresh" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
                r#"ssh h1: docker rm -f 'redis'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'redis' -p '127.0.0.1:6379:6379' -v 'redis-data:/data' 'redis:7'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
            ],
            None,
        ),
        "accessory_run_probe_failure" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#],
            Some(
                r#"Runtime error: container probe failed for redis on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "accessory_run_start_fails" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
                r#"ssh h1: docker rm -f 'redis'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'redis' 'redis:7'"#,
            ],
            Some(
                r#"Runtime error: Failed to start accessory redis on h1:
pull access denied"#,
            ),
        ),
        "accessory_stop_absent" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#],
            None,
        ),
        "accessory_stop_running" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'redis'"#,
                r#"ssh h1: docker stop -t '30' 'redis'"#,
            ],
            None,
        ),
        "accessory_upgrade_from_absent" => (
            vec![
                r#"ssh h1: docker pull 'postgres:17'"#,
                r#"ssh h1: docker stop -t '30' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'pg' 'postgres:17'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
            ],
            None,
        ),
        "accessory_upgrade_from_running" => (
            vec![
                r#"ssh h1: docker pull 'postgres:17'"#,
                r#"ssh h1: docker stop -t '30' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'pg' -v 'pgdata:/v' 'postgres:17'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
            ],
            None,
        ),
        "accessory_upgrade_new_image_exits" => (
            vec![
                r#"ssh h1: docker pull 'postgres:17'"#,
                r#"ssh h1: docker stop -t '30' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
                r#"ssh h1: docker rm -f 'pg'"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'pg' 'postgres:17'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'pg'"#,
            ],
            Some(
                r#"Runtime error: Accessory pg on h1 exited immediately after starting — check its logs (`docker logs pg` on h1, e.g. via `nrg ssh h1`) for why."#,
            ),
        ),
        "accessory_upgrade_pull_fails" => (
            vec![r#"ssh h1: docker pull 'postgres:17'"#],
            Some(
                r#"Runtime error: docker pull on h1 failed:
manifest unknown"#,
            ),
        ),
        "deploy_fleet_of_two" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h2: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h2: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h2: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h2: docker pull 'caddy:2'"#,
                r#"ssh h2: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h2: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h2: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h2: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h2: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h2: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"ssh h2: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h2: nc -z localhost 13000"#,
                r#"ssh h2: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h2: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h2: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"local: date -u '+%Y-%m-%d %H:%M:%S UTC'"#,
                r#"ssh h1: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h1: docker rename 'app-web-v9-13000' 'app-web'"#,
                r#"ssh h1: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h1: docker rm -f 'app-web-old'"#,
                r#"ssh h2: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h2: docker rename 'app-web-v9-13000' 'app-web'"#,
                r#"ssh h2: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h2: docker rm -f 'app-web-old'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
                r#"ssh h2: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            None,
        ),
        "deploy_fleet_second_host_unhealthy" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h2: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h2: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h2: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h2: docker pull 'caddy:2'"#,
                r#"ssh h2: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h2: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h2: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h2: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h2: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h2: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"ssh h2: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h2: nc -z localhost 13000"#,
                r#"ssh h2: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h2: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h2: docker rm -f 'app-web-v9-13000'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:3000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:3000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"ssh h1: docker rm -f 'app-web-v9-13000'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
                r#"ssh h2: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: Health check failed on h2 after 1 attempts: http://localhost:13000/up (last status: 0, needed 1 consecutive pass(es))"#,
            ),
        ),
        "deploy_health_gate_fails_single_host" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: docker rm -f 'app-web-v9-13000'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: Health check failed on h1 after 1 attempts: http://localhost:13000/up (last status: 503, needed 1 consecutive pass(es))"#,
            ),
        ),
        "deploy_port_exhaustion" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: nc -z localhost 13001"#,
                r#"ssh h1: nc -z localhost 13002"#,
                r#"ssh h1: nc -z localhost 13003"#,
                r#"ssh h1: nc -z localhost 13004"#,
                r#"ssh h1: nc -z localhost 13005"#,
                r#"ssh h1: nc -z localhost 13006"#,
                r#"ssh h1: nc -z localhost 13007"#,
                r#"ssh h1: nc -z localhost 13008"#,
                r#"ssh h1: nc -z localhost 13009"#,
                r#"ssh h1: nc -z localhost 13010"#,
                r#"ssh h1: nc -z localhost 13011"#,
                r#"ssh h1: nc -z localhost 13012"#,
                r#"ssh h1: nc -z localhost 13013"#,
                r#"ssh h1: nc -z localhost 13014"#,
                r#"ssh h1: nc -z localhost 13015"#,
                r#"ssh h1: nc -z localhost 13016"#,
                r#"ssh h1: nc -z localhost 13017"#,
                r#"ssh h1: nc -z localhost 13018"#,
                r#"ssh h1: nc -z localhost 13019"#,
                r#"ssh h1: nc -z localhost 13020"#,
                r#"ssh h1: nc -z localhost 13021"#,
                r#"ssh h1: nc -z localhost 13022"#,
                r#"ssh h1: nc -z localhost 13023"#,
                r#"ssh h1: nc -z localhost 13024"#,
                r#"ssh h1: nc -z localhost 13025"#,
                r#"ssh h1: nc -z localhost 13026"#,
                r#"ssh h1: nc -z localhost 13027"#,
                r#"ssh h1: nc -z localhost 13028"#,
                r#"ssh h1: nc -z localhost 13029"#,
                r#"ssh h1: nc -z localhost 13030"#,
                r#"ssh h1: nc -z localhost 13031"#,
                r#"ssh h1: nc -z localhost 13032"#,
                r#"ssh h1: nc -z localhost 13033"#,
                r#"ssh h1: nc -z localhost 13034"#,
                r#"ssh h1: nc -z localhost 13035"#,
                r#"ssh h1: nc -z localhost 13036"#,
                r#"ssh h1: nc -z localhost 13037"#,
                r#"ssh h1: nc -z localhost 13038"#,
                r#"ssh h1: nc -z localhost 13039"#,
                r#"ssh h1: nc -z localhost 13040"#,
                r#"ssh h1: nc -z localhost 13041"#,
                r#"ssh h1: nc -z localhost 13042"#,
                r#"ssh h1: nc -z localhost 13043"#,
                r#"ssh h1: nc -z localhost 13044"#,
                r#"ssh h1: nc -z localhost 13045"#,
                r#"ssh h1: nc -z localhost 13046"#,
                r#"ssh h1: nc -z localhost 13047"#,
                r#"ssh h1: nc -z localhost 13048"#,
                r#"ssh h1: nc -z localhost 13049"#,
                r#"ssh h1: nc -z localhost 13050"#,
                r#"ssh h1: nc -z localhost 13051"#,
                r#"ssh h1: nc -z localhost 13052"#,
                r#"ssh h1: nc -z localhost 13053"#,
                r#"ssh h1: nc -z localhost 13054"#,
                r#"ssh h1: nc -z localhost 13055"#,
                r#"ssh h1: nc -z localhost 13056"#,
                r#"ssh h1: nc -z localhost 13057"#,
                r#"ssh h1: nc -z localhost 13058"#,
                r#"ssh h1: nc -z localhost 13059"#,
                r#"ssh h1: nc -z localhost 13060"#,
                r#"ssh h1: nc -z localhost 13061"#,
                r#"ssh h1: nc -z localhost 13062"#,
                r#"ssh h1: nc -z localhost 13063"#,
                r#"ssh h1: nc -z localhost 13064"#,
                r#"ssh h1: nc -z localhost 13065"#,
                r#"ssh h1: nc -z localhost 13066"#,
                r#"ssh h1: nc -z localhost 13067"#,
                r#"ssh h1: nc -z localhost 13068"#,
                r#"ssh h1: nc -z localhost 13069"#,
                r#"ssh h1: nc -z localhost 13070"#,
                r#"ssh h1: nc -z localhost 13071"#,
                r#"ssh h1: nc -z localhost 13072"#,
                r#"ssh h1: nc -z localhost 13073"#,
                r#"ssh h1: nc -z localhost 13074"#,
                r#"ssh h1: nc -z localhost 13075"#,
                r#"ssh h1: nc -z localhost 13076"#,
                r#"ssh h1: nc -z localhost 13077"#,
                r#"ssh h1: nc -z localhost 13078"#,
                r#"ssh h1: nc -z localhost 13079"#,
                r#"ssh h1: nc -z localhost 13080"#,
                r#"ssh h1: nc -z localhost 13081"#,
                r#"ssh h1: nc -z localhost 13082"#,
                r#"ssh h1: nc -z localhost 13083"#,
                r#"ssh h1: nc -z localhost 13084"#,
                r#"ssh h1: nc -z localhost 13085"#,
                r#"ssh h1: nc -z localhost 13086"#,
                r#"ssh h1: nc -z localhost 13087"#,
                r#"ssh h1: nc -z localhost 13088"#,
                r#"ssh h1: nc -z localhost 13089"#,
                r#"ssh h1: nc -z localhost 13090"#,
                r#"ssh h1: nc -z localhost 13091"#,
                r#"ssh h1: nc -z localhost 13092"#,
                r#"ssh h1: nc -z localhost 13093"#,
                r#"ssh h1: nc -z localhost 13094"#,
                r#"ssh h1: nc -z localhost 13095"#,
                r#"ssh h1: nc -z localhost 13096"#,
                r#"ssh h1: nc -z localhost 13097"#,
                r#"ssh h1: nc -z localhost 13098"#,
                r#"ssh h1: nc -z localhost 13099"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: no free host port found on h1 in 13000..13100 (all 100 candidates are in use)"#,
            ),
        ),
        "deploy_port_probe_nc_missing" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: port-scan probe failed for h1:13000 (exit 127 — `nc` not found on h1; install netcat, or avoid automatic port selection by setting the port explicitly): nc: not found"#,
            ),
        ),
        "deploy_single_host" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"local: date -u '+%Y-%m-%d %H:%M:%S UTC'"#,
                r#"ssh h1: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h1: docker rename 'app-web-v9-13000' 'app-web'"#,
                r#"ssh h1: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h1: docker rm -f 'app-web-old'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            None,
        ),
        "deploy_with_options" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: echo hi"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#,
                r#"ssh-stdin h1: umask 077; dest='/tmp/.nrg-env-abc/env'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 4 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< A=1
"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '13000:3000' --env-file '/tmp/.nrg-env-abc/env' -v '/d:/v' --network 'n' 'ghcr.io/org/app:v9'; rc=$?; rm -rf '/tmp/.nrg-env-abc'; exit $rc"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/ready'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","match":[{"host":["app.example.com"]}],"handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/ready","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","match":[{"host":["app.example.com"]}],"handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/ready","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"local: date -u '+%Y-%m-%d %H:%M:%S UTC'"#,
                r#"ssh h1: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h1: docker rename 'app-web-v9-13000' 'app-web'"#,
                r#"ssh h1: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h1: docker rm -f 'app-web-old'"#,
                r#"ssh h1: docker images 'ghcr.io/org/app' --format '{{.Tag}}|{{.CreatedAt}}'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            None,
        ),
        "deploy_with_recorded_previous_port" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v9-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"local: date -u '+%Y-%m-%d %H:%M:%S UTC'"#,
                r#"ssh h1: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h1: docker rename 'app-web-v9-13000' 'app-web'"#,
                r#"ssh h1: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h1: docker rm -f 'app-web-old'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            None,
        ),
        "docker_container_running_absent" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(r#"Runtime error: running=false"#),
        ),
        "docker_container_running_cli_missing" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(
                r#"Runtime error: container probe failed for web on h1 (exit 127 — command not found; is the container runtime installed on this host?): sh: docker: not found"#,
            ),
        ),
        "docker_container_running_false" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(r#"Runtime error: running=false"#),
        ),
        "docker_container_running_other_failure" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(
                r#"Runtime error: container probe failed for web on h1 (exit 1): permission denied"#,
            ),
        ),
        "docker_container_running_signal_killed" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(r#"Runtime error: container probe failed for web on h1 (exit 137): "#),
        ),
        "docker_container_running_spawn_failure" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(
                r#"Runtime error: container probe failed for web on h1 (no real exit code — the probe process itself failed to run or was rejected before running): No such file or directory"#,
            ),
        ),
        "docker_container_running_ssh_down" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(
                r#"Runtime error: container probe failed for web on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "docker_container_running_true" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'web'"#],
            Some(r#"Runtime error: running=true"#),
        ),
        "docker_image_id_absent" => (
            vec![r#"ssh h1: docker image inspect -f '{{.Id}}' 'img:1'"#],
            Some(r#"Runtime error: id=[]"#),
        ),
        "docker_image_id_found" => (
            vec![r#"ssh h1: docker image inspect -f '{{.Id}}' 'img:1'"#],
            Some(r#"Runtime error: id=[sha256:abc]"#),
        ),
        "docker_image_id_other_failure" => (
            vec![r#"ssh h1: docker image inspect -f '{{.Id}}' 'img:1'"#],
            Some(
                r#"Runtime error: container probe failed for image img:1 on h1 (exit 1): permission denied"#,
            ),
        ),
        "docker_image_id_podman_absent" => (
            vec![r#"ssh h1: docker image inspect -f '{{.Id}}' 'img:1'"#],
            Some(r#"Runtime error: id=[]"#),
        ),
        "docker_image_id_ssh_down" => (
            vec![r#"ssh h1: docker image inspect -f '{{.Id}}' 'img:1'"#],
            Some(
                r#"Runtime error: container probe failed for image img:1 on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "docker_misc_commands" => (
            vec![
                r#"ssh h1: docker restart 'web'"#,
                r#"ssh h1: docker exec 'web' ls /"#,
                r#"ssh h1: docker logs --tail '100' 'web' 2>&1"#,
                r#"ssh h1: docker logs --tail '5' 'web' 2>&1"#,
                r#"ssh h1: docker container prune -f 2>/dev/null || true"#,
                r#"ssh h1: docker image prune -f 2>/dev/null || true"#,
            ],
            None,
        ),
        "docker_network_create_all_stops_at_first_failure" => (
            vec![
                r#"ssh h1: docker network create 'n'"#,
                r#"ssh h2: docker network create 'n'"#,
            ],
            Some(
                r#"Runtime error: Failed to create network n on h2:
daemon down"#,
            ),
        ),
        "docker_network_exists" => (
            vec![r#"ssh h1: docker network create 'n'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_network_ok" => (
            vec![r#"ssh h1: docker network create 'n'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_network_other_failure" => (
            vec![r#"ssh h1: docker network create 'n'"#],
            Some(r#"Runtime error: ok=false code=1"#),
        ),
        "docker_network_transport_failure" => (
            vec![r#"ssh h1: docker network create 'n'"#],
            Some(r#"Runtime error: ok=false code=-1"#),
        ),
        "docker_podman_runtime_commands" => (
            vec![
                r#"ssh h1: podman run -d --restart unless-stopped --name 'web' 'img:1'"#,
                r#"ssh h1: podman stop -t '30' 'web'"#,
                r#"ssh h1: podman inspect -f '{{.State.Running}}' 'web'"#,
                r#"ssh h1: podman image inspect -f '{{.Id}}' 'img:1'"#,
            ],
            Some(r#"Runtime error: id=[]"#),
        ),
        "docker_promote_absent_is_failure" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=false code=1"#),
        ),
        "docker_promote_ok" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_pull_all_names_failed_hosts" => (
            vec![
                r#"ssh h1: docker pull 'img:1'"#,
                r#"ssh h2: docker pull 'img:1'"#,
            ],
            Some(r#"Runtime error: docker pull failed on: h2"#),
        ),
        "docker_pull_fails" => (
            vec![r#"ssh h1: docker pull 'img:1'"#],
            Some(
                r#"Runtime error: docker pull on h1 failed:
denied"#,
            ),
        ),
        "docker_pull_push_ok" => (
            vec![
                r#"ssh h1: docker pull 'img:1'"#,
                r#"ssh h1: docker push 'img:1'"#,
                r#"ssh h1: docker pull 'img:1'"#,
                r#"ssh h2: docker pull 'img:1'"#,
            ],
            None,
        ),
        "docker_push_fails" => (
            vec![r#"ssh h1: docker push 'img:1'"#],
            Some(
                r#"Runtime error: docker push failed on h1:
denied"#,
            ),
        ),
        "docker_remove_absent" => (
            vec![r#"ssh h1: docker rm -f 'web'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_remove_ok" => (
            vec![r#"ssh h1: docker rm -f 'web'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_remove_other_failure" => (
            vec![r#"ssh h1: docker rm -f 'web'"#],
            Some(r#"Runtime error: ok=false code=1"#),
        ),
        "docker_rename_absent_is_ok" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_rename_conflict_is_failure" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=false code=1"#),
        ),
        "docker_rename_ok" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_rename_podman_absent_is_ok" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_rename_transport_failure" => (
            vec![r#"ssh h1: docker rename 'a' 'b'"#],
            Some(r#"Runtime error: ok=false code=-1"#),
        ),
        "docker_restart_absent_is_failure" => (
            vec![r#"ssh h1: docker restart 'web'"#],
            Some(r#"Runtime error: ok=false err=no such container"#),
        ),
        "docker_run_all_options" => (
            vec![
                r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#,
                r#"ssh-stdin h1: umask 077; dest='/tmp/.nrg-env-abc/env'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 4 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< A=1
"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'web' -p '127.0.0.1:8080:80' --env-file '/tmp/.nrg-env-abc/env' -v '/data:/d' --network 'net1' --cpus 2 'img:1'; rc=$?; rm -rf '/tmp/.nrg-env-abc'; exit $rc"#,
            ],
            Some(r#"Runtime error: ok=true"#),
        ),
        "docker_run_env_newline_rejected" => (
            vec![],
            Some(
                r#"Runtime error: docker env var A's value contains a newline, which would inject extra lines into the env-file — e.g. a PEM-encoded key can't be passed as a single env value this way. Base64-encode it instead."#,
            ),
        ),
        "docker_run_env_write_fails" => (
            vec![
                r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#,
                r#"ssh-stdin h1: umask 077; dest='/tmp/.nrg-env-abc/env'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 4 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< A=1
"#,
                r#"ssh h1: rm -rf '/tmp/.nrg-env-abc'"#,
            ],
            Some(
                r#"Runtime error: Failed to write env-file on h1 (/tmp/.nrg-env-abc/env):
write_remote failed (payload output suppressed)"#,
            ),
        ),
        "docker_run_mktemp_empty" => (
            vec![r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#],
            Some(r#"Runtime error: Remote mktemp returned no directory"#),
        ),
        "docker_run_mktemp_fails" => (
            vec![r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#],
            Some(r#"Runtime error: Cannot allocate private env directory: mktemp: denied"#),
        ),
        "docker_run_once_env" => (
            vec![
                r#"ssh h1: mktemp -d /tmp/.nrg-env-XXXXXXXXXX"#,
                r#"ssh-stdin h1: umask 077; dest='/tmp/.nrg-env-abc/env'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 4 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< K=v
"#,
                r#"ssh h1: docker run --rm --env-file '/tmp/.nrg-env-abc/env' 'img:1' bin/migrate; rc=$?; rm -rf '/tmp/.nrg-env-abc'; exit $rc"#,
            ],
            Some(r#"Runtime error: ok=true"#),
        ),
        "docker_run_once_plain" => (
            vec![r#"ssh h1: docker run --rm --network 'n' 'img:1' bin/migrate"#],
            Some(r#"Runtime error: ok=true"#),
        ),
        "docker_run_plain" => (
            vec![r#"ssh h1: docker run -d --restart unless-stopped --name 'web' 'img:1'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_stop_absent" => (
            vec![r#"ssh h1: docker stop -t '30' 'web'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_stop_ok" => (
            vec![r#"ssh h1: docker stop -t '30' 'web'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "docker_stop_other_failure" => (
            vec![r#"ssh h1: docker stop -t '30' 'web'"#],
            Some(r#"Runtime error: ok=false code=1"#),
        ),
        "docker_stop_ssh_failure" => (
            vec![r#"ssh h1: docker stop -t '30' 'web'"#],
            Some(r#"Runtime error: ok=false code=255"#),
        ),
        "docker_stop_timeout_cfg" => (
            vec![r#"ssh h1: docker stop -t '5' 'web'"#],
            Some(r#"Runtime error: ok=true code=0"#),
        ),
        "proxy_boot_admin_never_ready" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
            ],
            Some(r#"Runtime error: Caddy admin API did not become ready: refused"#),
        ),
        "proxy_boot_already_running" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
            ],
            None,
        ),
        "proxy_boot_fresh" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2.8.4'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2.8.4' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
            ],
            None,
        ),
        "proxy_boot_probe_failure" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#],
            Some(
                r#"Runtime error: container probe failed for caddy on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "proxy_boot_pull_fails" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
            ],
            Some(
                r#"Runtime error: Failed to pull caddy:2 on h1:
toomanyrequests"#,
            ),
        ),
        "proxy_deploy_fails" => (
            vec![
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}]}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}]}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
            ],
            Some(
                r#"Runtime error: Caddy deploy failed on h1 for app:
Caddy PATCH failed with HTTP 500"#,
            ),
        ),
        "proxy_deploy_with_domain" => (
            vec![
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","match":[{"host":["app.example.com"]}],"handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","match":[{"host":["app.example.com"]}],"handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
            ],
            Some(r#"Runtime error: ok=true"#),
        ),
        "rollback_to_previous_image" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v7'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: docker pull 'caddy:2'"#,
                r#"ssh h1: docker rm -f caddy 2>/dev/null || true"#,
                r#"ssh h1: install -d -m 700 /etc/caddy"#,
                r#"ssh-stdin h1: umask 077; dest='/etc/caddy/caddy.json'; [ ! -L "$dest" ] && [ ! -d "$dest" ] || exit 1; tmp=$(mktemp "${dest}.XXXXXXXXXX") || exit 1; trap 'rm -f "$tmp"' 0; trap 'exit 1' HUP INT TERM; cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq 106 ] && chmod 600 "$tmp" && mv -f "$tmp" "$dest" <<< {"admin":{"listen":"localhost:2019"},"apps":{"http":{"servers":{"srv0":{"listen":[":443"],"routes":[]}}}}}"#,
                r#"ssh h1: docker run -d --name caddy --restart unless-stopped --network host -v caddy-data:/data -v caddy-config:/config -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro 'caddy:2' caddy run --resume --config /etc/caddy/caddy.json"#,
                r#"ssh h1: i=0; until curl --max-time 2 -fsS http://localhost:2019/config/ >/dev/null; do i=$((i+1)); [ $i -lt 30 ] || exit 1; sleep 1; done"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: nc -z localhost 13000"#,
                r#"ssh h1: docker run -d --restart unless-stopped --name 'app-web-v7-13000' -p '127.0.0.1:13000:3000' 'ghcr.io/org/app:v7'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:13000/up'"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH 'http://localhost:2019/id/app' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}'); case "$code" in 404) curl -fsS -X POST 'http://localhost:2019/config/apps/http/servers/srv0/routes' -H 'Content-Type: application/json' -d '{"@id":"app","handle":[{"handler":"reverse_proxy","upstreams":[{"dial":"localhost:13000"}],"health_checks":{"active":{"uri":"/up","interval":"10s","timeout":"5s"}}}]}' ;; 2??) : ;; *) echo "Caddy PATCH failed with HTTP $code" >&2; exit 1 ;; esac"#,
                r#"local: date -u '+%Y-%m-%d %H:%M:%S UTC'"#,
                r#"ssh h1: docker rename 'app-web' 'app-web-old'"#,
                r#"ssh h1: docker rename 'app-web-v7-13000' 'app-web'"#,
                r#"ssh h1: docker stop -t '30' 'app-web-old'"#,
                r#"ssh h1: docker rm -f 'app-web-old'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            None,
        ),
        "wait_container_healthy_absent" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
            ],
            Some(r#"Runtime error: Container db not healthy on h1 after 2 attempts"#),
        ),
        "wait_container_healthy_exhausts" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
            ],
            Some(r#"Runtime error: Container db not healthy on h1 after 2 attempts"#),
        ),
        "wait_container_healthy_on_third_try" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
                r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#,
            ],
            Some(r#"Runtime error: r=true"#),
        ),
        "wait_container_healthy_probe_failure" => (
            vec![r#"ssh h1: docker inspect -f '{{.State.Health.Status}}' 'db'"#],
            Some(
                r#"Runtime error: container probe failed for db on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "wait_healthy_on_host_consecutive" => (
            vec![
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:3000/up'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:3000/up'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:3000/up'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:3000/up'"#,
            ],
            None,
        ),
        "wait_healthy_on_host_exhausts" => (
            vec![
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '7' 'http://localhost:3000/ready'"#,
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '7' 'http://localhost:3000/ready'"#,
            ],
            Some(
                r#"Runtime error: Health check failed on h1 after 2 attempts: http://localhost:3000/ready (last status: 0, needed 1 consecutive pass(es))"#,
            ),
        ),
        "wait_port_exhausts" => (
            vec![
                r#"ssh h1: nc -z localhost 5432"#,
                r#"ssh h1: nc -z localhost 5432"#,
                r#"ssh h1: nc -z localhost 5432"#,
            ],
            Some(r#"Runtime error: Port 5432 not open on h1 after 3 attempts"#),
        ),
        "wait_port_nc_missing" => (
            vec![r#"ssh h1: nc -z localhost 5432"#],
            Some(
                r#"Runtime error: port-scan probe failed for h1:5432 (exit 127 — `nc` not found on h1; install netcat, or avoid automatic port selection by setting the port explicitly): nc: not found"#,
            ),
        ),
        "wait_port_opens_on_third_try" => (
            vec![
                r#"ssh h1: nc -z localhost 5432"#,
                r#"ssh h1: nc -z localhost 5432"#,
                r#"ssh h1: nc -z localhost 5432"#,
            ],
            Some(r#"Runtime error: r=true"#),
        ),
        "wait_port_rejects_zero_attempts" => (
            vec![],
            Some(r#"Runtime error: wait_port: cfg.attempts must be >= 1 (got 0)."#),
        ),
        "wait_port_signal_killed" => (
            vec![r#"ssh h1: nc -z localhost 5432"#],
            Some(
                r#"Runtime error: port-scan probe failed for h1:5432 (exit 137 — the probe process was killed by a signal before it could report a real result, not a port-scan answer): "#,
            ),
        ),
        "wait_port_ssh_down" => (
            vec![r#"ssh h1: nc -z localhost 5432"#],
            Some(
                r#"Runtime error: port-scan probe failed for h1:5432 (exit 255 — ssh itself failed to reach h1, not a port-scan result): ssh: connect refused"#,
            ),
        ),
        "deploy_old_container_without_state" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: Cannot determine app's previous proxy target on h1: a container named 'app-web' is already running there, but no 'app.target.h1' or 'app.port.h1' state is recorded (fresh runner? state.json reset or not shared with this host?) — guessing would risk pointing a rollback's restored proxy target at nothing while still removing the new container. Record the real host port before retrying: state_set("app.port.h1", <port>)."#,
            ),
        ),
        "deploy_old_container_probe_fails" => (
            vec![
                r#"ssh h1: umask 077; mkdir '/tmp/nrg-deploy-lock-app' && printf '%s' '.tmpXXXXXX' > '/tmp/nrg-deploy-lock-app/holder'"#,
                r#"ssh h1: docker pull 'ghcr.io/org/app:v9'"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'app-web'"#,
                r#"ssh h1: if [ "$(cat '/tmp/nrg-deploy-lock-app/holder' 2>/dev/null)" = '.tmpXXXXXX' ]; then rm -f '/tmp/nrg-deploy-lock-app/holder' && rmdir '/tmp/nrg-deploy-lock-app'; fi"#,
            ],
            Some(
                r#"Runtime error: container probe failed for app-web on h1 (exit 255): ssh: connect refused"#,
            ),
        ),
        "docker_build_and_local_push_ok" => (
            vec![
                r#"local: uname -s 2>/dev/null"#,
                r#"local: docker build -t 'img:1' -f 'Dockerfile'  '.'"#,
                r#"local: docker push 'img:1'"#,
            ],
            None,
        ),
        "docker_build_fails" => (
            vec![
                r#"local: uname -s 2>/dev/null"#,
                r#"local: docker build -t 'img:1' -f 'Dockerfile'  '.'"#,
            ],
            Some(
                r#"Runtime error: docker build failed:
no space"#,
            ),
        ),
        "docker_local_push_fails" => (
            vec![
                r#"local: uname -s 2>/dev/null"#,
                r#"local: docker push 'img:1'"#,
            ],
            Some(
                r#"Runtime error: docker push failed:
denied"#,
            ),
        ),
        "docker_prune_old_images_removes_older" => (
            vec![
                r#"ssh h1: docker images 'app' --format '{{.Tag}}|{{.CreatedAt}}'"#,
                r#"ssh h1: docker rmi 'app:v2' 2>/dev/null || true"#,
                r#"ssh h1: docker rmi 'app:v3' 2>/dev/null || true"#,
            ],
            Some(r#"Runtime error: ok=true removed=["app:v2", "app:v3"]"#),
        ),
        "docker_prune_old_images_list_fails" => (
            vec![r#"ssh h1: docker images 'app' --format '{{.Tag}}|{{.CreatedAt}}'"#],
            Some(r#"Runtime error: ok=false err=ssh: connect refused"#),
        ),
        "proxy_stop_and_remove" => (
            vec![
                r#"ssh h1: docker stop caddy 2>/dev/null || true"#,
                r#"ssh h1: docker rm caddy 2>/dev/null || true"#,
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE 'http://localhost:2019/id/app'); case "$code" in 404) : ;; 2??) : ;; *) echo "Caddy DELETE failed with HTTP $code" >&2; exit 1 ;; esac"#,
            ],
            None,
        ),
        "proxy_remove_fails" => (
            vec![
                r#"ssh h1: code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE 'http://localhost:2019/id/app'); case "$code" in 404) : ;; 2??) : ;; *) echo "Caddy DELETE failed with HTTP $code" >&2; exit 1 ;; esac"#,
            ],
            Some(r#"Runtime error: ok=false err=Caddy DELETE failed with HTTP 500"#),
        ),
        "proxy_boot_all_running" => (
            vec![
                r#"ssh h1: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h1: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
                r#"ssh h2: docker inspect -f '{{.State.Running}}' 'caddy'"#,
                r#"ssh h2: curl --max-time 10 -fsS -X PATCH -H 'Content-Type: application/json' -d '[":443"]' http://localhost:2019/config/apps/http/servers/srv0/listen"#,
            ],
            None,
        ),
        "wait_healthy_all_ok" => (
            vec![
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:8080/up'"#,
                r#"ssh h2: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:8080/up'"#,
            ],
            None,
        ),
        "wait_healthy_all_first_host_unhealthy" => (
            vec![
                r#"ssh h1: curl -s -o /dev/null -w '%{http_code}' --max-time '30' 'http://localhost:8080/up'"#,
            ],
            Some(
                r#"Runtime error: Health check failed on h1 after 1 attempts: http://localhost:8080/up (last status: 503, needed 1 consecutive pass(es))"#,
            ),
        ),
        _ => panic!("no pinned expectation for {name}"),
    }
}
