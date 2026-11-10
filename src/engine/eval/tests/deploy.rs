//! deploy tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::{FakeRunner, RawOutput};
use std::fs;

#[test]
fn a_previous_runs_persisted_runtime_choice_does_not_leak_into_a_later_run() {
    // `set_runtime("podman")` used to persist into the DURABLE
    // project state store, so a run that never calls `set_runtime()` at all (e.g. after the
    // Energize.rhai script is edited to drop that line, reverting to the default) would
    // silently keep resolving to whatever a PAST run last persisted — here we prove a
    // second, independent live invocation against the SAME on-disk project root that never
    // calls set_runtime() still resolves lib/runtime.rhai's container_cmd() to "docker",
    // not the stale "podman" a prior run left behind.
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    // Run 1: explicitly configure podman. This should persist to disk (so nrg status/logs/
    // app exec can later recover it) — verified below.
    let main1 = dir.path().join("configure.rhai");
    fs::write(
        &main1,
        r#"import "lib/runtime" as rt; rt::set_runtime("podman");"#,
    )
    .unwrap();
    let store1 = StateStore::load(dir.path()).unwrap();
    let ctx1 = shared_with_state(FakeRunner::shared(), store1, EffectMode::Live);
    run_file(&main1, ctx1).unwrap();

    let persisted = StateStore::load(dir.path()).unwrap();
    assert_eq!(
        persisted.get("nrg.runtime.cmd"),
        Some("podman".to_string()),
        "set_runtime() must still persist to disk for nrg status/logs/app exec's benefit"
    );

    // Run 2: a SEPARATE invocation against the same project root that never calls
    // set_runtime() at all. It must resolve to the true default ("docker"), not the stale
    // "podman" the first run persisted.
    let main2 = dir.path().join("deploy.rhai");
    fs::write(
        &main2,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app", #{});"#,
    )
    .unwrap();
    let fake2 = FakeRunner::shared();
    let store2 = StateStore::load(dir.path()).unwrap();
    let ctx2 = shared_with_state(fake2.clone(), store2, EffectMode::Live);
    run_file(&main2, ctx2).unwrap();

    assert!(
        fake2.calls().iter().any(|c| c.contains("docker run")),
        "must default to docker (ignoring the previous run's persisted podman choice): {:?}",
        fake2.calls()
    );
    assert!(
        !fake2.calls().iter().any(|c| c.contains("podman run")),
        "must NOT pick up the stale persisted runtime: {:?}",
        fake2.calls()
    );
}

#[test]
fn deploy_re_persists_the_actual_runtime_it_used_even_without_set_runtime() {
    // The durable `nrg.runtime.cmd`/`nrg.runtime.name` mirror is only ever WRITTEN by `set_runtime()`/`auto_detect()`. So if a script that
    // once called `set_runtime("podman")` is later edited to drop that call entirely, the
    // NEXT deploy correctly uses docker — but the durable mirror is
    // never told, so it keeps saying "podman" forever, misleading `nrg status`/`nrg logs`/
    // `nrg app exec` about a runtime this service hasn't used since. `deploy()` now
    // re-persists the runtime it actually resolved to on every successful deploy, so the
    // durable copy always reflects the last REAL deploy, not just the last explicit
    // `set_runtime()` call.
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    // Run 1: deploy under podman. The durable mirror should now say "podman".
    let main1 = dir.path().join("deploy_v1.rhai");
    fs::write(
        &main1,
        r#"import "lib/runtime" as rt;
           import "lib/deploy" as deploy;
           rt::set_runtime("podman");
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v1", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });"#,
    )
    .unwrap();
    let fake1 = FakeRunner::shared();
    fake1.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake1.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    let store1 = StateStore::load(dir.path()).unwrap();
    let ctx1 = shared_with_state(fake1.clone(), store1, EffectMode::Live);
    run_file(&main1, ctx1).unwrap();

    assert_eq!(
        StateStore::load(dir.path()).unwrap().get("nrg.runtime.cmd"),
        Some("podman".to_string())
    );

    // Run 2: the script is edited to drop `set_runtime("podman")` entirely (reverting to
    // the default), and deploys again. It must use docker AND the durable
    // mirror must now say "docker" too — not the stale "podman" from run 1.
    let main2 = dir.path().join("deploy_v2.rhai");
    fs::write(
        &main2,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v2", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });"#,
    )
    .unwrap();
    let fake2 = FakeRunner::shared();
    fake2.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake2.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    let store2 = StateStore::load(dir.path()).unwrap();
    let ctx2 = shared_with_state(fake2.clone(), store2, EffectMode::Live);
    run_file(&main2, ctx2).unwrap();

    assert!(
        fake2
            .calls()
            .iter()
            .any(|c| c.contains("docker run") || c.contains("docker pull")),
        "the second deploy must actually use docker: {:?}",
        fake2.calls()
    );
    assert!(
        !fake2.calls().iter().any(|c| c.contains("podman")),
        "the second deploy must not use podman: {:?}",
        fake2.calls()
    );
    assert_eq!(
        StateStore::load(dir.path()).unwrap().get("nrg.runtime.cmd"),
        Some("docker".to_string()),
        "the durable mirror must be re-persisted as docker after a deploy that actually \
         used docker, not left stale at the previous deploy's podman"
    );
}

