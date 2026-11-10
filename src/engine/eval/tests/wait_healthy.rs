//! wait healthy tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::FakeRunner;
use std::fs;

use super::helpers::*;

#[test]
fn wait_healthy_requires_consecutive_passes_before_returning_healthy() {
    // Robustness review R12: a single passing check used to be enough to consider a container
    // healthy — an app that answers /up once during boot then crashes (or flaps) still got
    // traffic switched to it. With cfg.consecutive: 2, a server that answers 200, 500, 200, 200
    // must NOT pass until the 4th request (the 500 at index 1 resets the streak; only the pair
    // at indices 2-3 are two consecutive 200s) — asserted by checking EXACTLY 4 requests were
    // made, not just that wait_healthy eventually returned without throwing (which the old,
    // single-check behavior would ALSO do, just after only 1 request).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    let (port, counter) = spawn_flaky_http_server(std::collections::HashSet::from([1]));
    fs::write(
        &main,
        format!(
            r#"import "lib/healthcheck" as health;
               let r = health::wait_healthy("http://127.0.0.1:{port}/", #{{
                   attempts: 10, interval: 0, consecutive: 2,
               }});
               state_set("passed", "true");"#
        ),
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "must take exactly 4 requests (200, 500 resets streak, 200, 200) to see 2 CONSECUTIVE \
         passes — anything else (e.g. 1) means it accepted a single pass instead"
    );
}

#[test]
fn wait_healthy_with_default_consecutive_still_passes_on_the_first_200() {
    // Companion regression check: the historical default (consecutive: 1, i.e. cfg omitted
    // entirely) must still pass on the very first successful check, unchanged from before R12.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");

    let (port, counter) = spawn_flaky_http_server(std::collections::HashSet::new());
    fs::write(
        &main,
        format!(
            r#"import "lib/healthcheck" as health;
               health::wait_healthy("http://127.0.0.1:{port}/", #{{ attempts: 3, interval: 0 }});
               state_set("passed", "true");"#
        ),
    )
    .unwrap();

    let fake = FakeRunner::shared();
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "with the default consecutive: 1, must return after exactly ONE passing request"
    );
}

#[test]
fn wait_healthy_on_host_probes_via_ssh_curl_against_localhost_not_the_control_machine() {
    // Robustness review R7-health: this is the direct proof of the fix. `host` here is a
    // `user@host`-style SSH alias — exactly the form documented for `web_hosts` in
    // docs/deploy.md, and NOT valid as an HTTP authority (userinfo isn't allowed there). The
    // OLD code built "http://" + host + ":" + port + path and GETtted that from the control
    // machine, which would either be a malformed URL or, even if parsed leniently, target the
    // wrong thing. `wait_healthy_on_host` must SSH to that exact alias but curl "localhost" —
    // never the alias — from ON that host.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("deploy@web1", 3000, #{ attempts: 1 });
           state_set("passed", "true");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("deploy@web1", "curl -s -o /dev/null", "200");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("ssh deploy@web1: ") && c.contains("curl")),
        "must ssh to the exact alias given, not a hostname derived from it: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("http://localhost:3000/up")),
        "must curl localhost on the host itself, never the alias or a control-machine URL: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("deploy@web1:3000") || c.contains("http://deploy@web1")),
        "must never build an HTTP URL out of the raw ssh alias: {calls:?}"
    );
}

#[test]
fn wait_healthy_on_host_throws_after_exhausting_attempts_with_the_last_status() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("web1", 3000, #{ attempts: 2, interval: 0 });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("web1", "curl -s -o /dev/null", "503");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(
        err.contains("Health check failed on web1 after 2 attempts"),
        "got: {err}"
    );
    assert!(err.contains("last status: 503"), "got: {err}");
}

#[test]
fn wait_healthy_on_host_treats_an_ssh_level_failure_as_status_zero() {
    // The curl probe itself can fail at the SSH layer (e.g. the SSH connection drops, or the
    // command can't even run) rather than curl reporting a real HTTP status. `ssh_http_status`
    // must treat that the same as a transport failure (status 0), not crash or hang.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("web1", 3000, #{ attempts: 1, interval: 0 });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.fail_cmd(
        "web1",
        "curl -s -o /dev/null",
        255,
        "ssh: connection refused",
    );
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(
        err.contains("Health check failed on web1 after 1 attempts"),
        "got: {err}"
    );
    assert!(err.contains("last status: 0"), "got: {err}");
}

