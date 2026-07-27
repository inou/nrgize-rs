//! Integration: every LIVE `nrg exec`/`nrg run` appends an entry to `.energize/audit.log`,
//! `nrg audit` prints it, and `--dry-run` writes nothing (matching the "dry-run touches no
//! disk state" contract the rest of the suite already holds `nrg exec --dry-run` to).

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::fs;

fn project(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    dir
}

const SCRIPT: &str = r#"
fn hello() { print("hi"); }
fn boom() { throw "kaboom"; }
"#;

#[test]
fn successful_run_is_recorded_and_shown_by_audit() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "hello"])
        .assert()
        .success();

    assert!(dir.path().join(".energize/audit.log").exists());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("run hello"))
        .stdout(predicates::str::contains("success"));
}

#[test]
fn failed_run_is_recorded_with_its_error() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("run boom"))
        .stdout(predicates::str::contains("failed"))
        .stdout(predicates::str::contains("kaboom"));
}

#[test]
fn dry_run_writes_no_audit_entry() {
    let dir = project(SCRIPT);

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "hello", "--dry-run"])
        .assert()
        .success();

    assert!(
        !dir.path().join(".energize/audit.log").exists(),
        "dry-run must not write the audit log, same as it writes no state"
    );
}

#[test]
fn audit_on_fresh_project_reports_no_history() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("No audit history yet"));
}

#[test]
fn audit_filter_narrows_to_matching_target() {
    let dir = project(SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).args(["run", "hello"]).assert().success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["audit", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("run hello"))
        .stdout(predicates::str::contains("boom").not());
}

#[test]
fn audit_entries_are_most_recent_first() {
    let dir = project(SCRIPT);
    Command::cargo_bin("nrg").unwrap().current_dir(dir.path()).args(["run", "hello"]).assert().success();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "boom"])
        .assert()
        .failure();

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hello_pos = stdout.find("run hello").unwrap();
    let boom_pos = stdout.find("run boom").unwrap();
    assert!(boom_pos < hello_pos, "most recent (boom) must print first:\n{stdout}");
}

/// The audit log's headline safety property: a secret revealed into a thrown error must never
/// reach `.energize/audit.log` in plaintext, on disk or in `nrg audit`'s output. Mirrors the
/// same `ctx.secrets`-redaction boundary the dry-run plan already goes through.
#[test]
fn secret_revealed_into_a_thrown_error_is_redacted_from_the_audit_log() {
    let dir = project(
        r#"
fn boom() {
    let s = secret("DBPASS");
    throw "boom: " + reveal(s);
}
"#,
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_DBPASS", "hunter2supersecretvalue")
        .args(["run", "boom"])
        .assert()
        .failure();

    let raw = fs::read_to_string(dir.path().join(".energize/audit.log")).unwrap();
    assert!(
        !raw.contains("hunter2supersecretvalue"),
        "secret plaintext must never land in audit.log on disk:\n{raw}"
    );
    assert!(raw.contains("***"), "a redaction marker should stand in for the secret:\n{raw}");

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("hunter2supersecretvalue").not());
}

/// Same property from the OTHER direction: an operator-typed CLI arg that happens to equal a
/// value the script separately resolved via `secret()` must also be redacted from `entry.args`,
/// not just from the thrown-error path above.
#[test]
fn cli_arg_matching_a_registered_secret_is_redacted_from_audit_args() {
    let dir = project(
        r#"
fn rollback(pw) {
    let s = secret("DBPASS"); // registers the plaintext for redaction, regardless of `pw`
    print("rolling back");
}
"#,
    );

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_DBPASS", "hunter2supersecretvalue")
        .args(["run", "rollback", "hunter2supersecretvalue"])
        .assert()
        .success();

    let raw = fs::read_to_string(dir.path().join(".energize/audit.log")).unwrap();
    assert!(
        !raw.contains("hunter2supersecretvalue"),
        "a CLI arg matching a registered secret must be redacted from audit.log:\n{raw}"
    );
}

/// Write `line` (one raw JSON record) as the whole audit log of a fresh project.
fn project_with_audit_log(line: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/audit.log"), format!("{line}\n")).unwrap();
    dir
}

/// A failing remote command's stderr is folded into `outcome` verbatim, so a compromised deploy
/// host chooses those bytes. `append` stores them JSON-escaped and `read_all` decodes them back
/// to raw bytes — if `nrg audit` then printed them as-is, a CR + erase-line + fabricated entry
/// would erase the real (failed) line and render a clean "success" in its place. The rendering
/// must be inert instead: no terminal ever sees the CR or the ESC.
#[test]
fn control_sequences_in_a_recorded_field_render_inertly() {
    let dir = project_with_audit_log(concat!(
        r#"{"ts":"2026-07-25T09:59:59Z","user":"mallory","host":"web1","cwd":"/srv","command":"run","#,
        r#""file":"Energize.rhai","target":"deploy","args":["v9"],"#,
        r#""outcome":"failed: boom\r\u001b[2K2026-07-25T10:00:00Z  alice@web1  run deploy v9  success"}"#,
    ));

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!stdout.contains('\r'), "a recorded CR must not reach the terminal:\n{stdout:?}");
    assert!(
        !stdout.contains("\u{1b}[2K"),
        "a recorded erase-line sequence must not reach the terminal:\n{stdout:?}"
    );
    assert!(
        stdout.contains("failed: boom\\u{d}\\u{1b}[2K"),
        "the bytes must still be SHOWN, escaped, so tampering is visible:\n{stdout:?}"
    );
    assert_eq!(
        stdout.lines().count(),
        1,
        "one entry must render as exactly one line — nothing erased, nothing forged:\n{stdout:?}"
    );
}

/// The flip side: neutralizing controls must not touch legitimate text. A UTF-8 operator name,
/// an IDN hostname, a CJK path, an emoji and shell quoting in args all render as recorded.
#[test]
fn legitimate_multibyte_fields_render_unchanged() {
    let dir = project_with_audit_log(concat!(
        r#"{"ts":"2026-07-25T09:59:59Z","user":"Zoë","host":"münchen.example","cwd":"/srv","command":"run","#,
        r#""file":"Energize.rhai","target":"déploy","args":["東京/路径","🚀","--flag=\"a b\""],"#,
        r#""outcome":"failed: échec sur münchen.example ❌"}"#,
    ));

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("audit")
        .assert()
        .success()
        .stdout(predicates::str::contains("2026-07-25T09:59:59Z  Zoë@münchen.example"))
        .stdout(predicates::str::contains(r#"run déploy 東京/路径 🚀 --flag="a b""#))
        .stdout(predicates::str::contains("failed: échec sur münchen.example ❌"));
}