#[test]
fn deploy_throws_when_old_container_is_running_but_no_port_state_is_recorded() {
    // When neither `<service>.target.<host>` nor `.port.<host>` state
    // exists, deploy_one_host's old_target used to guess "localhost:<container_port>" (the
    // IN-CONTAINER port) unconditionally. That guess is fine for a genuine first deploy (no
    // old container exists at all — nothing to roll back to regardless). But if a canonical
    // OLD container is ACTUALLY running (fresh CI runner, unshared/lost state.json — NOT a
    // real first deploy), sim_pick_port hands out an essentially arbitrary host port, so
    // "localhost:<container_port>" is almost certainly the WRONG port: a later unwind's
    // "restore proxy" compensation would then "succeed" pointing traffic at a target nothing
    // listens on, while the OTHER compensation still tears down the just-started new
    // container — an outage the rollback itself caused, on a host serving traffic just fine
    // before this deploy attempt. This loads the REAL lib/deploy.rhai (symlinked, not
    // reimplemented) via a FakeRunner whose default response reports EVERY inspect probe as
    // "running" (so both Caddy's own is-it-already-up check and the new old-container
    // check both read "true"), and asserts the throw fires before the new container is ever
    // started or a port is even picked.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
           });"#,
    )
    .unwrap();

    let mut fake_inner = FakeRunner::new();
    fake_inner.default = RawOutput {
        stdout: "true".to_string(),
        stderr: String::new(),
        exit_code: 0,
    };
    let fake = Arc::new(fake_inner);
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Cannot determine"), "got: {err}");
    assert!(err.contains("app-web"), "got: {err}");
    assert!(
        !fake
            .calls()
            .iter()
            .any(|c| c.contains("nc -z") || c.contains("run -d --restart")),
        "must not pick a port or start the new container before the old-target guard fires: {:?}",
        fake.calls()
    );
}

#[test]
fn deploy_falls_back_to_the_container_port_when_no_old_container_is_running_either() {
    // Companion regression check: when state is ALSO missing but NO old container is running
    // (a genuine first deploy, or a newly added fleet host), the guard must NOT fire — there's
    // nothing to roll back to regardless, so a guessed target can't make anything worse. Uses
    // the plain default FakeRunner (every command reports exit 0, empty stdout): the
    // inspect-running probe's stdout ("") isn't "true", so it honestly reports "not running",
    // matching a genuine first deploy. The deploy is allowed to fail LATER for an unrelated
    // reason (every `nc -z` probe also reports exit 0 = "port busy", so port-picking
    // exhausts its 100 candidates) — the point here is only that it gets PAST the old-target
    // guard without the fresh-host throw, proven by the distinct "no free host port" error and by
    // `nc -z` calls actually appearing (deliberately NOT letting the whole deploy succeed,
    // since a live health check would otherwise attempt a real, slow HTTP request).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(
        !err.contains("Cannot determine"),
        "the guard must NOT fire here: {err}"
    );
    assert!(
        err.contains("no free host port"),
        "expected to reach port-picking: got: {err}"
    );
    assert!(
        fake.calls().iter().any(|c| c.contains("nc -z")),
        "must have reached port-picking (proving old_target fell through cleanly): {:?}",
        fake.calls()
    );
}

