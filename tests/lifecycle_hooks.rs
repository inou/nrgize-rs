//! Integration: optional `hook_pre_deploy` / `hook_post_deploy` / `hook_post_rollback` lifecycle
//! hook functions (roadmap 2.6). These are plain Rhai functions the ORCHESTRATION FILE may define
//! at its own top level; `deploy()`/`rollback()` (in `lib/deploy.rhai`) call them back by exact
//! name+arity via Rhai's `is_def_fn`/`Fn(name).call(...)` if and only if they exist — verified
//! empirically (via a standalone scratch check before implementing) that this correctly reaches
//! across the module-import boundary from the stdlib's own functions back into the top-level
//! script's functions, not just within the same file.
//!
//! NOTE: `hook_post_rollback` only fires when `rollback()` is called from WITHIN the
//! orchestration file's own code (e.g. a project-authored `rollback` task function, or a script
//! that calls `deploy::rollback(...)` directly) — NOT via the native `nrg rollback <service>` CLI
//! command, which synthesizes its own standalone script with no access to the project's
//! Energize.rhai functions at all (see `engine::eval::run_rollback`'s own doc comment).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

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

fn run(script: &str) -> (bool, String) {
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
        .output()
        .unwrap();
    // Rhai's print() goes to stderr (engine::mod.rs's on_print), while the formatted dry-run
    // PLAN goes to stdout — concatenate both so a single string can assert on either.
    let combined = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), combined)
}

const BASE_DEPLOY: &str = r#"
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#;

#[test]
fn hook_pre_deploy_runs_before_any_deploy_work() {
    let (ok, out) = run(&format!(
        r#"
        import "lib/deploy" as deploy;
        fn hook_pre_deploy(service, image, hosts) {{
            print("HOOK PRE " + service + " " + image + " " + hosts.len());
        }}
        {BASE_DEPLOY}
    "#
    ));
    assert!(ok, "deploy should still succeed:\n{out}");
    let hook_pos = out
        .find("HOOK PRE app ghcr.io/org/app:v9 1")
        .unwrap_or_else(|| panic!("hook_pre_deploy was never called:\n{out}"));
    // Both "HOOK PRE ..." (the hook's own print()) and "--- Pull ---" (deploy()'s own print()
    // right before docker_pull_all) go through the SAME print() sink (stderr) in real
    // chronological order, so comparing their positions in the combined output is reliable —
    // unlike comparing against the formatted PLAN text (stdout), which is rendered as a whole
    // only at the very end and would give a meaningless ordering result.
    let pull_pos = out
        .find("--- Pull ---")
        .unwrap_or_else(|| panic!("deploy never reached the pull phase:\n{out}"));
    assert!(
        hook_pos < pull_pos,
        "hook_pre_deploy must run BEFORE any deploy work (pull/build/push):\n{out}"
    );
}

#[test]
fn hook_pre_deploy_can_block_the_deploy_by_throwing() {
    let (ok, out) = run(&format!(
        r#"
        import "lib/deploy" as deploy;
        fn hook_pre_deploy(service, image, hosts) {{
            throw "not during the freeze window";
        }}
        {BASE_DEPLOY}
    "#
    ));
    assert!(!ok, "a throwing hook_pre_deploy must abort the deploy:\n{out}");
    assert!(
        !out.contains("pull 'ghcr.io/org/app:v9'"),
        "no deploy work should have happened once hook_pre_deploy blocked it:\n{out}"
    );
}

#[test]
fn hook_post_deploy_runs_after_the_deploy_completes_and_receives_the_right_args() {
    let (ok, out) = run(&format!(
        r#"
        import "lib/deploy" as deploy;
        fn hook_post_deploy(service, image, hosts) {{
            print("HOOK POST " + service + " " + image + " " + hosts.len());
        }}
        {BASE_DEPLOY}
    "#
    ));
    assert!(ok, "deploy should succeed:\n{out}");
    let complete_pos = out
        .find("Deploy complete")
        .unwrap_or_else(|| panic!("deploy never reported completion:\n{out}"));
    let hook_pos = out
        .find("HOOK POST app ghcr.io/org/app:v9 1")
        .unwrap_or_else(|| panic!("hook_post_deploy was never called:\n{out}"));
    assert!(
        complete_pos < hook_pos,
        "hook_post_deploy must run AFTER the deploy has already committed:\n{out}"
    );
}

#[test]
fn hook_post_deploy_throwing_does_not_fail_an_already_successful_deploy() {
    let (ok, out) = run(&format!(
        r#"
        import "lib/deploy" as deploy;
        fn hook_post_deploy(service, image, hosts) {{
            throw "webhook is down";
        }}
        {BASE_DEPLOY}
    "#
    ));
    assert!(
        ok,
        "a throwing hook_post_deploy must NOT turn an already-successful deploy into a \
         reported failure (best-effort, matching run_post_deploy_hook's own convention):\n{out}"
    );
    assert!(
        out.contains("hook_post_deploy threw") && out.contains("webhook is down"),
        "the failure must still be reported loudly, not silently swallowed:\n{out}"
    );
}

