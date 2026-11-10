//! Dry-run contract tests: a dry run runs reads and plans mutations; nothing carries over from a
//! planned mutation to a later read; waits are assumed to pass; a probe that fails to run is
//! noted in a dry run and thrown in a live one.

use super::super::*;
use crate::engine::context::{shared, shared_dry};
use crate::engine::runner::FakeRunner;
use std::fs;

/// Run `script` as a dry run against `fake`; returns the plan details.
fn dry_run(fake: &Arc<FakeRunner>, script: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(&main, script).unwrap();
    let ctx = shared_dry(fake.clone());
    run_file(&main, ctx.clone()).unwrap_or_else(|e| panic!("dry run failed: {e}"));
    let plan = ctx.plan.lock().unwrap();
    plan.iter().map(|a| a.detail.clone()).collect()
}

/// The reads a dry run may run for real.
const READ_ALLOWLIST: [&str; 4] = ["inspect", "nc -z", "uname", "readlink"];

fn assert_only_reads(calls: &[String], what: &str) {
    for c in calls {
        assert!(
            READ_ALLOWLIST.iter().any(|r| c.contains(r)),
            "{what}: a dry run ran a non-read command on a host: {c}\nall calls: {calls:?}"
        );
    }
}

#[test]
fn dry_running_deploy_runs_only_reads() {
    let fake = FakeRunner::shared();
    let plan = dry_run(
        &fake,
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );
    assert!(!plan.is_empty());
    assert_only_reads(&fake.calls(), "deploy");
}

#[test]
fn dry_running_accessories_runs_only_reads() {
    let fake = FakeRunner::shared();
    dry_run(
        &fake,
        r#"
        import "std/deploy" as deploy;
        deploy::accessory_run("host1", "db", "postgres:16");
        deploy::accessory_stop("host1", "db");
        deploy::accessory_restart("host1", "db");
        deploy::accessory_upgrade("host1", "db", "postgres:17");
    "#,
    );
    assert_only_reads(&fake.calls(), "accessory_*");
}

#[test]
fn dry_running_release_runs_only_reads() {
    let fake = FakeRunner::shared();
    dry_run(
        &fake,
        r#"
        import "std/release" as r;
        r::deploy("host1", "/srv/app", "v1", #{prepare: "true", activate: "true", health: "true", keep: 2});
        r::rollback("host1", "/srv/app", "v0", #{activate: "true", health: "true"});
    "#,
    );
    assert_only_reads(&fake.calls(), "release");
}

#[test]
fn dry_running_docker_wrappers_runs_only_reads() {
    let fake = FakeRunner::shared();
    dry_run(
        &fake,
        r#"
        import "std/docker" as docker;
        docker::docker_pull("host1", "app:v1");
        docker::docker_run("host1", "app:v1", "x", #{ports: #{"8080": "80"}, envs: #{A: "b"}});
        docker::docker_run_once("host1", "app:v1", "true", #{});
        docker::docker_stop("host1", "x");
        docker::docker_remove("host1", "x");
        docker::docker_rename("host1", "x", "y");
        docker::docker_restart("host1", "y");
        docker::docker_promote("host1", "y", "z");
        docker::docker_network_create("host1", "net");
        docker::docker_exec("host1", "z", "ls");
        docker::docker_cleanup("host1");
        docker::docker_prune_old_images("host1", "app", 2, ["v1"]);
        docker::docker_container_running("host1", "z");
        docker::docker_image_id("host1", "app:v1");
    "#,
    );
    assert_only_reads(&fake.calls(), "docker_*");
}

#[test]
fn a_planned_mutation_does_not_change_what_a_later_read_returns() {
    // The runner says the container is absent; a planned docker_run does not make it running.
    let fake = FakeRunner::shared();
    fake.fail_cmd("host1", "inspect", 1, "Error: No such container: x");
    let plan = dry_run(
        &fake,
        r#"
        import "std/docker" as docker;
        docker::docker_run("host1", "app:v1", "x");
        if container_running("host1", "x") { throw "read saw the planned run"; }
        if docker::docker_container_running("host1", "x") { throw "read saw the planned run"; }
    "#,
    );
    assert!(plan.iter().any(|d| d.contains("run -d")), "{plan:?}");
}

#[test]
fn every_wait_returns_in_a_dry_run_without_touching_a_host() {
    let fake = FakeRunner::shared();
    dry_run(
        &fake,
        r#"
        import "std/healthcheck" as health;
        let r = health::wait_healthy("http://127.0.0.1:1/up", #{expected_status: 204});
        if type_of(r) != "HttpResponse" || r.status != 204 || !r.ok { throw "bad synthetic response"; }
        health::wait_healthy_on_host("host1", 3000, #{});
        health::wait_healthy_all(["host1", "host2"], 3000, #{});
        health::wait_port("host1", 3000);
        health::wait_container_healthy("host1", "x");
    "#,
    );
    assert!(
        fake.calls().is_empty(),
        "waits must not run anything: {:?}",
        fake.calls()
    );
}

#[test]
fn wait_arguments_are_validated_in_a_dry_run_too() {
    let fake = FakeRunner::shared();
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/healthcheck" as health; health::wait_port("host1", 3000, #{attempts: 0});"#,
    )
    .unwrap();
    let err = run_file(&main, shared_dry(fake)).unwrap_err();
    assert!(err.contains("attempts must be >= 1"), "got: {err}");
}

#[test]
fn a_probe_failure_is_noted_in_a_dry_run_and_thrown_in_a_live_run() {
    let script = r#"container_running("host1", "x")"#;
    let unreachable = || {
        let fake = FakeRunner::shared();
        fake.fail_host(
            "host1",
            255,
            "ssh: connect to host host1: Connection refused",
        );
        fake
    };
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        format!("let r = {script}; if r {{ throw \"assumed absent\"; }}"),
    )
    .unwrap();

    let ctx = shared_dry(unreachable());
    run_file(&main, ctx.clone()).unwrap();
    let plan = ctx.plan.lock().unwrap().clone();
    assert!(
        plan.iter().any(|a| a.kind == "check"
            && a.detail.contains("[probe failed — assuming x absent]")
            && a.detail.contains("Connection refused")),
        "{plan:?}"
    );

    let err = run_file(&main, shared(unreachable())).unwrap_err();
    assert!(err.contains("container probe failed"), "got: {err}");
}
