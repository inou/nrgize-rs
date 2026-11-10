//! hooks tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn run_post_deploy_hook_reports_failed_hosts_but_does_not_throw() {
    // deploy()'s post-deploy hook used to discard ssh_exec's result
    // entirely — a hook that failed on some hosts still reported the whole deploy as a full
    // success. run_post_deploy_hook is best-effort BY DESIGN (it runs after the fleet has
    // already committed, so nothing here can roll anything back) — it must not throw, but it
    // must return exactly which host(s) failed and why, instead of silently swallowing it.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           let failed = deploy::run_post_deploy_hook(["host1", "host2"], "bin/rails runner Cache.warm");
           state_set("failed.len", "" + failed.len());
           state_set("failed.0", failed[0]);"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("host2", "Cache.warm", 1, "boom: cache backend unreachable");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(state.get("failed.len").as_deref(), Some("1"));
    let msg = state.get("failed.0").unwrap();
    assert!(msg.contains("host2"), "got: {msg}");
    assert!(
        msg.contains("boom: cache backend unreachable"),
        "got: {msg}"
    );

    // BOTH hosts must still have been attempted — a failure on host2 must not stop host1 (or
    // vice versa if ordering were reversed): this is what "best-effort" means.
    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("host1")),
        "host1 must still have been attempted: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("host2")),
        "host2 must still have been attempted: {calls:?}"
    );
}

#[test]
fn run_post_deploy_hook_returns_empty_when_every_host_succeeds() {
    // Companion regression check: the ordinary case (every host succeeds) must return an
    // empty failure list, not spuriously report a failure.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           let failed = deploy::run_post_deploy_hook(["host1", "host2"], "bin/rails runner Cache.warm");
           state_set("failed.len", "" + failed.len());"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    assert_eq!(
        ctx.state.lock().unwrap().get("failed.len").as_deref(),
        Some("0")
    );
}
