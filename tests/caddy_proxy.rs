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
