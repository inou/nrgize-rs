//! Integration: robustness-review R1 and R2 — two stdlib helpers that spliced a
//! caller-controlled value into a remote/local shell command WITHOUT `sh_quote()`, so a
//! value containing shell metacharacters (`;`, `"`, `$(...)`) could run arbitrary extra
//! commands (registry review, issue #10's quoting contract).
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

fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
}

/// Escape `s` so it can be embedded as the content of a Rhai double-quoted string literal.
fn rhai_string_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn run_live(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join(".energize")).unwrap();
    link_lib(dir);
    fs::write(dir.join("Energize.rhai"), script).unwrap();
    // Exit code is irrelevant here (aws/docker are almost certainly not installed in the test
    // environment, so the real login/exec calls fail with "command not found" — that's fine,
    // and expected); only whether the injected statement ran is under test.
    let _ = Command::cargo_bin("nrg").unwrap().current_dir(dir).arg("exec").output();
}

/// R1 — `ecr_login`'s account-auto-detect branch used to splice `cfg.region` RAW inside an
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
        import "lib/registry" as registry;
        registry::ecr_login("local", #{{ region: "{region}" }});
        "#,
        region = rhai_string_literal(&region),
    );
    run_live(dir.path(), &script);
    assert!(
        !marker.exists(),
        "cfg.region injected a shell command via ecr_login's account-auto-detect branch (R1)"
    );
}

/// R2 — `runtime_exec_cmd(container_name, command)` spliced `container_name` RAW into the
/// command line, unlike its `docker_exec` twin (which quotes the name).
#[test]
fn runtime_exec_cmd_container_name_cannot_inject_shell_commands() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("injected.marker");
    let name = format!("x; touch {}; echo y", marker.display());
    let script = format!(
        r#"
        import "lib/runtime" as rt;
        let cmd = rt::runtime_exec_cmd("{name}", "echo hi");
        local_exec(cmd);
        "#,
        name = rhai_string_literal(&name),
    );
    run_live(dir.path(), &script);
    assert!(
        !marker.exists(),
        "container_name injected a shell command via runtime_exec_cmd (R2)"
    );
}
