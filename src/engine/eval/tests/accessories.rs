//! accessories tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

use super::helpers::*;

#[test]
fn accessory_run_removes_a_stopped_but_present_container_before_starting() {
    // A STOPPED-but-present accessory container used to make
    // `docker run --name` fail with "the container name ... is already in use", wedging every
    // future deploy until an operator manually removed it. This asserts the idempotent `rm -f`
    // now runs BEFORE `docker run` in every case (dry-run plan order is the observable
    // contract, same as the caddy/docker_run tests above).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
    )
    .unwrap();

    let ctx = crate::engine::context::shared_dry(FakeRunner::shared());
    run_file(&main, ctx.clone()).unwrap();
    let plan = ctx.plan.lock().unwrap().clone();
    let rm_pos = plan
        .iter()
        .position(|a| a.detail.contains("rm -f") && a.detail.contains("redis"));
    let run_pos = plan
        .iter()
        .position(|a| a.detail.contains("run -d") && a.detail.contains("redis"));
    assert!(
        rm_pos.is_some() && run_pos.is_some(),
        "missing rm -f or run -d in plan: {plan:?}"
    );
    assert!(
        rm_pos.unwrap() < run_pos.unwrap(),
        "the idempotent rm -f must run BEFORE docker run --name: {plan:?}"
    );
}

#[test]
fn accessory_run_throws_when_the_container_exits_immediately_after_starting() {
    // `docker run -d`'s exit code only reflects that the container
    // STARTED, not that it's still up a moment later. A FakeRunner whose inspect probe ALWAYS
    // reports "not running" simulates a container that starts (docker run -d succeeds, exit 0)
    // and then crashes near-instantly — the SAME "not running" answer is honest both before
    // the start attempt and in the post-start re-check, so this needs no stateful mock.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(
        err.contains("exited immediately after starting"),
        "got: {err}"
    );
    assert!(err.contains("redis"), "got: {err}");
}

#[test]
fn accessory_run_succeeds_when_the_container_is_still_running_after_starting() {
    // Companion regression check: the post-start re-check must NOT spuriously fail a
    // genuinely healthy start — the ordinary case for almost every real accessory.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
    )
    .unwrap();

    let fake = Arc::new(StartsThenStaysUpRunner {
        inspect_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    run_file(&main, shared(fake.clone())).unwrap();
    // Without this, the test would pass VACUOUSLY if the mock ever degenerated
    // to reporting "running" on its FIRST call too — accessory_run would then take the
    // `already running` early return (before ever starting/re-checking anything) and this
    // test's only assertion (no throw) would still hold, for the wrong reason. Asserting
    // exactly 2 probes ran proves the pre-start AND post-start checks both actually executed.
    assert_eq!(
        fake.inspect_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "must probe exactly twice: once before starting (not yet running), once after (still \
         running) — anything else means this test isn't exercising the real start+recheck path"
    );
}
