//! Integration: `lib/bunny.rhai` — the Bunny Magic Containers deploy target (roadmap 2.9, Phase 2).
//!
//! Follows `tests/lifecycle_hooks.rs`'s pattern: `link_lib`/`run` helpers for `--dry-run` scripts
//! whose recorded plan text is asserted on directly, and a real local `TcpListener` (never a
//! mocked `ureq`) for live-request assertions — this codebase's own established convention, see
//! `src/engine/builtins/http.rs`'s own tests.
//!
//! `cfg.base_url` (an undocumented-by-default override, see `lib/bunny.rhai`'s own comment) is
//! what lets these tests point the module at a real local listener instead of the actual Bunny
//! API.

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

/// Run `script` as `Energize.rhai` under `--dry-run` in a fresh temp project. Returns
/// (succeeded, combined stdout+stderr) — print() goes to stderr, the formatted PLAN to stdout.
fn run_dry(script: &str) -> (bool, String) {
    run_in(script, &tempfile::tempdir().unwrap(), true)
}

/// Run `script` LIVE (no --dry-run) in a fresh temp project.
fn run_live(script: &str) -> (bool, String) {
    run_in(script, &tempfile::tempdir().unwrap(), false)
}

fn run_in(script: &str, dir: &tempfile::TempDir, dry_run: bool) -> (bool, String) {
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    if !dir.path().join("lib").exists() {
        link_lib(dir.path());
    }
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    let mut cmd = Command::cargo_bin("nrg").unwrap();
    cmd.current_dir(dir.path()).arg("exec");
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.arg("Energize.rhai");
    let out = cmd.output().unwrap();
    let combined = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), combined)
}

/// Bind an ephemeral localhost listener that serves `responses` in order — one accepted
/// connection per response, "Connection: close" so `ureq` never reuses the socket across the
/// GET-then-PATCH sequence `deploy_app`/`rollback_app` actually issue. Returns the address and a
/// receiver yielding each request's raw bytes, in the order received.
fn spawn_bunny_responder(
    responses: Vec<&'static str>,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for response in responses {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
                let mut received = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            received.extend_from_slice(&buf[..n]);
                            // A request line + headers + (for PATCH) a short JSON body always
                            // arrives well within one read on loopback; stop once we've seen the
                            // blank-line header terminator and, if Content-Length was declared,
                            // that many body bytes — avoids blocking out the read timeout on
                            // every single request.
                            let text = String::from_utf8_lossy(&received);
                            if let Some(header_end) = text.find("\r\n\r\n") {
                                let body_len = text
                                    .lines()
                                    .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                                    .unwrap_or(0);
                                if received.len() >= header_end + 4 + body_len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&received).into_owned());
                let _ = stream.write_all(response.as_bytes());
            }
        }
    });
    (addr, rx)
}

fn app_config_response(containers: &str) -> String {
    let body = format!("{{\"id\":\"app1\",\"containerTemplates\":[{containers}]}}");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn patch_ok_response() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 11\r\n\r\n{\"ok\":true}"
}

// ---------------------------------------------------------------------------
// deploy_app — the happy path
// ---------------------------------------------------------------------------

#[test]
fn deploy_app_finds_the_named_container_and_patches_its_image_with_the_access_key_header() {
    let (addr, rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(
                r#"{"id":"c-other","name":"worker","imageTag":"old-worker"},{"id":"c-web","name":"web","imageTag":"v1"}"#,
            )
            .into_boxed_str(),
        ),
        patch_ok_response(),
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        print("STATUS=" + r.status);
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("STATUS=200"), "{out}");

    let get_req = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("no GET received");
    assert!(get_req.contains("GET /mc/apps/app1"), "{get_req}");
    assert!(get_req.to_lowercase().contains("accesskey: testkey"), "{get_req}");

    let patch_req = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("no PATCH received");
    assert!(
        patch_req.contains("PATCH /mc/apps/app1/containers/c-web"),
        "must PATCH the MATCHED container's id (c-web), not the app id or container name:\n{patch_req}"
    );
    assert!(patch_req.to_lowercase().contains("accesskey: testkey"), "{patch_req}");
    assert!(
        patch_req.contains("\"imageTag\":\"v2\""),
        "PATCH body must carry the new image tag:\n{patch_req}"
    );
    assert!(
        patch_req.contains("\"id\":\"c-web\""),
        "PATCH body must carry the matched container's own id:\n{patch_req}"
    );
}

