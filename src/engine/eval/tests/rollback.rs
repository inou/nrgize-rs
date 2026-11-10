//! rollback tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn rollback_happy_path_redeploys_the_previous_image_and_swaps_prev() {
    // rollback() had ZERO tests exercising an actual successful
    // rollback — every existing test only covered a REFUSAL path (nested transaction, empty
    // hosts, a mutable ":latest" snapshot, a rejected keep_images override), each of which
    // throws before deploy() is ever reached. This is the first test that runs rollback()
    // all the way through: deploy v1, deploy v2 (which snapshots .prev = v1 automatically),
    // then roll back with NO cfg (the 2-arg overload, using the snapshotted .prev), and assert
    // the full round trip: the live image/version are back to v1, AND the current-before-
    // rollback image (v2) becomes the NEW .prev, so a second rollback would undo this one.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v1", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v2", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1,
           });
           deploy::rollback(["127.0.0.1"], "app");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(
        state.get("app.image").as_deref(),
        Some("ghcr.io/org/app:v1"),
        "rollback() must actually redeploy the SNAPSHOTTED .prev image"
    );
    assert_eq!(state.get("app.version").as_deref(), Some("v1"));
    assert_eq!(
        state.get("app.prev").as_deref(),
        Some("ghcr.io/org/app:v2"),
        "the image that was live BEFORE the rollback (v2) must become the new rollback \
         target, so a second rollback undoes this one"
    );
    drop(state);

    let calls = fake.calls();
    // >= 2, NOT >= 1: the SETUP's own deploy(v1) already pulls v1 once, so a single v1 pull
    // proves nothing about the rollback (a hollowed-out rollback() that only rewrote
    // .image/.version state without ever calling deploy() still left one v1 pull in the log
    // — caught by mutation testing). The rollback's internal deploy() must
    // add a SECOND v1 pull of its own.
    assert!(
        calls
            .iter()
            .filter(|c| c.contains("pull ") && c.contains("v1"))
            .count()
            >= 2,
        "the rollback's own internal deploy() call must actually pull v1 on the host again \
         (one v1 pull is just the setup deploy's): {calls:?}"
    );
}

#[test]
fn run_rollback_works_with_zero_vendored_lib_via_the_embedded_stdlib() {
    // `run_rollback` used to hard-error without a vendored `lib/deploy.rhai` —
    // the one native command that actually REQUIRED vendoring. It now falls back to the
    // embedded `import "std/deploy"` and works exactly the same as the vendored-lib case
    // (see `run_rollback_falls_back_to_the_snapshotted_prev_image_without_an_override`),
    // with no `lib/` directory present on disk at all.
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap(); // deliberately no `lib/` symlink

    let mut store = StateStore::load(dir.path()).unwrap();
    store.set("app.image", "ghcr.io/org/app:v2").unwrap();
    store.set("app.prev", "ghcr.io/org/app:v1").unwrap();
    let fake = FakeRunner::shared();
    let ctx = shared_with_state(fake.clone(), store, EffectMode::Live);

    let err = run_rollback(dir.path(), &["host1".to_string()], "app", None, ctx).unwrap_err();
    assert!(
        err.contains("no free host port"),
        "expected to reach port-picking: got: {err}"
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("pull 'ghcr.io/org/app:v1'")),
        "must roll back to the snapshotted .prev image (v1) via the embedded stdlib: {calls:?}"
    );
}

#[test]
fn run_rollback_prefers_a_real_vendored_lib_deploy_over_the_embedded_one() {
    // A project that vendored (and customized) lib/deploy.rhai must still have THAT file
    // win — the embedded fallback only applies when nothing is vendored. A trivially
    // "customized" deploy.rhai (just a distinctive throw) proves which one actually ran.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(
        dir.path().join("lib/deploy.rhai"),
        r#"fn rollback(hosts, service, cfg) { throw "CUSTOM VENDORED DEPLOY.RHAI RAN"; }"#,
    )
    .unwrap();

    let err = run_rollback(
        dir.path(),
        &["web1".to_string()],
        "app",
        None,
        shared(FakeRunner::shared()),
    )
    .unwrap_err();
    assert!(
        err.contains("CUSTOM VENDORED DEPLOY.RHAI RAN"),
        "got: {err}"
    );
}

