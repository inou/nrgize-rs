//! Integration: `deploy()` drives Caddy (lib/caddy.rhai, admin API) and threads `cfg.domain`
//! into the route (Caddy auto-HTTPS).

use assert_cmd::Command;
use std::fs;

fn plan_for(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn deploy_with_caddy_proxy_uses_the_admin_api() {
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            proxy: "caddy", domain: "app.example.com",
        });
    "#,
    );

    // Caddy is booted (with --resume for route durability) and the switch goes through its admin API.
    assert!(
        plan.contains("docker pull 'caddy:2'"),
        "missing caddy boot:\n{plan}"
    );
    assert!(
        plan.contains("caddy run --resume --config /etc/caddy/caddy.json"),
        "missing caddy run --resume:\n{plan}"
    );
    assert!(
        plan.contains("/config/apps/http/servers/srv0/routes") || plan.contains("/id/app"),
        "missing caddy admin-API traffic switch:\n{plan}"
    );
    // cfg.domain threaded into the route (Caddy auto-HTTPS). The JSON is json_string-escaped, so
    // the value is still a plain "app.example.com"; the whole body is sh_quote-wrapped.
    assert!(
        plan.contains("\"host\":[\"app.example.com\"]"),
        "domain not threaded into the caddy route (no host match -> no TLS):\n{plan}"
    );
}

#[test]
fn deploy_boots_caddy_without_any_cfg_proxy() {
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );
    assert!(
        plan.contains("caddy run --resume") && plan.contains("localhost:2019"),
        "the default must boot Caddy:\n{plan}"
    );
}

#[test]
fn deploy_rejects_the_removed_kamal_proxy_before_any_mutation() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true, proxy: "kamal",
        });
    "#,
    )
    .unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("cfg.proxy names a proxy backend that was removed in 0.2"),
        "missing migration error:\n{stderr}"
    );
    assert!(
        !stdout.contains("docker pull") && !stdout.contains("caddy run"),
        "must fail before touching any host:\n{stdout}"
    );
}

#[test]
fn deploy_with_caddy_proxy_configures_an_active_health_check_on_the_upstream() {
    // lib/caddy.rhai's `proxy_deploy` already builds a `health_checks.active` block whenever a
    // non-empty `health_path` is passed, and `deploy()` (lib/deploy.rhai) always passes one
    // (default `/up`) unless a caller explicitly overrides it to `""`. This test proves that
    // wiring actually reaches a real `deploy()` call, not just that the mechanism exists in
    // isolation.
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            proxy: "caddy",
        });
    "#,
    );
    assert!(
        plan.contains("\"health_checks\":{\"active\":{\"uri\":\"/up\"")
            && plan.contains("\"interval\":\"10s\""),
        "deploy()'s default health_path (\"/up\") must reach Caddy's route as an active health \
         check:\n{plan}"
    );
}

#[test]
fn deploy_pins_the_caddy_version_from_cfg_proxy_version() {
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            proxy: "caddy", proxy_version: "2.8.4",
        });
    "#,
    );
    assert!(
        plan.contains("docker pull 'caddy:2.8.4'"),
        "cfg.proxy_version must pin the pulled caddy image tag:\n{plan}"
    );
    assert!(
        !plan.contains("docker pull 'caddy:2'\n"),
        "must not also pull the default tag:\n{plan}"
    );
}

#[test]
fn proxy_deploy_url_encodes_a_service_name_containing_a_slash() {
    // `service` addresses a Caddy admin-API URL PATH SEGMENT
    // (`/id/<service>`), not just a shell argument — sh_quote() alone doesn't stop a `/` or
    // `../` inside it from addressing a DIFFERENT admin-API path than intended. url_encode()
    // must run before the segment is assembled into the URL.
    let plan = plan_for(
        r#"
        import "std/caddy" as proxy;
        proxy::proxy_deploy("host1", "x/../../config/apps", "localhost:13000", #{});
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2F..%2Fconfig%2Fapps"),
        "the slash-containing service name must be percent-encoded before it's used as a URL \
         path segment:\n{plan}"
    );
    assert!(
        !plan.contains("/id/x/../../config/apps"),
        "must never address the admin API with an un-encoded, traversal-shaped path:\n{plan}"
    );
}

#[test]
fn proxy_remove_url_encodes_a_service_name_containing_a_slash() {
    let plan = plan_for(
        r#"
        import "std/caddy" as proxy;
        proxy::proxy_remove("host1", "x/../secret");
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2Fsecret"),
        "proxy_remove must percent-encode the service name too:\n{plan}"
    );
    assert!(
        !plan.contains("/id/x/../secret"),
        "must not leave an un-encoded path:\n{plan}"
    );
}

#[test]
fn proxy_set_tls_url_encodes_a_service_name_containing_a_slash() {
    let plan = plan_for(
        r#"
        import "std/caddy" as proxy;
        proxy::proxy_set_tls("host1", "x/../secret", "app.example.com");
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2Fsecret/match"),
        "proxy_set_tls must percent-encode the service name too:\n{plan}"
    );
    assert!(
        !plan.contains("/id/x/../secret/match"),
        "must not leave an un-encoded path:\n{plan}"
    );
}