#[test]
fn deploy_app_snapshots_the_previous_tag_into_state_for_a_later_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);

    let (ok, out) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok, "{out}");

    let (ok2, out2) = run_in(
        r#"print("PREV=" + state_get("bunny.app1.web.prev"));"#,
        &dir,
        false,
    );
    assert!(ok2, "{out2}");
    assert!(
        out2.contains("PREV=v1"),
        "deploy_app must snapshot the pre-deploy tag so rollback_app can revert to it:\n{out2}"
    );
}

#[test]
fn deploy_app_skips_the_snapshot_when_the_container_reports_no_image_tag() {
    // Opus review: the whole module hinges on containerTemplates[i].imageTag being the right
    // field name (flagged as an inference in lib/bunny.rhai's own header comment, since every
    // doc page that could confirm it returned HTTP 403 during research) — the one path that
    // exercises what happens when it's ABSENT (a nonexistent field reads as unit in Rhai) was
    // untested. deploy_app must take its documented "skip the snapshot" branch, not crash or
    // silently record a bogus ".prev" state entry.
    let dir = tempfile::tempdir().unwrap();
    let (addr, _rx) = spawn_bunny_responder(vec![
        Box::leak(app_config_response(r#"{"id":"c-web","name":"web"}"#).into_boxed_str()),
        patch_ok_response(),
    ]);

    let (ok, out) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok, "deploy_app must still succeed even with no reported current tag:\n{out}");
    assert!(
        out.contains("skipping the rollback snapshot"),
        "must print the documented skip notice:\n{out}"
    );

    let (ok2, out2) = run_in(
        r#"print("HAS_PREV=" + has_state("bunny.app1.web.prev"));"#,
        &dir,
        false,
    );
    assert!(ok2, "{out2}");
    assert!(
        out2.contains("HAS_PREV=false"),
        "no .prev snapshot should have been written when the container reported no imageTag:\n{out2}"
    );
}

#[test]
fn deploy_app_sends_the_optional_image_name_and_image_digest_fields() {
    let (addr, rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            image_name: "ghcr.io/acme/app", image_digest: "sha256:deadbeef",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(ok, "{out}");

    let _get = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let patch_req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        patch_req.contains("\"imageName\":\"ghcr.io/acme/app\""),
        "PATCH body must carry cfg.image_name under the exact field name imageName:\n{patch_req}"
    );
    assert!(
        patch_req.contains("\"imageDigest\":\"sha256:deadbeef\""),
        "PATCH body must carry cfg.image_digest under the exact field name imageDigest:\n{patch_req}"
    );
}

#[test]
fn zero_matching_containers_throws_a_clear_error_naming_the_container() {
    let (addr, _rx) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(r#"{"id":"c-other","name":"worker","imageTag":"x"}"#).into_boxed_str(),
    )]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(!ok, "must throw when no container matches");
    assert!(
        out.contains("could not find a container named \"web\""),
        "error must name the missing container:\n{out}"
    );
}

#[test]
fn more_than_one_matching_container_throws_a_clear_error_naming_the_container() {
    let (addr, _rx) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(
            r#"{"id":"c-1","name":"web","imageTag":"v1"},{"id":"c-2","name":"web","imageTag":"v1"}"#,
        )
        .into_boxed_str(),
    )]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(!ok, "must throw when more than one container matches");
    assert!(
        out.contains("found more than one container named \"web\""),
        "error must name the ambiguous container:\n{out}"
    );
}