#[test]
fn run_rollback_falls_back_to_the_embedded_stdlib_when_vendoring_is_only_partial() {
    // `lib/deploy.rhai` existing is not enough on its own — ITS OWN
    // internal imports (`lib/docker`, `lib/caddy`, `lib/healthcheck`,
    // `lib/runtime`) resolve independently, via pure on-disk FileModuleResolver (no embedded
    // fallback for "lib/X"). A REAL project's lib/deploy.rhai (copy the actual repo file, not
    // a stub) with `lib/docker.rhai` missing used to hard-fail with a confusing "Module not
    // found: lib/docker" — even though the fully self-contained embedded std/deploy was right
    // there. This must now fall back to it instead, with a clear warning, not a hard error.
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    std::fs::copy(
        repo_lib.join("deploy.rhai"),
        dir.path().join("lib/deploy.rhai"),
    )
    .unwrap();
    // Deliberately do NOT copy lib/docker.rhai (or any of deploy.rhai's other dependencies).

    let mut store = StateStore::load(dir.path()).unwrap();
    store.set("app.image", "ghcr.io/org/app:v2").unwrap();
    store.set("app.prev", "ghcr.io/org/app:v1").unwrap();
    let fake = FakeRunner::shared();
    let ctx = shared_with_state(fake.clone(), store, EffectMode::Live);

    let err = run_rollback(dir.path(), &["host1".to_string()], "app", None, ctx).unwrap_err();
    assert!(
        !err.contains("Module not found"),
        "must not surface the raw module-resolution error to the caller: {err}"
    );
    assert!(
        err.contains("no free host port"),
        "expected to reach port-picking via the embedded stdlib: {err}"
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("pull 'ghcr.io/org/app:v1'")),
        "must still roll back to the snapshotted .prev image via the embedded fallback: {calls:?}"
    );
}

#[test]
fn run_rollback_does_not_silently_swap_in_the_stock_rollback_when_a_customized_deploy_rhai_is_missing_its_own_non_stdlib_dependency(
) {
    // is_module_not_found must NOT retry for just any missing module —
    // only for the embedded stdlib's own lib/X names. A customized lib/deploy.rhai importing
    // some OTHER project-specific module (e.g. "lib/mycustom") that's missing is a real,
    // project-specific error and must surface as such — never silently substitute the stock
    // embedded rollback() (which never went through the customization) for a customized one
    // that never ran.
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(
        dir.path().join("lib/deploy.rhai"),
        r#"
        import "lib/mycustom" as custom;
        fn rollback(hosts, service, cfg) { custom::do_rollback(hosts, service, cfg); }
        "#,
    )
    .unwrap();
    // Deliberately do NOT create lib/mycustom.rhai.

    let mut store = StateStore::load(dir.path()).unwrap();
    store.set("app.image", "ghcr.io/org/app:v2").unwrap();
    store.set("app.prev", "ghcr.io/org/app:v1").unwrap();
    let ctx = shared_with_state(FakeRunner::shared(), store, EffectMode::Live);

    let err = run_rollback(dir.path(), &["host1".to_string()], "app", None, ctx).unwrap_err();
    assert!(
        err.contains("lib/mycustom") || err.contains("Module not found"),
        "the missing custom module must surface as a real error, not get silently \
         swallowed by the embedded-stdlib fallback: {err}"
    );
}

#[test]
fn run_rollback_pulls_the_explicit_image_override_on_every_host() {
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    // No persisted config/prev at all — the --image override alone must be enough to reach
    // deploy(), regardless of any snapshotted rollback target.
    let store = StateStore::load(dir.path()).unwrap();
    let fake = FakeRunner::shared();
    let ctx = shared_with_state(fake.clone(), store, EffectMode::Live);

    let err = run_rollback(
        dir.path(),
        &["host1".to_string(), "host2".to_string()],
        "app",
        Some("ghcr.io/org/app:v5"),
        ctx,
    )
    .unwrap_err();
    // Port-picking exhausts every candidate under the plain default FakeRunner (every
    // `nc -z` reports exit 0 = "port busy") — the same trick this file's own deploy() tests
    // use to prove execution reached a known-later point without a full live health check.
    assert!(
        err.contains("no free host port"),
        "expected to reach port-picking: got: {err}"
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("host1") && c.contains("pull 'ghcr.io/org/app:v5'")),
        "must pull the --image override on host1: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("host2") && c.contains("pull 'ghcr.io/org/app:v5'")),
        "must pull the --image override on host2 too: {calls:?}"
    );
}

