//! docker prune tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn docker_cleanup_reports_failure_when_container_prune_fails_even_if_image_prune_succeeds() {
    // docker_cleanup used to return ONLY the image-prune ExecResult
    // unconditionally, discarding the container-prune result entirely — so a caller checking
    // `.ok` (like deploy()'s post-commit loop) couldn't tell an SSH-level failure during the
    // FIRST prune (e.g. a dropped connection) from a clean run, as long as the SECOND prune
    // still happened to succeed. Now returns whichever prune actually failed.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           let r = docker::docker_cleanup("host1");
           state_set("cleanup.ok", "" + r.ok);"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "host1",
        "container prune",
        1,
        "ssh: connection reset by peer",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    assert_eq!(
        ctx.state.lock().unwrap().get("cleanup.ok").as_deref(),
        Some("false"),
        "a failed container-prune must make docker_cleanup report failure, even though the \
         later image-prune succeeded"
    );
    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("container prune")),
        "{calls:?}"
    );
    assert!(calls.iter().any(|c| c.contains("image prune")), "{calls:?}");
}

#[test]
fn docker_prune_old_images_keeps_the_newest_n_and_never_removes_protected_tags() {
    // Robustness review R22: docker_prune_old_images removes a repo's own old TAGGED images
    // beyond the `keep_n` most recent, but must NEVER remove a tag in `protect_tags` no matter
    // how old it is (the caller — deploy() — always protects the version just deployed and the
    // one rollback() might still need). Three real tags (v9 newest, v8, v7 oldest) plus a
    // dangling `<none>` entry (which docker_cleanup's own image-prune handles, not this
    // function). keep_n: 1 with v7 (the OLDEST) explicitly protected proves protection is
    // independent of recency — v9 survives as "the 1 most recent", v7 survives because it's
    // protected despite being older than the keep-window, and only v8 (neither newest-kept nor
    // protected) is actually removed.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           let r = docker::docker_prune_old_images("host1", "ghcr.io/org/app", 1, ["v7"]);
           state_set("prune.ok", "" + r.ok);
           state_set("prune.removed", "" + r.removed.len());
           state_set("prune.removed.0", r.removed[0]);"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd(
        "host1",
        "--format '{{.Tag}}|{{.CreatedAt}}'",
        "v9|2024-01-03 10:00:00 +0000 UTC\n\
         v8|2024-01-02 10:00:00 +0000 UTC\n\
         v7|2024-01-01 10:00:00 +0000 UTC\n\
         <none>|2024-01-04 10:00:00 +0000 UTC\n",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(state.get("prune.ok").as_deref(), Some("true"));
    assert_eq!(
        state.get("prune.removed").as_deref(),
        Some("1"),
        "exactly one tag (v8) should have been removed"
    );
    assert_eq!(
        state.get("prune.removed.0").as_deref(),
        Some("ghcr.io/org/app:v8")
    );

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
        "v8 (neither newest-kept nor protected) must be removed: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v9'")),
        "v9 (the 1 most recent, keep_n=1) must survive: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v7'")),
        "v7 (explicitly protected) must survive despite being the oldest: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("rmi") && c.contains("<none>")),
        "the dangling <none> tag must never be targeted by this function: {calls:?}"
    );
}

#[test]
fn docker_prune_old_images_reports_failure_without_guessing_when_listing_fails() {
    // An SSH-level failure to even LIST images (dropped connection, runtime CLI error) must be
    // reported via `ok: false` with nothing removed — never guessed at by acting on incomplete
    // data.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           let r = docker::docker_prune_old_images("host1", "ghcr.io/org/app", 0, []);
           state_set("prune.ok", "" + r.ok);
           state_set("prune.removed", "" + r.removed.len());"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "host1",
        "--format '{{.Tag}}|{{.CreatedAt}}'",
        255,
        "ssh: connection reset by peer",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(state.get("prune.ok").as_deref(), Some("false"));
    assert_eq!(state.get("prune.removed").as_deref(), Some("0"));
    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("rmi")),
        "must not attempt any rmi when the listing itself failed: {calls:?}"
    );
}