#[test]
fn a_non_200_get_throws_naming_the_real_status() {
    let (addr, _rx) = spawn_bunny_responder(vec![
        "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "bad-id", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(!ok, "must throw on a 400 GET");
    assert!(
        out.contains("bad-id") && out.to_lowercase().contains("400"),
        "error must mention the app_id and the real HTTP 400:\n{out}"
    );
}

#[test]
fn a_200_response_missing_containertemplates_throws_a_clear_error_instead_of_a_raw_iteration_failure(
) {
    // Fable final review: find_container's containerTemplates.contains() guard had no test of
    // its own — a 200 GET whose body doesn't even have the expected shape must produce a clean,
    // named error, not a raw Rhai "cannot iterate over ()"-style failure.
    let (addr, _rx) = spawn_bunny_responder(vec![
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 13\r\n\r\n{\"foo\":\"bar\"}",
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(!ok, "must throw on a response with no containerTemplates");
    assert!(
        out.contains("containerTemplates"),
        "error must clearly name the missing field, not surface a raw iteration failure:\n{out}"
    );
}

#[test]
fn a_non_200_patch_throws_naming_the_real_status() {
    let (addr, _rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        "HTTP/1.1 409 Conflict\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        "#
    ));
    assert!(!ok, "must throw on a non-200 PATCH");
    assert!(
        out.contains("409"),
        "error must mention the real HTTP 409 from the PATCH:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// rollback_app
// ---------------------------------------------------------------------------

#[test]
fn rollback_app_reads_the_snapshotted_tag_and_patches_back_to_it_without_re_snapshotting() {
    let dir = tempfile::tempdir().unwrap();

    // 1. A deploy that snapshots v1 as prev and moves to v2.
    let (addr1, _rx1) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);
    let (ok, out) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr1}",
        }});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok, "{out}");

    // 2. Roll back — must PATCH back to v1 (the snapshotted prev), read from state, not from a
    //    fresh cfg.image_tag (rollback_app's cfg never carries one).
    let (addr2, rx2) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v2"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);
    let (ok2, out2) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::rollback_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", base_url: "http://{addr2}",
        }});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok2, "{out2}");
    let _get = rx2.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let patch_req = rx2.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        patch_req.contains("\"imageTag\":\"v1\""),
        "rollback must PATCH back to the snapshotted prev tag (v1), not any other value:\n{patch_req}"
    );

    // 3. `.prev` must STILL be v1 — rollback must not have re-snapshotted the value it rolled
    //    back FROM (v2), which would corrupt the single-snapshot rollback chain.
    let (ok3, out3) = run_in(
        r#"print("PREV=" + state_get("bunny.app1.web.prev"));"#,
        &dir,
        false,
    );
    assert!(ok3, "{out3}");
    assert!(
        out3.contains("PREV=v1"),
        "rollback_app must NOT overwrite the .prev snapshot:\n{out3}"
    );
}

#[test]
fn rollback_app_never_forwards_a_reused_cfgs_image_name_or_image_digest() {
    // Fable final review: deploy_app and rollback_app share one cfg shape, so reusing the SAME
    // cfg map for both calls (build cfg once, deploy_app(cfg), later rollback_app(cfg)) is a
    // natural calling pattern. patch_container used to read image_name/image_digest straight off
    // whatever cfg it was given — so a rollback using a cfg that still carried the NEW deploy's
    // image_name/image_digest would silently PATCH those alongside the OLD (rolled-back-to) tag.
    // Since a digest pin normally takes precedence over a tag, that made "rollback" silently
    // keep the bad image while still reporting success. The PATCH body during a rollback must
    // always be exactly {id, imageTag} — no imageName/imageDigest key at all.
    let dir = tempfile::tempdir().unwrap();
    let (addr1, _rx1) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);
    // The SAME cfg map, reused verbatim for rollback_app below — this is the realistic
    // reuse pattern that would leak image_name/image_digest if patch_container read them off cfg.
    let cfg = format!(
        r#"#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            image_name: "ghcr.io/acme/app", image_digest: "sha256:deadbeef",
            base_url: "http://{addr1}",
        }}"#
    );
    let (ok, out) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app({cfg});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok, "{out}");

    let (addr2, rx2) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v2"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);
    let cfg2 = cfg.replace(&format!("http://{addr1}"), &format!("http://{addr2}"));
    let (ok2, out2) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::rollback_app({cfg2});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok2, "{out2}");

    let _get = rx2.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let patch_req = rx2.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        !patch_req.contains("imageName") && !patch_req.contains("imageDigest"),
        "rollback's PATCH body must never carry a reused cfg's image_name/image_digest:\n{patch_req}"
    );
    assert!(
        patch_req.contains("\"imageTag\":\"v1\""),
        "rollback must still PATCH back to the snapshotted prev tag:\n{patch_req}"
    );
}

