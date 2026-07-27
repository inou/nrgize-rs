//! Integration: `nrg exec --dry-run` records a plan and makes no changes.

use assert_cmd::Command;
use std::fs;

#[test]
fn dry_run_records_plan_and_makes_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let sentinel = dir.path().join("should-not-exist");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
        local_exec("echo build > {sentinel}");
        state_set("version", "v9");
        // sim_http_healthy is the NEW-container probe: stubbed healthy (200) in dry-run.
        let r = sim_http_healthy("http://127.0.0.1:1/health");
        if !r.ok {{ throw "health failed" }}
        // http_get is HONEST even in dry-run (issue #16): an unreachable URL is status 0, NOT a
        // synthetic 200, so a precondition gate on existing reality can't be fooled by a plan.
        let real = http_get("http://127.0.0.1:1/never");
        if real.ok {{ throw "http_get must NOT report a synthetic ok in dry-run" }}
        "#,
            sentinel = sentinel.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stdout(predicates::str::contains("PLAN (dry run"))
        // The plan names the state KEY and the value's size — never the value itself.
        .stdout(predicates::str::contains("version = <2 bytes>"))
        .stdout(predicates::str::contains("probed live"))
        .stdout(predicates::str::contains("0 executed."));

    // No state.json was written (dry-run uses the overlay):
    assert!(!dir.path().join(".energize/state.json").exists());
    // The local_exec side effect did NOT happen:
    assert!(!sentinel.exists());
}

#[test]
fn dry_run_plan_never_prints_a_url_encoded_secret() {
    // The plan prints to STDOUT (bypassing the on_print redaction hook), so `nrg exec --dry-run`
    // — the encouraged pre-deploy and CI check — must not leak a password into build logs. A
    // percent-encoded secret (what the shipped example builds for a DATABASE_URL) no longer
    // contains the registered plaintext, so substring redaction alone cannot catch it. Two
    // things keep it out: `state_set` records the key + byte count instead of the value, and
    // `url_encode` registers the encoded form of the secret it was handed.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        let pw = url_encode(reveal(secret("DB_PASSWORD")));
        let url = "postgres://app:" + pw + "@db:5432/app_production";
        state_set("app.config", url);
        print("url:" + url);
        "#,
    )
    .unwrap();

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_DB_PASSWORD", "p@ssw0rd#1")
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The plan still records THAT state changed, and under which key.
    assert!(
        stdout.contains("app.config = <"),
        "plan must still record the state key:\n{stdout}"
    );
    for stream in [&stdout, &stderr] {
        assert!(
            !stream.contains("p%40ssw0rd%231"),
            "percent-encoded password leaked:\n{stream}"
        );
        assert!(!stream.contains("p@ssw0rd#1"), "password leaked:\n{stream}");
    }
    // ...and the encoded form is redacted everywhere else too (here: `print`, via on_print).
    assert!(
        stderr.contains("url:postgres://app:***@db:5432/app_production"),
        "the encoded secret must be redacted, not just elided:\n{stderr}"
    );
}
