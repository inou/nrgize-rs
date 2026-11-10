//! docker env tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn docker_run_throws_when_the_env_file_write_fails() {
    // Robustness review R30 (same bug class as R3b, found during its review): docker_run's
    // env-file path is fixed per container NAME (not per run), and accessory_run redeploys
    // reuse a stable name — so an unchecked write on a failed re-run would silently leave
    // `docker run --env-file` reading a STALE file from a prior successful run instead of
    // erroring. This loads the REAL lib/docker.rhai (symlinked, not reimplemented) via a
    // FakeRunner that fails specifically the env-file write, and asserts BOTH the thrown
    // message AND that the container is never actually started afterward.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{ envs: #{ "KEY": "value" } });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("host1", "mktemp -d", "/tmp/nrg-test-env");
    fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Failed to write env-file"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("run -d")),
        "must not start the container after the env-file write failed: {:?}",
        fake.calls()
    );
}

#[test]
fn docker_run_once_throws_when_the_env_file_write_fails() {
    // Same R30 fix, the docker_run_once sibling (used for pre-deploy release tasks like
    // migrations). Its env-file path is keyed by image tag, so repeated releases of the same
    // tag share the same risk of silently reusing a stale file.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run_once("host1", "myapp:v1", "bin/rails db:migrate", #{ envs: #{ "KEY": "value" } });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("host1", "mktemp -d", "/tmp/nrg-test-env");
    fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Failed to write env-file"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("run --rm")),
        "must not run the release-task container after the env-file write failed: {:?}",
        fake.calls()
    );
}

#[test]
fn docker_run_refuses_an_env_value_containing_a_newline() {
    // Robustness review R19: the env-file format is line-based (`KEY=VALUE` per line) — a
    // value containing a literal newline (e.g. a PEM-encoded key from a CI variable) used to
    // silently inject an EXTRA `KEY=VALUE` line into the container's environment instead of
    // being refused. Must throw BEFORE ever writing the env-file (so no stale/partial file is
    // left behind for a later successful retry to read).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{
               envs: #{ "SECRET_KEY": "line1\nEVIL=injected" },
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("newline"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("cat > ")),
        "must refuse before ever writing the env-file: {:?}",
        fake.calls()
    );
}

#[test]
fn docker_run_refuses_an_env_key_containing_equals() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{
               envs: #{ "BAD=KEY": "value" },
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("not a valid"), "got: {err}");
}

#[test]
fn docker_run_refuses_an_env_key_containing_a_newline() {
    // The KEY-newline sibling of the value-newline check above — an env var NAME containing
    // a literal newline is just as capable of injecting an extra line into the env-file as a
    // value is.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{
               envs: #{ "BAD\nKEY": "value" },
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("newline"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("cat > ")),
        "must refuse before ever writing the env-file: {:?}",
        fake.calls()
    );
}

#[test]
fn docker_run_accepts_a_non_string_env_value() {
    // Robustness review: `envs` map values aren't
    // restricted to strings — `k + "=" + v` already coerced a bare int/bool via Rhai's string
    // concat before this fix, so a config like `envs: #{ PORT: 3000 }` worked. The R19
    // validation must coerce the SAME way before calling `.contains()` on it, or it throws an
    // opaque "Function not found: contains" error for every non-string value instead of
    // validating (or not) anything.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{
               envs: #{ "PORT": 3000, "DEBUG": true },
           });
           state_set("passed", "true");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("host1", "mktemp -d", "/tmp/nrg-test-env");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );
}

#[test]
fn docker_run_once_refuses_an_env_value_containing_a_newline() {
    // Same R19 fix, the docker_run_once sibling used for pre-deploy release tasks.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run_once("host1", "myapp:v1", "bin/rails db:migrate", #{
               envs: #{ "SECRET_KEY": "line1\nEVIL=injected" },
           });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("newline"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("cat > ")),
        "must refuse before ever writing the env-file: {:?}",
        fake.calls()
    );
}

#[test]
fn docker_run_accepts_a_normal_env_value_unaffected_by_the_r19_validation() {
    // Companion regression check: an ordinary env value (no newline, key has no '=') must
    // still work exactly as before.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker;
           docker::docker_run("host1", "myapp:v1", "app-new", #{
               envs: #{ "DATABASE_URL": "postgres://u:p@db/x" },
           });
           state_set("passed", "true");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("host1", "mktemp -d", "/tmp/nrg-test-env");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );
}
