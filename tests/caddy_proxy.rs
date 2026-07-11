//! Integration: `deploy()` is proxy-pluggable. `cfg.proxy: "caddy"` routes the fleet through
//! lib/caddy.rhai (Caddy admin API) instead of the default kamal-proxy, and threads `cfg.domain`
//! into the route (Caddy auto-HTTPS). The default stays kamal-proxy.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Symlink the repo's real `lib/` into `dir` so `import "lib/deploy"` resolves.
fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
    #[cfg(not(unix))]
    {
        let dst = dir.join("lib");
        fs::create_dir_all(&dst).unwrap();
        for e in fs::read_dir(&repo_lib).unwrap() {
            let e = e.unwrap();
            if e.path().extension().and_then(|s| s.to_str()) == Some("rhai") {
                fs::copy(e.path(), dst.join(e.file_name())).unwrap();
            }
        }
    }
}

fn plan_for(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
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
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            proxy: "caddy", domain: "app.example.com",
        });
    "#,
    );

    // Caddy is booted (with --resume for route durability) and the switch goes through its admin API.
    assert!(plan.contains("docker pull caddy:2"), "missing caddy boot:\n{plan}");
    assert!(
        plan.contains("caddy run --resume --config /etc/caddy/caddy.json"),
        "missing caddy run --resume:\n{plan}"
    );
    assert!(
        plan.contains("/config/apps/http/servers/srv0/routes")
            || plan.contains("/id/app"),
        "missing caddy admin-API traffic switch:\n{plan}"
    );
    // cfg.domain threaded into the route (Caddy auto-HTTPS). The JSON is json_string-escaped, so
    // the value is still a plain "app.example.com"; the whole body is sh_quote-wrapped.
    assert!(
        plan.contains("\"host\":[\"app.example.com\"]"),
        "domain not threaded into the caddy route (no host match -> no TLS):\n{plan}"
    );
    // NOT kamal-proxy.
    assert!(
        !plan.contains("kamal-proxy deploy"),
        "caddy deploy must not use kamal-proxy:\n{plan}"
    );
}

#[test]
fn deploy_defaults_to_kamal_proxy() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );
    assert!(
        plan.contains("kamal-proxy deploy 'app'"),
        "default should use kamal-proxy:\n{plan}"
    );
    assert!(
        !plan.contains("caddy run") && !plan.contains("localhost:2019"),
        "default must not boot Caddy:\n{plan}"
    );
}

#[test]
fn deploy_with_caddy_proxy_configures_an_active_health_check_on_the_upstream() {
    // Robustness review R12's "still open" note asked whether the kamal-proxy-vs-Caddy
    // switch-time health-gating asymmetry the original finding described is still accurate —
    // lib/caddy.rhai's `proxy_deploy` already builds a `health_checks.active` block whenever a
    // non-empty `health_path` is passed, and `deploy()` (lib/deploy.rhai) always passes one
    // (default `/up`) unless a caller explicitly overrides it to `""`. This test proves that
    // wiring actually reaches a real `deploy()` call, not just that the mechanism exists in
    // isolation — closing the investigation with a concrete assertion, not just code-reading.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
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
         check, the same way it reaches kamal-proxy's --health-check-path — Caddy must not be \
         left without a switch-time health gate kamal-proxy has:\n{plan}"
    );
}

#[test]
fn proxy_deploy_url_encodes_a_service_name_containing_a_slash() {
    // Robustness review R17: `service` addresses a Caddy admin-API URL PATH SEGMENT
    // (`/id/<service>`), not just a shell argument — sh_quote() alone doesn't stop a `/` or
    // `../` inside it from addressing a DIFFERENT admin-API path than intended. url_encode()
    // must run before the segment is assembled into the URL.
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
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
        import "lib/caddy" as proxy;
        proxy::proxy_remove("host1", "x/../secret");
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2Fsecret"),
        "proxy_remove must percent-encode the service name too:\n{plan}"
    );
    assert!(!plan.contains("/id/x/../secret"), "must not leave an un-encoded path:\n{plan}");
}

#[test]
fn proxy_set_tls_url_encodes_a_service_name_containing_a_slash() {
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_set_tls("host1", "x/../secret", "app.example.com");
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2Fsecret/match"),
        "proxy_set_tls must percent-encode the service name too:\n{plan}"
    );
    assert!(!plan.contains("/id/x/../secret/match"), "must not leave an un-encoded path:\n{plan}");
}