#[test]
fn docker_rename_no_such_container_is_treated_as_the_idempotent_no_op_it_is() {
    // M6: docker_rename's remote command used to always
    // end in `2>/dev/null || true`, so EVERY docker-level rename failure looked identical to
    // success. This is the "no such container" half of that finding: a rename failing because
    // its source no longer exists (e.g. a retry after a previous partial run already renamed
    // it away) is a legitimate no-op, not an error — the fix must still let deploy() proceed
    // normally and persist state, exactly as it did before (when `|| true` masked it the same
    // way, just less honestly).
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
        1,
        "Error: No such container: app-web",
    );

    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(
        state.get("app.port.127.0.0.1").as_deref(),
        Some("13000"),
        "a 'No such container' rename failure is idempotent, not a real error — the swap must \
         still be considered complete and the port persisted"
    );
    assert!(
        state.get("app.target.127.0.0.1").is_some(),
        "the target must also still be persisted for the idempotent no-op case"
    );
}

#[test]
fn docker_rename_surfaces_a_real_name_conflict_instead_of_masking_it() {
    // M6, the other half: a rename failing because the
    // TARGET name is already in use (e.g. a stale leftover container from an earlier
    // interrupted deploy) is a real, non-idempotent problem — before this fix, the blanket
    // `|| true` made this look identical to success too. Now it must surface exactly like the
    // existing R6b SSH-failure case: state must NOT be persisted for that host, since the
    // rename dance did not actually complete.
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
        1,
        "Error response from daemon: Conflict. The container name \"/app-web-old\" is already \
         in use by container \"deadbeef\". You have to remove (or rename) that container to be \
         able to reuse that name.",
    );

    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert!(
        state.get("app.port.127.0.0.1").as_deref() == Some("13000"),
        "a real name-conflict rename failure must NOT be masked as success — the port must not \
         be persisted as if the swap completed"
    );
    assert!(
        state.get("app.target.127.0.0.1").as_deref() == Some("localhost:13000"),
        "a real name-conflict rename failure must NOT be masked as success — the target must \
         not be persisted as if the swap completed"
    );

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("prune")),
        "the rest of that host's cleanup steps must still be attempted: {calls:?}"
    );
}

#[test]
fn docker_rename_recognizes_podmans_differently_phrased_no_such_container_error() {
    // M6 follow-up: the "no such container" classification in docker_rename
    // (lib/docker.rhai) must not be a Docker-only, exact-case match. Podman is a first-class
    // supported runtime (lib/runtime.rhai, not marked experimental) and phrases the same
    // "nothing to rename" condition differently — lowercase, different sentence shape — than
    // Docker's "Error: No such container: X". An exact-case, Docker-phrasing-only match would
    // misclassify Podman's own idempotent-retry wording as a real failure, reintroducing the
    // false-alarm-and-unpersisted-state bug the M6 fix exists to close, just for Podman fleets.
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
    // Podman's actual phrasing: lowercase, "no container with name or ID ... found: no such
    // container" — deliberately NOT capitalized like Docker's "Error: No such container".
    fake.fail_cmd(
        "127.0.0.1",
        "rename",
        125,
        "Error: no container with name or ID \"app-web\" found: no such container",
    );

    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let state = ctx.state.lock().unwrap();
    assert_eq!(
        state.get("app.port.127.0.0.1").as_deref(),
        Some("13000"),
        "Podman's differently-cased/-phrased 'no such container' error must still be \
         recognized as the idempotent no-op it is, not misclassified as a real failure"
    );
    assert!(state.get("app.target.127.0.0.1").is_some());
}

