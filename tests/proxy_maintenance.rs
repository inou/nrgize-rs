//! Integration: `proxy_maintenance(host, service, on_off, cfg)` (roadmap 2.8) — the same-surface
//! maintenance-mode entry point on both proxy backends. kamal-proxy has a native suspend/resume
//! primitive that remembers the registered target; Caddy has no such primitive, so maintenance
//! mode there replaces the route with a static response and resuming requires `cfg.target`.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Symlink the repo's real `lib/` into `dir` so `import "lib/…"` resolves.
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

/// Like `plan_for`, but calls a named function via `nrg run <fn> [args...]` instead of evaluating
/// the file top-to-bottom.
fn plan_run(script: &str, args: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("run")
        .args(args)
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// kamal-proxy backend
// ---------------------------------------------------------------------------

#[test]
fn kamal_proxy_maintenance_on_stops_the_service_with_the_default_drain_timeout() {
    let plan = plan_for(
        r#"
        import "lib/proxy" as proxy;
        proxy::proxy_maintenance("host1", "app", true);
    "#,
    );
    assert!(
        plan.contains("kamal-proxy stop 'app' --drain-timeout='30s'"),
        "missing kamal-proxy stop with the default drain timeout:\n{plan}"
    );
}

#[test]
fn kamal_proxy_maintenance_on_honors_a_custom_drain_timeout() {
    let plan = plan_for(
        r#"
        import "lib/proxy" as proxy;
        proxy::proxy_maintenance("host1", "app", true, #{ drain_timeout: "5s" });
    "#,
    );
    assert!(
        plan.contains("kamal-proxy stop 'app' --drain-timeout='5s'"),
        "custom drain_timeout not threaded through:\n{plan}"
    );
}

#[test]
fn kamal_proxy_maintenance_on_accepts_an_integer_drain_timeout_without_crashing() {
    // A plausible caller mistake — passing a bare number ("30 seconds") instead of a duration
    // string — must be coerced to a string, not crash sh_quote() with "Function not found:
    // sh_quote (i64)".
    let plan = plan_for(
        r#"
        import "lib/proxy" as proxy;
        proxy::proxy_maintenance("host1", "app", true, #{ drain_timeout: 5 });
    "#,
    );
    assert!(
        plan.contains("kamal-proxy stop 'app' --drain-timeout='5'"),
        "an integer drain_timeout must be stringified, not crash:\n{plan}"
    );
}

#[test]
fn kamal_proxy_maintenance_off_resumes_the_service() {
    let plan = plan_for(
        r#"
        import "lib/proxy" as proxy;
        proxy::proxy_maintenance("host1", "app", false);
    "#,
    );
    assert!(plan.contains("kamal-proxy resume 'app'"), "missing kamal-proxy resume:\n{plan}");
    assert!(!plan.contains("kamal-proxy stop"), "resume must not also stop:\n{plan}");
}

// ---------------------------------------------------------------------------
// Caddy backend
// ---------------------------------------------------------------------------

#[test]
fn caddy_maintenance_on_swaps_only_the_handle_not_the_whole_route() {
    // Opus review: PATCHing the WHOLE route object (as an earlier version of this code did) would
    // silently wipe the route's `match` (host/domain match gating TLS), turning a domained service
    // into a match-less catch-all that could swallow OTHER services' traffic in a multi-service
    // Caddy — the exact hazard robustness review R13 already warns about elsewhere. Scoping the
    // PATCH to `/id/<service>/handle` (the same sub-path-under-@id trick proxy_set_tls already uses
    // for `/match`) leaves everything else on the route untouched.
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "app", true);
    "#,
    );
    assert!(
        plan.contains("/id/app/handle"),
        "must PATCH only the /handle sub-path, not the whole route:\n{plan}"
    );
    assert!(
        plan.contains("\"handler\":\"static_response\""),
        "missing the static_response handler swap:\n{plan}"
    );
    assert!(plan.contains("\"status_code\":503"), "missing the default 503 status:\n{plan}");
    assert!(
        plan.contains("Service temporarily unavailable for maintenance."),
        "missing the default maintenance message:\n{plan}"
    );
    assert!(!plan.contains("\"@id\""), "must not PATCH the route's @id (would replace the whole route):\n{plan}");
}

#[test]
fn caddy_maintenance_on_url_encodes_a_service_name_containing_a_slash() {
    // Matching the existing precedent for proxy_deploy/proxy_remove/proxy_set_tls (robustness
    // review R17): the service name addresses a URL PATH SEGMENT, not just a shell argument.
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "x/../secret", true);
    "#,
    );
    assert!(
        plan.contains("/id/x%2F..%2Fsecret/handle"),
        "the slash-containing service name must be percent-encoded before it's used as a URL path segment:\n{plan}"
    );
    assert!(!plan.contains("/id/x/../secret"), "must never address the admin API with an un-encoded, traversal-shaped path:\n{plan}");
}

