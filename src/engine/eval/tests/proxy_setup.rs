//! proxy setup tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn caddy_proxy_boot_throws_when_the_config_write_fails() {
    // Robustness review R3b: lib/caddy.rhai's proxy_boot() used to discard write_remote's
    // result — a non-root deploy user can't write /etc/caddy, but `docker run -d` still
    // returns 0 regardless (Docker's `-v` just creates an empty directory at that path if
    // nothing exists), so Caddy would crash-loop with no config while proxy_boot reported
    // success; the failure only surfaced later as an opaque curl error during the traffic
    // switch. This loads the REAL lib/caddy.rhai (symlinked from the repo, not reimplemented)
    // via a FakeRunner that fails specifically the config-write command, and asserts BOTH
    // that proxy_boot throws with a clear message AND that it never proceeds to start the
    // Caddy container afterward.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/caddy" as proxy; proxy::proxy_boot("host1", #{});"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Failed to write Caddy config"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("caddy run")),
        "must not start Caddy after the config write failed: {:?}",
        fake.calls()
    );
}

#[test]
fn kamal_proxy_boot_throws_when_the_image_pull_fails() {
    // Robustness review R25: proxy_boot used to discard the pull's result — a failed pull
    // (network blip, registry auth, rate limit) would silently fall through to `docker run
    // -d`, which happily starts whatever image is ALREADY cached locally (stale, or nothing
    // at all) instead of the fresh one the caller asked for. This loads the REAL
    // lib/proxy.rhai (symlinked, not reimplemented) via a FakeRunner that fails specifically
    // the pull command, and asserts BOTH the thrown message AND that it never proceeds to
    // start the proxy container afterward.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/proxy" as proxy; proxy::proxy_boot("host1", #{});"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "host1",
        "pull 'basecamp/kamal-proxy",
        1,
        "rate limit exceeded",
    );
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Failed to pull"), "got: {err}");
    assert!(err.contains("basecamp/kamal-proxy"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("run -d")),
        "must not start kamal-proxy after the pull failed: {:?}",
        fake.calls()
    );
}

#[test]
fn caddy_proxy_boot_throws_when_the_image_pull_fails() {
    // Same R25 fix, the lib/caddy.rhai sibling.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/caddy" as proxy; proxy::proxy_boot("host1", #{});"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("host1", "pull 'caddy:2", 1, "rate limit exceeded");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Failed to pull"), "got: {err}");
    assert!(err.contains("caddy:2"), "got: {err}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("run -d")),
        "must not start Caddy after the pull failed: {:?}",
        fake.calls()
    );
}

#[test]
fn run_setup_creates_the_network_and_boots_kamal_proxy_on_every_host() {
    // Roadmap 1.5 step 1 (`nrg setup`): the Rhai-engine half of the bootstrap — create the
    // network (if given) and boot the proxy — reusing the SAME stdlib logic deploy() uses,
    // via a synthesized script exactly like run_rollback's own pattern.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    let fake = FakeRunner::shared();
    let hosts = vec!["web1".to_string(), "web2".to_string()];
    run_setup(
        dir.path(),
        &hosts,
        "kamal",
        None,
        Some("mynet"),
        shared(fake.clone()),
    )
    .unwrap();

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("web1") && c.contains("network create 'mynet'")),
        "must create the network on web1: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("web2") && c.contains("network create 'mynet'")),
        "must create the network on web2: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("pull 'basecamp/kamal-proxy:latest'")),
        "must boot kamal-proxy (the default backend): {calls:?}"
    );
}

#[test]
fn run_setup_boots_caddy_instead_when_proxy_kind_is_caddy() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

    let fake = FakeRunner::shared();
    let hosts = vec!["web1".to_string()];
    run_setup(
        dir.path(),
        &hosts,
        "caddy",
        Some("2.8.4"),
        None,
        shared(fake.clone()),
    )
    .unwrap();

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("pull 'caddy:2.8.4'")),
        "must boot Caddy with the pinned proxy_version, not kamal-proxy: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("kamal-proxy")),
        "must not touch kamal-proxy when --proxy caddy is chosen: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("network create")),
        "no network was requested, so none should be created: {calls:?}"
    );
}

#[test]
fn docker_network_create_all_throws_naming_the_host_on_a_real_failure() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker; docker::docker_network_create_all(["web1"], "mynet");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd("web1", "network create", 1, "Error: permission denied");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("web1"), "got: {err}");
    assert!(err.contains("permission denied"), "got: {err}");
}

#[test]
fn docker_network_create_treats_already_exists_as_success() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/docker" as docker; docker::docker_network_create_all(["web1"], "mynet");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "web1",
        "network create",
        1,
        "Error response from daemon: network with name mynet already exists",
    );
    run_file(&main, shared(fake.clone())).unwrap();
}