#[test]
fn wait_healthy_on_host_treats_a_nonzero_exit_as_failure_even_with_numeric_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("web1", 3000, #{ attempts: 1, interval: 0 });"#,
    )
    .unwrap();

    let ctx = shared(Arc::new(FailedExitButNumericStdoutRunner));
    let err = run_file(&main, ctx).unwrap_err();
    assert!(
        err.contains("Health check failed on web1 after 1 attempts"),
        "got: {err}"
    );
    assert!(
        err.contains("last status: 0"),
        "got: {err} (must not treat a failed ssh_exec as a real 200)"
    );
}

#[test]
fn wait_healthy_on_host_requires_consecutive_passes_before_returning_healthy() {
    // Robustness review R12's consecutive-pass requirement, exercised through the NEW
    // R7-health host-side probe: with cfg.consecutive: 2, a host that answers 200, 503, 200,
    // 200 must NOT pass until the 4th probe (the 503 resets the streak) — asserted by the
    // EXACT probe count, not just "eventually passes" (which the old single-check behavior
    // would also do, just after the 1st probe).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("web1", 3000, #{
               attempts: 10, interval: 0, consecutive: 2,
           });
           state_set("passed", "true");"#,
    )
    .unwrap();

    let runner = Arc::new(SequencedCurlRunner {
        responses: vec!["200", "503", "200", "200"],
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let ctx = shared(runner.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );
    assert_eq!(
        runner.calls.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "must take exactly 4 probes (200, 503 resets streak, 200, 200) to see 2 CONSECUTIVE \
         passes — anything else (e.g. 1) means it accepted a single pass instead"
    );
}

#[test]
fn wait_healthy_all_actually_probes_every_host_via_ssh() {
    // Robustness review R8b: tests/deploy_behaviors.rs's own
    // wait_healthy_all_checks_each_host_via_ssh_not_a_control_machine_url only asserts the
    // ABSENCE of a control-machine URL in a dry-run plan — emptying wait_healthy_all's entire
    // body still passes it (robustness review R7-health). This test runs LIVE
    // and asserts the POSITIVE claim the old test's name promised but never checked: every
    // host in the list is actually curled over its own SSH connection.
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_all(["web1", "web2", "web3"], 3000, #{ attempts: 1 });
           state_set("passed", "true");"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("web1", "curl -s -o /dev/null", "200");
    fake.respond_cmd("web2", "curl -s -o /dev/null", "200");
    fake.respond_cmd("web3", "curl -s -o /dev/null", "200");
    let ctx = shared(fake.clone());
    run_file(&main, ctx.clone()).unwrap();
    assert_eq!(
        ctx.state.lock().unwrap().get("passed").as_deref(),
        Some("true")
    );

    let calls = fake.calls();
    for host in ["web1", "web2", "web3"] {
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with(&format!("ssh {host}: ")) && c.contains("curl")),
            "must actually curl {host} over its own ssh connection, not just skip it: {calls:?}"
        );
    }
}

#[test]
fn wait_healthy_all_fails_fast_and_never_probes_a_later_host() {
    // The sibling half of the coverage gap above: when an EARLIER host is unhealthy,
    // wait_healthy_all must throw (propagating wait_healthy_on_host's own exhaustion error)
    // WITHOUT ever probing a LATER host in the list — proving the sequential loop is fail-fast,
    // not "probe all, then report."
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_all(["web1", "web2"], 3000, #{ attempts: 1, interval: 0 });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("web1", "curl -s -o /dev/null", "503");
    fake.respond_cmd("web2", "curl -s -o /dev/null", "200");
    let err = run_file(&main, shared(fake.clone())).unwrap_err();
    assert!(err.contains("Health check failed on web1"), "got: {err}");

    let calls = fake.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("ssh web2: ")),
        "must never probe web2 once web1 has already failed: {calls:?}"
    );
}

#[test]
fn wait_healthy_on_host_float_timeout_reaches_curl_shell_quoted() {
    // A fractional timeout is accepted (curl --max-time takes fractions) and is spliced into
    // the remote curl command through sh_quote, like every other interpolated value. A live
    // FakeRunner run captures the exact ssh command (dry-run returns before building it).
    let dir = tempfile::tempdir().unwrap();
    let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"import "lib/healthcheck" as health;
           health::wait_healthy_on_host("web1", 8080, #{ attempts: 1, interval: 0, timeout: 2.5 });"#,
    )
    .unwrap();

    let fake = FakeRunner::shared();
    fake.respond_cmd("web1", "curl", "200");
    run_file(&main, shared(fake.clone())).unwrap();
    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.contains("--max-time '2.5' ")),
        "float timeout must reach curl single-quoted: {calls:?}"
    );
}