#[test]
fn rollback_app_throws_a_clear_error_when_nothing_was_ever_deployed() {
    let (ok, out) = run_live(
        r#"
        import "lib/bunny" as bunny;
        bunny::rollback_app(#{app_id: "app1", api_key: "testkey", container: "web"});
        "#,
    );
    assert!(!ok, "must throw when there is no prev snapshot");
    assert!(
        out.contains("nothing to roll back to"),
        "error must explain there's no snapshot to roll back to:\n{out}"
    );
}

#[test]
fn rollback_app_under_dry_run_still_makes_a_real_get_but_never_a_real_patch() {
    // Opus review: deploy_app's dry-run path was thoroughly guarded (a listener that only ever
    // gets ONE canned response, so a real second connection for the PATCH would leave the test
    // hanging on recv_timeout) — rollback_app had no equivalent, despite sharing the exact same
    // patch_container short-circuit.
    let dir = tempfile::tempdir().unwrap();
    let (addr1, _rx1) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);
    let (ok, out) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr1}",
        }});
        "#
        ),
        &dir,
        false,
    );
    assert!(ok, "{out}");

    let (addr2, rx2) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v2"}"#).into_boxed_str(),
    )]);
    let (ok2, out2) = run_in(
        &format!(
            r#"
        import "lib/bunny" as bunny;
        let r = bunny::rollback_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", base_url: "http://{addr2}",
        }});
        print("STATUS=" + r.status);
        "#
        ),
        &dir,
        true,
    );
    assert!(ok2, "{out2}");
    assert!(out2.contains("STATUS=200"), "PATCH must synthesize a 200 under dry-run:\n{out2}");
    assert!(
        out2.contains("[assumed ok] PATCH"),
        "the PATCH must be recorded as short-circuited, never really sent:\n{out2}"
    );

    let _get = rx2.recv_timeout(std::time::Duration::from_secs(5)).expect("the GET must be real");
    assert!(
        rx2.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "no second (PATCH) connection should ever have been attempted under --dry-run"
    );
}

// ---------------------------------------------------------------------------
// dry-run: GET is honest, PATCH short-circuits, matching Phase 1's semantics exactly
// ---------------------------------------------------------------------------

#[test]
fn deploy_app_under_dry_run_still_makes_a_real_get_but_never_a_real_patch() {
    let (addr, rx) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
    )]);

    let (ok, out) = run_dry(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_app(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        print("STATUS=" + r.status);
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("STATUS=200"), "PATCH must synthesize a 200 under dry-run:\n{out}");
    assert!(
        out.contains("GET http://") && out.contains("-> 200 (probed live)"),
        "the GET must be recorded as a REAL probe, per Phase 1's http_get semantics:\n{out}"
    );
    assert!(
        out.contains("[assumed ok] PATCH"),
        "the PATCH must be recorded as short-circuited, never really sent:\n{out}"
    );
    assert!(
        out.contains("bunny.app1.web.prev = v1"),
        "the rollback snapshot must still be recorded in the dry-run plan:\n{out}"
    );

    // The listener was only given ONE canned response (for the GET) — if a real PATCH had gone
    // out, `spawn_bunny_responder`'s thread would still be blocked in its second `accept()`, and
    // this `recv_timeout` would simply time out (no third message was ever sent). Draining the
    // one message we DO expect and then confirming there's nothing else proves no second
    // connection (the PATCH) was ever attempted.
    let _get = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the GET must be real");
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "no second (PATCH) connection should ever have been attempted under --dry-run"
    );
}