#[test]
fn deploy_wires_keep_images_through_to_docker_prune_old_images_with_the_right_protect_tags() {
    // Robustness review R22, end-to-end: a real (LIVE, not dry-run — dry-run's ssh_exec never
    // actually runs, so the prune listing would see empty stdout) deploy() with `keep_images`
    // set must reach `docker_prune_old_images` with the deployed version protected, and must
    // extract the bare repo correctly even from a `registry:port/path` image reference (the
    // same registry-host:port ambiguity `extract_version` already has to handle) — proving
    // `extract_repo` isn't confused by the registry's OWN colon.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           deploy::deploy(["127.0.0.1"], "registry.example.com:5000/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1, keep_images: 0,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    fake.respond_cmd(
        "127.0.0.1",
        "--format '{{.Tag}}|{{.CreatedAt}}'",
        "v9|2024-01-03 10:00:00 +0000 UTC\nv8|2024-01-02 10:00:00 +0000 UTC\n",
    );

    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("images 'registry.example.com:5000/app'")),
        "must list the bare repo, unconfused by the registry's own :5000 port: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("rmi 'registry.example.com:5000/app:v8'")),
        "the older, unprotected tag must be pruned: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("rmi 'registry.example.com:5000/app:v9'")),
        "the just-deployed version must be protected regardless of keep_images: 0: {calls:?}"
    );

    // Pruning must never gate the post-commit port/target persistence.
    let state = ctx.state.lock().unwrap();
    assert!(state.get("app.port.127.0.0.1").is_some());
    assert!(state.get("app.target.127.0.0.1").is_some());
}

#[test]
fn deploy_with_keep_images_unset_never_calls_docker_prune_old_images() {
    // Strict opt-in (robustness review R22): omitting cfg.keep_images entirely must leave
    // pruning completely inert — no image listing, no rmi, identical to pre-R22 behavior.
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
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("{{.Tag}}")),
        "keep_images unset must never even list images: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("rmi")),
        "keep_images unset must never remove any image: {calls:?}"
    );
}

#[test]
fn deploy_protects_the_previous_versions_tag_but_only_when_it_is_the_same_repo() {
    // Robustness review R22: the previous version rollback() might still need must survive
    // pruning regardless of age — UNLESS the caller changed image_repo between deploys, in
    // which case `.image`'s old value is a different repo entirely and irrelevant to pruning
    // THIS repo. Pre-seeds `<service>.image` (deploy() reads it as `prev_image` before
    // overwriting it) to simulate "this service was already on some prior version."
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           state_set("app.image", "ghcr.io/org/app:v8");
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1, keep_images: 0,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    fake.respond_cmd(
        "127.0.0.1",
        "--format '{{.Tag}}|{{.CreatedAt}}'",
        "v9|2024-01-03 10:00:00 +0000 UTC\n\
         v8|2024-01-02 10:00:00 +0000 UTC\n\
         v7|2024-01-01 10:00:00 +0000 UTC\n",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
        "v8 (the PREVIOUS version, same repo) must survive even with keep_images: 0: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v7'")),
        "v7 (neither current nor previous) must still be pruned: {calls:?}"
    );
}

#[test]
fn deploy_does_not_protect_a_previous_versions_tag_from_a_different_repo() {
    // Companion: if the caller changed image_repo between deploys, `.image`'s old value names
    // an UNRELATED repo — its version number is coincidental and must not spuriously protect a
    // same-named tag in the NEW repo.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    fs::write(
        &main,
        r#"import "lib/deploy" as deploy;
           state_set("app.image", "ghcr.io/org/OLDREPO:v8");
           deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
               container_port: 3000, skip_build: true, skip_push: true,
               health_attempts: 1, keep_images: 0,
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
    fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
    fake.respond_cmd(
        "127.0.0.1",
        "--format '{{.Tag}}|{{.CreatedAt}}'",
        "v9|2024-01-03 10:00:00 +0000 UTC\nv8|2024-01-02 10:00:00 +0000 UTC\n",
    );
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
        "v8 in the NEW repo must NOT be spuriously protected just because an unrelated OLD \
         repo happened to have the same version number: {calls:?}"
    );
}