#[test]
fn caddy_maintenance_on_honors_a_custom_message_and_status_code() {
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "app", true, #{ status_code: 503, message: "Back soon!" });
    "#,
    );
    assert!(plan.contains("Back soon!"), "custom message not threaded through:\n{plan}");
}

#[test]
fn caddy_maintenance_off_requires_cfg_target() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "app", false);
    "#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dry-run", "Energize.rhai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cfg.target"));
}

#[test]
fn caddy_maintenance_off_with_target_restores_normal_routing() {
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "app", false, #{ target: "localhost:13000" });
    "#,
    );
    assert!(
        plan.contains("/id/app/handle"),
        "must PATCH only the /handle sub-path — cfg.domain was never touched by maintenance mode, \
         so restoring it must not require re-supplying cfg.domain either:\n{plan}"
    );
    assert!(
        plan.contains("\"handler\":\"reverse_proxy\""),
        "must restore the normal reverse_proxy handler:\n{plan}"
    );
    assert!(
        plan.contains("\"dial\":\"localhost:13000\""),
        "must point back at cfg.target:\n{plan}"
    );
    assert!(
        !plan.contains("static_response"),
        "must not still be serving the maintenance response:\n{plan}"
    );
}

#[test]
fn caddy_maintenance_off_honors_a_custom_health_path() {
    let plan = plan_for(
        r#"
        import "lib/caddy" as proxy;
        proxy::proxy_maintenance("host1", "app", false, #{ target: "localhost:13000", health_path: "/healthz" });
    "#,
    );
    assert!(
        plan.contains("\"health_checks\":{\"active\":{\"uri\":\"/healthz\""),
        "health_path must be threaded into the restored route's active health check:\n{plan}"
    );
}

// ---------------------------------------------------------------------------
// nrg run maintenance — a standalone task-oriented file (NOT lib/examples/rails.rhai, whose top
// level unconditionally calls recipe::standard_deploy() on every evaluation — including a bare
// `nrg run maintenance`, which would trigger a full redeploy as a side effect of a maintenance
// toggle. This mirrors docs/cli.md's own recommended pattern: put logic in named functions and
// call them explicitly, so `nrg run <fn>` only ever does what its name says.
// ---------------------------------------------------------------------------

#[test]
fn nrg_run_maintenance_task_puts_every_web_host_into_maintenance_mode() {
    let plan = plan_run(
        r#"
        import "lib/proxy" as proxy;
        const WEB_HOSTS = ["web1", "web2"];
        fn maintenance(on) {
            let on_off = on == "true";
            for host in global::WEB_HOSTS {
                proxy::proxy_maintenance(host, "app", on_off);
            }
        }
        "#,
        &["maintenance", "true"],
    );
    assert_eq!(
        plan.matches("kamal-proxy stop 'app'").count(),
        2,
        "expected the maintenance task to stop 'app' on both web hosts:\n{plan}"
    );
}