// ---------------------------------------------------------------------------
// wait_for_image
// ---------------------------------------------------------------------------

#[test]
fn wait_for_image_succeeds_once_the_polled_tag_matches() {
    let (addr, _rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v2"}"#).into_boxed_str(),
        ),
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::wait_for_image(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}", attempts: 5, interval: 0,
        }});
        print("DONE");
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("DONE"), "{out}");
}

#[test]
fn wait_for_image_throws_after_exhausting_attempts_naming_the_last_seen_tag() {
    let (addr, _rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::wait_for_image(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}", attempts: 2, interval: 0,
        }});
        "#
    ));
    assert!(!ok, "must throw once attempts are exhausted");
    assert!(
        out.contains("never reported image \"v2\"") && out.contains("last seen: \"v1\""),
        "error must name both the target tag and the last-seen tag:\n{out}"
    );
}

#[test]
fn wait_for_image_under_dry_run_never_polls_at_all() {
    // No responses configured — if wait_for_image made even one real GET under --dry-run, this
    // test would hang until the listener thread's implicit accept() never returns, which
    // `recv_timeout` below turns into a clean failure instead of a hang.
    let (addr, rx) = spawn_bunny_responder(vec![]);

    let (ok, out) = run_dry(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::wait_for_image(#{{
            app_id: "app1", api_key: "testkey", container: "web", image_tag: "v2",
            base_url: "http://{addr}",
        }});
        print("DONE");
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("DONE"), "{out}");
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "wait_for_image must never poll under --dry-run:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// api_key as a Secret
// ---------------------------------------------------------------------------

#[test]
fn api_key_accepted_as_a_bare_secret_and_revealed_before_use() {
    let (addr, rx) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        patch_ok_response(),
    ]);

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
            import "lib/bunny" as bunny;
            bunny::deploy_app(#{{
                app_id: "app1", api_key: secret("BUNNY_KEY"), container: "web", image_tag: "v2",
                base_url: "http://{addr}",
            }});
            "#
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .env("NRG_SECRET_BUNNY_KEY", "real-secret-key")
        .assert()
        .success();

    let get_req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        get_req.to_lowercase().contains("accesskey: real-secret-key"),
        "a bare secret() api_key must be revealed and sent as the AccessKey header:\n{get_req}"
    );
}

// ---------------------------------------------------------------------------
// deploy_fleet — Phase 3: canary-then-batched rollout
// ---------------------------------------------------------------------------

/// A listener that accepts a connection (proving it was contacted) but never answers — used to
/// assert a target was (or was NOT) reached at all, independent of what it would have returned.
fn spawn_bunny_probe_listener() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if listener.accept().is_ok() {
            let _ = tx.send(());
        }
    });
    (addr, rx)
}

