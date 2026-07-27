//! Integration: running `nrg` from a SUBDIRECTORY of a project still resolves secrets and the
//! default orchestration file against the PROJECT ROOT, not CWD (issue #19) — and that upward
//! walk refuses a root planted by somebody else (see the ownership tests at the bottom).

use assert_cmd::Command;
#[cfg(unix)]
use predicates::prelude::PredicateBooleanExt;
use std::fs;

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn secrets_resolve_against_project_root_from_a_subdir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".energize")).unwrap();
    // Project-level secret file at the ROOT (not the subdir).
    fs::write(root.join(".energize/secrets"), "API_TOKEN=rootsecretvalue\n").unwrap();
    // The orchestration file lives at the root and reveals the secret onto a captured file.
    let captured = root.join("captured.txt");
    fs::write(
        root.join("Energize.rhai"),
        format!(
            r#"let t = secret("API_TOKEN");
               local_exec("printf %s " + sh_quote(t) + " > {out}");"#,
            out = captured.display()
        ),
    )
    .unwrap();

    // Run from a nested subdirectory with NO secrets/Energize of its own.
    let sub = root.join("services/web");
    fs::create_dir_all(&sub).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub) // CWD is the subdir; the root is discovered upward
        .arg("exec")
        .assert()
        .success();

    // The secret was found (resolved against the root) and delivered to the command.
    let got = fs::read_to_string(&captured).unwrap();
    assert_eq!(got, "rootsecretvalue", "secret must resolve against the project root, not CWD");
}

#[test]
fn default_file_is_found_from_a_subdir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".energize")).unwrap();
    fs::write(
        root.join("Energize.rhai"),
        r#"print("ran-from-root-file");"#,
    )
    .unwrap();
    let sub = root.join("a/b");
    fs::create_dir_all(&sub).unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub)
        .arg("exec") // no file arg -> must discover Energize.rhai at the root
        .assert()
        .success()
        .stderr(predicates::str::contains("ran-from-root-file"));
}

#[cfg(unix)]
#[test]
fn a_project_marker_planted_in_a_world_writable_ancestor_is_never_adopted_as_the_root() {
    // The `$HOME` bound on the upward walk only fires when `$HOME` really is an ancestor, so from
    // a CWD outside it (a CI workspace, `/srv`, `/opt`, a container `WORKDIR` — or this temp dir)
    // the walk used to pop all the way to `/` and adopt the first marker-bearing directory it
    // met. Any other local user could therefore plant `.energize` + `Energize.rhai` in a
    // world-writable ancestor and have it executed, as you, on your next `nrg` run.
    let dir = tempfile::tempdir().unwrap();
    let planted = dir.path().join("shared");
    let sub = planted.join("work");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(planted.join(".energize")).unwrap(); // the planted marker
    let beacon = dir.path().join("PWNED");
    fs::write(
        planted.join("Energize.rhai"),
        format!(r#"local_exec("touch {}");"#, beacon.display()),
    )
    .unwrap();
    chmod(&planted, 0o777);

    // Nothing of our own to run: the planted script must not be picked up, and above all must
    // not execute.
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub)
        .arg("exec")
        .assert()
        .failure();
    assert!(!beacon.exists(), "the planted script must never run");

    // With a script of our own to run, the refusal is what stops the run — loudly, naming the
    // directory and the reason, before a single line of any script is evaluated (the untrusted
    // root is also where secrets, state and the audit log would have come from).
    fs::write(sub.join("Energize.rhai"), r#"print("our-own-script-ran");"#).unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub)
        .arg("exec")
        .assert()
        .failure()
        .stderr(predicates::str::contains("refusing to use"))
        .stderr(predicates::str::contains("writable by other users"))
        .stderr(predicates::str::contains(planted.display().to_string()))
        .stderr(predicates::str::contains("our-own-script-ran").not());
    assert!(!beacon.exists(), "the planted script must never run");
}

#[cfg(unix)]
#[test]
fn an_ordinary_group_writable_root_under_a_sticky_shared_parent_still_works_from_a_subdir() {
    // The other side of the same check: only the directory the marker is ACCEPTED in is vetted,
    // and group-writable is fine there. A `0775` checkout (the umask-002 default on RHEL/Fedora
    // and setgid team checkouts) sitting under a `1777` shared parent (`/tmp`, a build area) is
    // an ordinary working setup that must keep working, secrets included.
    let dir = tempfile::tempdir().unwrap();
    let shared_parent = dir.path().join("shared");
    let root = shared_parent.join("proj");
    let sub = root.join("services/web");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(root.join(".energize")).unwrap();
    fs::write(root.join(".energize/secrets"), "API_TOKEN=rootsecretvalue\n").unwrap();
    chmod(&root.join(".energize/secrets"), 0o664);
    let captured = root.join("captured.txt");
    fs::write(
        root.join("Energize.rhai"),
        format!(
            r#"let t = secret("API_TOKEN");
               local_exec("printf %s " + sh_quote(t) + " > {out}");"#,
            out = captured.display()
        ),
    )
    .unwrap();
    chmod(&shared_parent, 0o1777); // sticky + world-writable, like /tmp
    chmod(&root, 0o775); // group-writable, ours

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(&sub)
        .arg("exec")
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&captured).unwrap(), "rootsecretvalue");
}
