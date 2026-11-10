//! locks tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn deploy_acquires_and_releases_the_cross_machine_lock_on_success() {
    // A successful deploy must acquire the remote per-service lock on
    // the first host (an atomic `mkdir`) before doing any real work, and release it (`rm -rf`)
    // once the whole deploy has completed.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "std/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("mkdir '/tmp/nrg-deploy-lock-app'")),
        "must acquire the lock before doing any work: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("rmdir '/tmp/nrg-deploy-lock-app'")),
        "must release the lock after a successful deploy: {calls:?}"
    );
    // The lock's mkdir must happen BEFORE any real work (here, the pull).
    let mkdir_idx = calls
        .iter()
        .position(|c| c.contains("mkdir '/tmp/nrg-deploy-lock-app'"));
    let pull_idx = calls.iter().position(|c| c.contains("pull "));
    assert!(
        mkdir_idx.unwrap() < pull_idx.unwrap(),
        "lock must be acquired before the pull: {calls:?}"
    );
}

#[test]
fn deploy_refuses_when_the_lock_is_already_held() {
    // A concurrent deploy of the SAME service (the lock directory
    // already exists on the lock host) must refuse immediately, before any build/push/pull —
    // proving this is a genuine up-front guard, not just an eventually-detected conflict.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/deploy" as deploy;
           deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
               skip_build: true, skip_push: true,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "web1",
        "mkdir",
        1,
        "mkdir: cannot create directory '/tmp/nrg-deploy-lock-app': File exists",
    );
    let ctx = shared(fake.clone());
    let err = run_file(&main, ctx.clone()).unwrap_err();
    assert!(err.contains("could not acquire deploy lock"), "got: {err}");
    assert!(err.contains("File exists"), "got: {err}");

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("pull ")),
        "must refuse before reaching the pull step: {calls:?}"
    );
}

#[test]
fn deploy_releases_the_lock_even_when_a_later_step_fails() {
    // A failure ANYWHERE later in deploy() (here, the pull) must still
    // release the lock before the error propagates — otherwise a single failed deploy would
    // permanently block every future one for this service.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/deploy" as deploy;
           deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
               skip_build: true, skip_push: true,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("web1", "pull", 1, "pull failed: connection reset");
    let ctx = shared(fake.clone());
    let err = run_file(&main, ctx.clone()).unwrap_err();
    assert!(
        err.contains("pull failed") || err.contains("connection reset"),
        "the ORIGINAL pull error must still surface, not be masked by lock release: {err}"
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("rmdir '/tmp/nrg-deploy-lock-app'")),
        "the lock must still be released after a later failure: {calls:?}"
    );
}

#[test]
fn deploy_with_skip_lock_never_touches_the_cross_machine_lock() {
    // cfg.skip_lock is an explicit opt-out (the lock depends on remote infrastructure — a
    // writable /tmp, a POSIX shell — this codebase can't unconditionally guarantee for every
    // exotic host) — must leave the lock completely untouched, no mkdir/rm -rf at all.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "std/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1, skip_lock: true,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("nrg-deploy-lock")),
        "skip_lock: true must never touch the lock at all: {calls:?}"
    );
}