/// A mock target whose deploy fully succeeds AND propagates: GET (old tag) for the initial
/// find/snapshot, PATCH ok, then a SECOND GET (new tag) for deploy_batch/deploy_one_target's
/// own wait_for_image call, which polls current_image_tag again after a successful PATCH — a
/// target that's only going to succeed the PATCH still needs this third response queued, or the
/// listener runs out of canned responses mid-poll and the whole thing reads as a transport
/// failure instead of a genuine propagation check.
fn ok_target_responder(
    old_tag: &str,
    new_tag: &str,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(&format!(r#"{{"id":"c-web","name":"web","imageTag":"{old_tag}"}}"#))
                .into_boxed_str(),
        ),
        patch_ok_response(),
        Box::leak(
            app_config_response(&format!(r#"{{"id":"c-web","name":"web","imageTag":"{new_tag}"}}"#))
                .into_boxed_str(),
        ),
    ])
}

#[test]
fn deploy_fleet_rolls_out_canary_then_the_rest_and_reports_all_successes() {
    let (addr_a, _rx_a) = ok_target_responder("v1", "v2");
    let (addr_b, _rx_b) = ok_target_responder("v1", "v2");
    let (addr_c, _rx_c) = ok_target_responder("v1", "v2");

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_fleet([
            #{{app_id: "app-a", container: "web", image_tag: "v2", base_url: "http://{addr_a}"}},
            #{{app_id: "app-b", container: "web", image_tag: "v2", base_url: "http://{addr_b}"}},
            #{{app_id: "app-c", container: "web", image_tag: "v2", base_url: "http://{addr_c}"}},
        ], #{{api_key: "testkey", canary_size: 1, batch_size: 5}});
        print("SUCCEEDED=" + r.succeeded.len() + " FAILED=" + r.failed.len());
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("SUCCEEDED=3 FAILED=0"), "{out}");
}

#[test]
fn deploy_fleet_aborts_during_canary_before_touching_the_rest_of_the_fleet() {
    // Target "app-a" (the sole canary, canary_size defaults to 1) fails outright — its listener
    // returns a 400 on the GET. With the default max_failures: 0, that single canary failure must
    // abort the WHOLE run before app-b is ever contacted.
    let (addr_a, _rx_a) = spawn_bunny_responder(vec![
        "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ]);
    let (addr_b, rx_b) = spawn_bunny_probe_listener();

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_fleet([
            #{{app_id: "app-a", container: "web", image_tag: "v2", base_url: "http://{addr_a}"}},
            #{{app_id: "app-b", container: "web", image_tag: "v2", base_url: "http://{addr_b}"}},
        ], #{{api_key: "testkey"}});
        "#
    ));
    assert!(!ok, "must throw once the canary failure exceeds max_failures: 0:\n{out}");
    assert!(out.contains("app-a"), "error must name the failed target:\n{out}");
    assert!(
        rx_b.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "app-b must never be contacted once the canary already exceeded max_failures:\n{out}"
    );
}

#[test]
fn deploy_fleet_tolerates_a_batch_failure_within_the_threshold_and_reports_it() {
    // canary_size: 0 — both targets land in the same batch. One fails its PATCH (500); with
    // max_failures: 1 the run must NOT throw, and must report both the success and the failure.
    let (addr_ok, _rx_ok) = ok_target_responder("v1", "v2");
    let (addr_bad, _rx_bad) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ]);

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_fleet([
            #{{app_id: "app-ok", container: "web", image_tag: "v2", base_url: "http://{addr_ok}"}},
            #{{app_id: "app-bad", container: "web", image_tag: "v2", base_url: "http://{addr_bad}"}},
        ], #{{api_key: "testkey", canary_size: 0, batch_size: 5, max_failures: 1}});
        print("SUCCEEDED=" + r.succeeded.len() + " FAILED=" + r.failed.len() + " FIRST_FAIL=" + r.failed[0].app_id);
        "#
    ));
    assert!(ok, "one failure within max_failures: 1 must not throw:\n{out}");
    assert!(out.contains("SUCCEEDED=1 FAILED=1 FIRST_FAIL=app-bad"), "{out}");
}

