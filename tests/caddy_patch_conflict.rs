//! Robustness review R13: `lib/caddy.rhai`'s `proxy_deploy` used to run
//! `curl -fsS -X PATCH <url> -d <body> || curl -fsS -X POST <url2> -d <body>` as one shell
//! one-liner. `curl -fsS` treats ANY non-2xx response (or a connection-level failure) as a
//! nonzero exit, so this couldn't tell "the route doesn't exist yet" (404, expected on a first
//! deploy — fall through to POST) from a TRANSIENT failure on an EXISTING route (a timeout, a
//! 400, a 500) — both took the same `|| POST` branch, which APPENDS a brand-new route instead of
//! replacing the existing one. Caddy matches the FIRST route in its array, so the stale upstream
//! kept serving traffic while `nrg` reported success.
//!
//! The fix captures PATCH's real HTTP status code and only falls through to POST on an EXACT
//! "404"; anything else fails loudly. This is verified here by extracting the REAL shell command
//! `proxy_deploy` builds (via a dry-run plan, which shows the exact string that would run live)
//! and executing that exact string with a real `/bin/sh`, backed by a fake `curl` on `PATH` that
//! reports a chosen HTTP status for the PATCH call and logs every invocation it received — proving
//! the shell logic itself branches correctly for a 404, a 500, and a 200, not just that the Rhai
//! source superficially changed.

use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;

/// Symlink the repo's real `lib/` into `dir` so `import "lib/caddy"` resolves.
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

/// Get the exact shell command `proxy_deploy` builds for a plain (no domain, no health_path)
/// traffic switch, by reading it straight out of a dry-run plan (the same string that would be
/// handed to `ssh` live — see `docker_mutation`/`effect`, which records `cmd` verbatim).
fn proxy_deploy_cmd() -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"import "lib/caddy" as proxy; proxy::proxy_deploy("host1", "app", "localhost:13000", #{});"#,
    )
    .unwrap();
    let out = assert_cmd::Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["exec", "--dry-run", "Energize.rhai"])
        .assert()
        .success()
        .get_output()
        .clone();
    let plan = String::from_utf8_lossy(&out.stdout).into_owned();
    let start = plan
        .find("code=$(curl")
        .unwrap_or_else(|| panic!("plan is missing the caddy PATCH command:\n{plan}"));
    let rest = &plan[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim_end().to_string()
}

/// Write a fake `curl` to `<dir>/curl` that:
/// - logs its full argv (one invocation per line) to `log_path`
/// - if invoked with `-X PATCH`, prints `$PATCH_STATUS` to stdout (standing in for curl's real
///   `-w '%{http_code}'` output) and otherwise prints nothing (standing in for a real POST, which
///   this script has nothing useful to say about beyond "it ran")
/// - always exits 0 — matching real curl's behavior of exiting 0 for an HTTP-level error response
///   as long as it got a response at all; only a genuine connection failure would make real curl
///   exit nonzero, which is a separate, already-covered case (`code` would be empty/"000").
fn fake_curl_bin(dir: &Path, log_path: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> {log_path:?}\n\
         method=\"\"\n\
         prev=\"\"\n\
         for a in \"$@\"; do\n\
         \x20 case \"$prev\" in\n\
         \x20   -X) method=\"$a\" ;;\n\
         \x20 esac\n\
         \x20 prev=\"$a\"\n\
         done\n\
         if [ \"$method\" = PATCH ]; then\n\
         \x20 printf '%s' \"$PATCH_STATUS\"\n\
         fi\n\
         exit 0\n"
    );
    let bin = dir.join("curl");
    fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

/// Run `cmd` under a real `/bin/sh -c`, with a fake `curl` on `PATH` that reports `patch_status`
/// for the PATCH call. Returns (exit success, logged curl invocations, stderr).
fn run_with_fake_curl(cmd: &str, patch_status: &str) -> (bool, Vec<String>, String) {
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("curl_argv.log");
    fake_curl_bin(bin.path(), &log);

    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("PATH", path)
        .env("PATCH_STATUS", patch_status)
        .output()
        .unwrap();

    let calls = fs::read_to_string(&log).unwrap_or_default().lines().map(|s| s.to_string()).collect();
    (out.status.success(), calls, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn patch_404_falls_through_to_post() {
    let cmd = proxy_deploy_cmd();
    let (ok, calls, _stderr) = run_with_fake_curl(&cmd, "404");
    assert!(ok, "a 404-then-POST sequence must succeed overall: {calls:?}");
    assert!(calls.iter().any(|c| c.contains("PATCH")), "PATCH must have been attempted: {calls:?}");
    assert!(
        calls.iter().any(|c| c.contains("POST")),
        "a 404 on PATCH (route doesn't exist yet) must fall through to POST: {calls:?}"
    );
}

#[test]
fn patch_200_succeeds_without_ever_calling_post() {
    let cmd = proxy_deploy_cmd();
    let (ok, calls, _stderr) = run_with_fake_curl(&cmd, "200");
    assert!(ok, "a 200 on PATCH must succeed on its own: {calls:?}");
    assert!(
        !calls.iter().any(|c| c.contains("POST")),
        "a successful PATCH (200) must NOT also POST a duplicate route: {calls:?}"
    );
}

#[test]
fn patch_500_fails_loudly_without_duplicating_the_route_via_post() {
    // Robustness review R13's actual bug: a transient non-404 failure on an EXISTING route used to
    // silently fall through to POST (appending a duplicate route) instead of failing.
    let cmd = proxy_deploy_cmd();
    let (ok, calls, stderr) = run_with_fake_curl(&cmd, "500");
    assert!(!ok, "a transient 500 on PATCH must fail the whole command, not silently succeed");
    assert!(
        !calls.iter().any(|c| c.contains("POST")),
        "a 500 on PATCH (an EXISTING route, transient failure) must NOT fall through to POST — \
         that would append a duplicate route while the stale upstream keeps serving traffic: {calls:?}"
    );
    assert!(
        stderr.contains("500"),
        "the failure should name the HTTP status so an operator can tell a transient error from \
         a genuine 404: {stderr:?}"
    );
}

#[test]
fn patch_connection_failure_reported_as_000_also_fails_without_post() {
    // A curl-level failure (couldn't connect, DNS, etc.) reports as http_code "000" — must be
    // treated the same as any other non-404/non-2xx failure, not silently treated as "route absent".
    let cmd = proxy_deploy_cmd();
    let (ok, calls, _stderr) = run_with_fake_curl(&cmd, "000");
    assert!(!ok, "a connection-level PATCH failure (\"000\") must fail the whole command");
    assert!(
        !calls.iter().any(|c| c.contains("POST")),
        "\"000\" must NOT be treated as \"route absent\" and fall through to POST: {calls:?}"
    );
}