#[test]
fn run_rollback_falls_back_to_the_snapshotted_prev_image_without_an_override() {
    use crate::engine::context::{shared_with_state, EffectMode};
    use crate::engine::state::StateStore;

    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    let mut store = StateStore::load(dir.path()).unwrap();
    store.set("app.image", "ghcr.io/org/app:v2").unwrap();
    store.set("app.prev", "ghcr.io/org/app:v1").unwrap();
    let fake = FakeRunner::shared();
    let ctx = shared_with_state(fake.clone(), store, EffectMode::Live);

    let err = run_rollback(dir.path(), &["host1".to_string()], "app", None, ctx).unwrap_err();
    assert!(
        err.contains("no free host port"),
        "expected to reach port-picking: got: {err}"
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("pull 'ghcr.io/org/app:v1'")),
        "must roll back to the snapshotted .prev image (v1), not the current .image (v2): \
         {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("pull 'ghcr.io/org/app:v2'")),
        "must NOT pull the CURRENT image when no override is given: {calls:?}"
    );
}

#[test]
fn run_rollback_surfaces_the_stdlibs_own_empty_hosts_guard() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    let err = run_rollback(
        dir.path(),
        &[],
        "app",
        Some("ghcr.io/org/app:v1"),
        shared(FakeRunner::shared()),
    )
    .unwrap_err();
    // The empty-hosts guard (lib/deploy.rhai's rollback()) — proves the hosts array and service name
    // both actually reach the stdlib call, rather than being silently dropped somewhere in
    // the scope-injection plumbing.
    assert!(err.contains("empty hosts array"), "got: {err}");
    assert!(err.contains("'app'"), "got: {err}");
}

#[test]
fn mid_fleet_failure_unwinds_web1_with_the_real_stdlib_and_never_crosses_hosts() {
    // Issue #27: transaction.rs's mid-fleet tests hand-write the compensation closures, so a
    // capture bug in lib/deploy.rhai's REAL deploy_one_host would slip past them. This runs the
    // real deploy() live against two hosts: web1 rolls fully (container up, health ok, proxy
    // switched), then web2's `docker run` fails. The unwind must restore web1's proxy to web1's
    // OLD target, then `rm -f` web1's NEW container (LIFO), and no compensation may act on the
    // other host's state.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           state_set("app.target.web1", "localhost:21111");
           state_set("app.target.web2", "localhost:22222");
           let failed = false;
           try {
               deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });
           } catch (e) { failed = true; }
           if !failed { throw "deploy must fail when web2's docker run fails"; }"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    for h in ["web1", "web2"] {
        fake.fail_cmd(h, "nc -z", 1, "");
        fake.respond_cmd(h, "curl -s -o /dev/null", "200");
    }
    fake.fail_cmd(
        "web2",
        "--name 'app-web-v9",
        1,
        "web2: no space left on device",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    let on = |host: &str| -> Vec<(usize, &String)> {
        let prefix = format!("ssh {host}: ");
        calls
            .iter()
            .enumerate()
            .filter(|(_, c)| c.starts_with(&prefix))
            .collect()
    };
    let web1 = on("web1");
    let web2 = on("web2");
    let pos =
        |v: &[(usize, &String)], pred: &dyn Fn(&str) -> bool| v.iter().position(|(_, c)| pred(c));
    let new1 = "app-web-v9-13000";

    let fwd = web1
        .iter()
        .position(|(_, c)| c.contains("2019/id/app") && c.contains("localhost:13000"))
        .unwrap_or_else(|| panic!("web1 never switched to its new target: {calls:?}"));
    let restore = web1
        .iter()
        .position(|(_, c)| c.contains("2019/id/app") && c.contains("localhost:21111"))
        .unwrap_or_else(|| panic!("web1's proxy was not restored to its OLD target: {calls:?}"));
    let rm = pos(&web1, &|c| c.contains("rm -f") && c.contains(new1))
        .unwrap_or_else(|| panic!("web1's new container was not removed: {calls:?}"));
    assert!(
        fwd < restore && restore < rm,
        "web1 unwind must be switch -> restore proxy -> rm new (LIFO): {calls:?}"
    );

    // No cross-host leakage in either direction.
    assert!(
        !web2.iter().any(|(_, c)| c.contains("2019/id/app")),
        "web2 never switched, so nothing on web2 may touch the proxy: {calls:?}"
    );
    assert!(
        !web2.iter().any(|(_, c)| c.contains("21111")),
        "a web1 compensation must not act on web2: {calls:?}"
    );
    assert!(
        !web1.iter().any(|(_, c)| c.contains("22222")),
        "a web2 compensation must not act on web1: {calls:?}"
    );
    // web2's own cleanup (its never-started container) is allowed, but only on web2.
    assert_eq!(
        web1.iter()
            .filter(|(_, c)| c.contains("rm -f") && c.contains(new1))
            .count(),
        1,
        "web1's new container is removed exactly once: {calls:?}"
    );
}