#[test]
fn deploy_fleet_throws_once_failures_exceed_the_threshold_and_stops_dispatching_further_batches() {
    // Three single-target batches (batch_size: 1). The first batch fails; max_failures: 0 means
    // the run must throw right after batch 1, WITHOUT ever dispatching batch 2 (target "app-c").
    let (addr_a, _rx_a) = spawn_bunny_responder(vec![
        Box::leak(
            app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
        ),
        "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ]);
    let (addr_b, _rx_b) = ok_target_responder("v1", "v2");
    let (addr_c, rx_c) = spawn_bunny_probe_listener();

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_fleet([
            #{{app_id: "app-a", container: "web", image_tag: "v2", base_url: "http://{addr_a}"}},
            #{{app_id: "app-b", container: "web", image_tag: "v2", base_url: "http://{addr_b}"}},
            #{{app_id: "app-c", container: "web", image_tag: "v2", base_url: "http://{addr_c}"}},
        ], #{{api_key: "testkey", canary_size: 0, batch_size: 1, max_failures: 0}});
        "#
    ));
    assert!(!ok, "must throw once the first batch's failure exceeds max_failures: 0:\n{out}");
    assert!(out.contains("app-a"), "error must name the failed target:\n{out}");
    assert!(
        rx_c.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
        "app-c's batch must never be dispatched once the threshold was already exceeded:\n{out}"
    );
}

#[test]
fn deploy_fleet_under_dry_run_never_makes_a_real_patch_for_any_target() {
    let (addr_a, rx_a) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
    )]);
    let (addr_b, rx_b) = spawn_bunny_responder(vec![Box::leak(
        app_config_response(r#"{"id":"c-web","name":"web","imageTag":"v1"}"#).into_boxed_str(),
    )]);

    let (ok, out) = run_dry(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_fleet([
            #{{app_id: "app-a", container: "web", image_tag: "v2", base_url: "http://{addr_a}"}},
            #{{app_id: "app-b", container: "web", image_tag: "v2", base_url: "http://{addr_b}"}},
        ], #{{api_key: "testkey", canary_size: 1, batch_size: 5}});
        print("SUCCEEDED=" + r.succeeded.len() + " FAILED=" + r.failed.len());
        "#
    ));
    assert!(ok, "{out}");
    assert!(out.contains("SUCCEEDED=2 FAILED=0"), "{out}");

    // Each listener was given exactly ONE canned response (for the honest GET) — a real PATCH
    // would need a SECOND connection neither listener has a response queued for.
    let _get_a = rx_a.recv_timeout(std::time::Duration::from_secs(5)).expect("GET must be real");
    assert!(rx_a.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "no real PATCH for app-a under --dry-run");
    let _get_b = rx_b.recv_timeout(std::time::Duration::from_secs(5)).expect("GET must be real");
    assert!(rx_b.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "no real PATCH for app-b under --dry-run");
}

#[test]
fn deploy_fleet_counts_a_health_url_failure_as_a_target_failure_even_though_the_patch_succeeded() {
    // canary_size: 0, one target, whose health_url never answers — wait_healthy must exhaust and
    // throw, and deploy_batch must catch that and report the target as failed (not crash the run,
    // not report it as succeeded just because the PATCH itself returned 200).
    let (addr, _rx) = ok_target_responder("v1", "v2");
    let (health_addr, _health_rx) = spawn_bunny_probe_listener();

    let (ok, out) = run_live(&format!(
        r#"
        import "lib/bunny" as bunny;
        let r = bunny::deploy_fleet([
            #{{
                app_id: "app-a", container: "web", image_tag: "v2", base_url: "http://{addr}",
                health_url: "http://{health_addr}/healthz",
            }},
        ], #{{api_key: "testkey", canary_size: 0, batch_size: 5, max_failures: 1,
              health_attempts: 2, health_interval: 0}});
        print("SUCCEEDED=" + r.succeeded.len() + " FAILED=" + r.failed.len());
        "#
    ));
    assert!(ok, "one health failure within max_failures: 1 must not throw:\n{out}");
    assert!(
        out.contains("SUCCEEDED=0 FAILED=1"),
        "a health_url that never responds must count as a target failure despite the PATCH \
         itself succeeding:\n{out}"
    );
}

#[test]
fn deploy_fleet_rejects_an_empty_targets_array() {
    let (ok, out) = run_live(
        r#"
        import "lib/bunny" as bunny;
        bunny::deploy_fleet([], #{api_key: "testkey"});
        "#,
    );
    assert!(!ok, "must throw on an empty targets array");
    assert!(out.contains("targets must not be empty"), "{out}");
}