#[test]
fn a_hook_defined_with_the_wrong_arity_is_treated_as_not_defined() {
    // is_def_fn checks NAME + ARITY together — a 1-arg hook_post_deploy must not be mistaken for
    // the real 3-arg hook, exactly the same way a caller-supplied cfg key can't silently coerce.
    let (ok, out) = run(&format!(
        r#"
        import "lib/deploy" as deploy;
        fn hook_post_deploy(service) {{
            print("WRONG ARITY HOOK CALLED");
        }}
        {BASE_DEPLOY}
    "#
    ));
    assert!(ok, "deploy should still succeed:\n{out}");
    assert!(
        !out.contains("WRONG ARITY HOOK CALLED"),
        "a hook defined with the wrong arity must be treated as undefined, not called:\n{out}"
    );
}

#[test]
fn no_hooks_defined_is_the_default_and_changes_nothing() {
    let (ok, out) = run(&format!("import \"lib/deploy\" as deploy;\n{BASE_DEPLOY}"));
    assert!(ok, "deploy with no hooks defined must behave exactly as before this feature:\n{out}");
    assert!(!out.contains("HOOK"), "no hook output should appear when none are defined:\n{out}");
}

#[test]
fn hook_post_rollback_fires_in_addition_to_hook_post_deploy_when_rollback_is_called_from_the_orchestration_file(
) {
    // rollback() calls deploy() internally, so hook_post_deploy fires too (a rollback IS a
    // deploy of a different image) — hook_post_rollback fires ADDITIONALLY, letting a caller's
    // hooks distinguish "routine deploy" from "this was specifically a rollback".
    let (ok, out) = run(
        r#"
        import "lib/deploy" as deploy;
        fn hook_post_deploy(service, image, hosts) {
            print("HOOK POST DEPLOY " + service + " " + image);
        }
        fn hook_post_rollback(service, image, hosts) {
            print("HOOK POST ROLLBACK " + service + " " + image + " " + hosts.len());
        }
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app");
    "#,
    );
    assert!(ok, "rollback should succeed:\n{out}");
    assert!(
        out.contains("HOOK POST DEPLOY app ghcr.io/org/app:v1"),
        "the nested deploy()'s own hook_post_deploy must still fire during a rollback:\n{out}"
    );
    assert!(
        out.contains("HOOK POST ROLLBACK app ghcr.io/org/app:v1 1"),
        "hook_post_rollback must fire after a successful rollback:\n{out}"
    );
}

#[test]
fn hook_post_rollback_throwing_does_not_fail_an_already_successful_rollback() {
    let (ok, out) = run(
        r#"
        import "lib/deploy" as deploy;
        fn hook_post_rollback(service, image, hosts) {
            throw "pager is down";
        }
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app");
    "#,
    );
    assert!(ok, "a throwing hook_post_rollback must not fail an already-successful rollback:\n{out}");
    assert!(
        out.contains("hook_post_rollback threw") && out.contains("pager is down"),
        "the failure must still be reported loudly:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// notify.rhai
// ---------------------------------------------------------------------------

#[test]
fn notify_webhook_posts_the_payload_verbatim() {
    let (ok, out) = run(
        r#"
        import "lib/notify" as notify;
        notify::webhook("https://example.com/hook", "{\"raw\":true}");
    "#,
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("[assumed ok] POST https://example.com/hook"),
        "must go through the dry-run-safe http_post builtin (never a real request):\n{out}"
    );
}

#[test]
fn notify_slack_sends_a_correctly_escaped_json_payload_over_the_real_wire() {
    // http_post's dry-run recording only shows the URL, never the body — a dry-run-only test
    // can't actually prove notify::slack builds a well-formed, correctly-escaped payload. This
    // runs LIVE (no --dry-run) against a real local TCP listener and inspects the exact bytes
    // the server received, the same pattern http_post's own Rust unit test
    // (http_post_sends_its_body_and_extracts_a_real_response) uses.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            stream.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        }
    });

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
            import "lib/notify" as notify;
            notify::slack("http://{addr}/", "app \"v9\" is live");
            "#
        ),
    )
    .unwrap();

    // LIVE (no --dry-run): http_post only executes for real outside dry-run mode.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success();

    let received = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("server never received a request");
    assert!(
        received.contains("{\"text\":\"app \\\"v9\\\" is live\"}"),
        "the JSON payload must be exactly {{\"text\": ...}} with the message quote-escaped:\n{received}"
    );
    assert!(
        received.to_lowercase().contains("content-type: application/json"),
        "http_post must send the JSON content type:\n{received}"
    );
}

#[test]
fn notify_webhook_accepts_a_secret_url_and_reveals_it_before_posting() {
    // Opus review: notify.rhai's own documented usage example passes a bare secret("...") as the
    // url — but http_post only accepts a plain string, so a Secret used to make BOTH webhook and
    // slack throw "Function not found: http_post (Secret, ...)" at runtime. Since the post-deploy
    // hook this is meant to be called from is best-effort, that throw would have been silently
    // swallowed (only a [warn] printed) — the notification would just never send, with no loud
    // failure. webhook() must accept a Secret url and reveal it internally.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            stream.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        }
    });

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/notify" as notify;
        notify::webhook(secret("WEBHOOK_URL"), "{\"raw\":true}");
        "#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .env("NRG_SECRET_WEBHOOK_URL", format!("http://{addr}/"))
        .assert()
        .success();

    let received = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("server never received a request — the Secret URL was never revealed/posted to");
    assert!(
        received.contains("{\"raw\":true}"),
        "must actually post to the revealed Secret URL:\n{received}"
    );
}