#[test]
fn post_commit_cleanup_failure_skips_persisting_that_hosts_port_and_target() {
    // deploy()'s post-commit loop used to call docker_rename (x2) /
    // docker_stop / docker_remove / docker_cleanup WITHOUT checking any of their results, then
    // unconditionally persisted <service>.port.<host>/.target.<host> as if the rename dance
    // had completed. Each of those commands bakes `|| true` into the REMOTE shell string (by
    // design, so a harmless retry of an already-done step doesn't fail) — but that can't mask
    // an SSH-level failure (dropped connection, auth failure): the remote shell (and its
    // `|| true`) never even runs, so the returned ExecResult correctly reports `ok: false`.
    // Before this fix that signal was discarded; a host whose SSH connection dropped between
    // commit and cleanup got its state blindly overwritten as if the OLD container had been
    // retired and the NEW one promoted, when in reality NEITHER rename ever happened.
    //
    // This loads the REAL lib/deploy.rhai via a FakeRunner, run in LIVE mode so the fix's
    // ExecResult checks actually execute. Getting all the way to post-commit needs the health
    // check to pass; that's an ssh_exec'd curl on the target host itself (not
    // a real HTTP GET from the control machine), so a FakeRunner response is enough — no real
    // HTTP server needed. `sim_pick_port`'s `nc -z` probe is forced (a nonzero exit means "port
    // free") so it settles on the first candidate.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    fake.fail_cmd(
        "127.0.0.1",
        "rename",
        255,
        "ssh: broken pipe, connection reset by peer",
    );

    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert!(
        state.get("app.port.127.0.0.1").as_deref() == Some("13000"),
        "a host whose cleanup failed must NOT get its port persisted as if the swap completed"
    );
    assert!(
        state.get("app.target.127.0.0.1").as_deref() == Some("localhost:13000"),
        "a host whose cleanup failed must NOT get its target persisted as if the swap completed"
    );
    // Service-level state still persists — the fleet-wide traffic switch (inside the
    // transaction, already committed) genuinely succeeded; only this host's post-commit
    // tidying is in question.
    assert_eq!(state.get("app.version").as_deref(), Some("v9"));
    assert_eq!(
        state.get("app.image").as_deref(),
        Some("ghcr.io/org/app:v9")
    );

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("rename")),
        "the rename attempts must still have been made: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("prune")),
        "the REST of that host's cleanup steps must still be attempted (idempotent, try \
         everything, then decide): {calls:?}"
    );
}

#[test]
fn post_commit_cleanup_success_persists_port_and_target_normally() {
    // Companion regression check: when every post-commit command succeeds (the common case),
    // behavior is unchanged from before this fix — port/target ARE persisted.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    // sim_pick_port scans upward from container_port+10000 for the first port a `nc -z` probe
    // reports free (nonzero exit); a blanket `nc -z` failure below makes it settle on the very
    // first candidate, so with container_port: 3000 the picked port is deterministically 13000.
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
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

    let state = ctx.state.lock().unwrap();
    assert_eq!(
        state.get("app.port.127.0.0.1").as_deref(),
        Some("13000"),
        "a fully successful cleanup must still persist the host's new port"
    );
    assert_eq!(
        state.get("app.target.127.0.0.1").as_deref(),
        Some("localhost:13000"),
        "a fully successful cleanup must still persist the host's new target"
    );
}
