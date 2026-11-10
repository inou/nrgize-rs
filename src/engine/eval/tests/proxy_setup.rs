//! proxy setup tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

#[test]
fn caddy_proxy_boot_throws_when_the_config_write_fails() {
    // lib/caddy.rhai's proxy_boot() used to discard write_remote's
    // result — a non-root deploy user can't write /etc/caddy, but `docker run -d` still
    // returns 0 regardless (Docker's `-v` just creates an empty directory at that path if
    // nothing exists), so Caddy would crash-loop with no config while proxy_boot reported
    // success; the failure only surfaced later as an opaque curl error during the traffic
    // switch. This loads the REAL lib/caddy.rhai (embedded, not reimplemented)
    // via a FakeRunner that fails specifically the config-write command, and asserts BOTH
    // that proxy_boot throws with a clear message AND that it never proceeds to start the
    // Caddy container afterward.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/caddy" as proxy; proxy::proxy_boot("host1", #{});"#,
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
fn caddy_proxy_boot_throws_when_the_image_pull_fails() {
    // Same pull-failure fix, the lib/caddy.rhai sibling.
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/caddy" as proxy; proxy::proxy_boot("host1", #{});"#,
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
fn run_setup_creates_the_network_and_boots_caddy_on_every_host() {
    // `nrg setup`: the Rhai-engine half of the bootstrap — create the
    // network (if given) and boot the proxy — reusing the SAME stdlib logic deploy() uses,
    // via a synthesized script exactly like run_rollback's own pattern.
    let fake = FakeRunner::shared();
    let hosts = vec!["web1".to_string(), "web2".to_string()];
    run_setup(&hosts, None, Some("mynet"), shared(fake.clone())).unwrap();

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
        calls.iter().any(|c| c.contains("pull 'caddy:2'")),
        "must boot Caddy: {calls:?}"
    );
}

#[test]
fn run_setup_pins_the_caddy_version_and_skips_the_network_when_none_is_given() {
    let fake = FakeRunner::shared();
    let hosts = vec!["web1".to_string()];
    run_setup(&hosts, Some("2.8.4"), None, shared(fake.clone())).unwrap();

    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("pull 'caddy:2.8.4'")),
        "must boot Caddy with the pinned proxy_version: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("network create")),
        "no network was requested, so none should be created: {calls:?}"
    );
}

#[test]
fn docker_network_create_all_throws_naming_the_host_on_a_real_failure() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/docker" as docker; docker::docker_network_create_all(["web1"], "mynet");"#,
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
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "std/docker" as docker; docker::docker_network_create_all(["web1"], "mynet");"#,
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

/// Run `body` against the real stdlib `lib/caddy` on a FakeRunner and return every command it saw.
fn proxy_calls(body: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(&main, format!("import \"std/caddy\" as proxy; {body}")).unwrap();
    let fake = FakeRunner::shared();
    run_file(&main, shared(fake.clone())).unwrap();
    fake.calls()
}

/// Index of the one call containing `needle`; fails if none.
fn call_index(calls: &[String], needle: &str) -> usize {
    calls
        .iter()
        .position(|c| c.contains(needle))
        .unwrap_or_else(|| panic!("no call containing {needle:?}: {calls:#?}"))
}

#[test]
fn caddy_proxy_boot_pulls_removes_stale_then_runs_with_its_mounts_and_command() {
    let calls = proxy_calls(r#"proxy::proxy_boot("host1", #{ proxy_version: "2.8.4" });"#);
    let pull = call_index(&calls, "pull 'caddy:2.8.4'");
    let rm = call_index(&calls, "rm -f caddy 2>/dev/null || true");
    let run = call_index(&calls, "run -d --name caddy");
    assert!(pull < rm && rm < run, "wrong order: {calls:#?}");
    assert_eq!(
        calls[run],
        "ssh host1: docker run -d --name caddy --restart unless-stopped --network host \
         -v caddy-data:/data -v caddy-config:/config \
         -v /etc/caddy/caddy.json:/etc/caddy/caddy.json:ro \
         'caddy:2.8.4' caddy run --resume --config /etc/caddy/caddy.json"
    );
}

#[test]
fn proxy_stop_stops_then_removes_the_container() {
    let calls = proxy_calls(r#"proxy::proxy_stop("host1");"#);
    let stop = call_index(&calls, "stop caddy 2>/dev/null || true");
    let rm = call_index(&calls, "rm caddy 2>/dev/null || true");
    assert!(stop < rm, "{calls:#?}");
}
