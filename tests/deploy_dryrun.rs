//! Integration: a `--dry-run` fleet-atomic deploy() of the real `lib/deploy.rhai`.
//!
//! Two fake hosts go through the whole pipeline against the dry-run sim (nothing
//! real runs). We assert the PLAN shows the fleet-atomic shape:
//!   * per-host `docker run` of a unique new container, publishing its host port on
//!     loopback only (`-p 127.0.0.1:<port>:<container_port>`)
//!   * per-host proxy traffic switch, with the rollback compensations registered
//!     BEFORE the switch (atomic-unwind wiring)
//!   * a single post-commit cleanup pass (rename / remove old / prune)
//!   * 0 executed (pure plan)
//!
//! Plus a wiring assertion that deploy() registers TWO rollback compensations
//! per host before each proxy switch — the mechanism that lets a mid-fleet
//! failure unwind (the transaction unwind mechanism itself is covered by
//! tests/transaction.rs).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Symlink the repo's real `lib/` into `dir` so `import "lib/deploy"` resolves
/// (imports anchor at the executed file's directory).
fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
    #[cfg(not(unix))]
    {
        // Fallback: copy the lib dir on non-unix.
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

/// Build a temp project (`.energize` marker DIR + `Energize.rhai` + linked lib).
fn project(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // The `.energize` PROJECT MARKER MUST BE A DIRECTORY (a file breaks state writes).
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    dir
}

const DEPLOY_SCRIPT: &str = r#"
import "lib/deploy" as deploy;
deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v42", "app", #{
    container_port: 3000,
    skip_build: true,
    skip_push: true,
});
"#;

#[test]
fn fleet_atomic_deploy_dry_run_plans_per_host_swap_and_post_commit_cleanup() {
    let dir = project(DEPLOY_SCRIPT);

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

    let stdout = String::from_utf8_lossy(&out.stdout);

    // Pure plan: nothing executed, and no state.json was written.
    assert!(
        stdout.contains("PLAN (dry run"),
        "plan header missing:\n{stdout}"
    );
    assert!(
        stdout.contains("0 executed."),
        "should execute nothing:\n{stdout}"
    );
    assert!(
        !dir.path().join(".energize/state.json").exists(),
        "dry-run must not write state.json"
    );

    // Pull happens on ALL hosts (outside the transaction).
    assert!(
        stdout.contains("docker pull 'ghcr.io/org/app:v42'"),
        "missing pull:\n{stdout}"
    );

    // Per host: a NEW container is run with a unique name (svc-web-<ver>-...).
    for host in ["web1", "web2"] {
        let run_line = stdout
            .lines()
            .find(|l| {
                l.contains(host) && l.contains("docker run") && l.contains("--name 'app-web-v42-")
            })
            .unwrap_or_else(|| panic!("missing per-host new-container run for {host}:\n{stdout}"));
        // The new name must NOT be the bare canonical name (it must be unique).
        assert!(
            run_line.contains("--name 'app-web-v42-"),
            "new container should use a unique versioned name on {host}: {run_line}"
        );
    }

    // Per host: a proxy traffic switch to the new target.
    for host in ["web1", "web2"] {
        assert!(
            stdout
                .lines()
                .any(|l| l.contains(host) && l.contains("kamal-proxy deploy 'app'")),
            "missing proxy switch for {host}:\n{stdout}"
        );
    }

    // POST-COMMIT cleanup pass: rename new->canonical, retire & remove the old
    // container, prune. These appear AFTER both hosts have switched.
    for host in ["web1", "web2"] {
        assert!(
            stdout
                .lines()
                .any(|l| l.contains(host) && l.contains("docker rename 'app-web-v42-")),
            "missing post-commit rename(new->canonical) for {host}:\n{stdout}"
        );
        assert!(
            stdout
                .lines()
                .any(|l| l.contains(host) && l.contains("docker rm -f 'app-web-old'")),
            "missing post-commit removal of old container for {host}:\n{stdout}"
        );
        assert!(
            !stdout
                .lines()
                .any(|l| l.contains(host) && l.contains("image prune")),
            "unexpected host-wide prune for {host}:\n{stdout}"
        );
    }

    // Final deploy state is recorded — by KEY and value size; the plan never prints a state
    // value, since one can be (or contain) a secret in a form redaction cannot match.
    assert!(
        stdout.contains("app.version = <3 bytes>"),
        "missing version state:\n{stdout}"
    );
    assert!(
        stdout.contains("app.image = <19 bytes>"),
        "missing image state:\n{stdout}"
    );
    assert!(
        !stdout.contains("app.image = ghcr.io/org/app:v42"),
        "the plan must not print state VALUES:\n{stdout}"
    );
}

/// Run a dry-run of `script` and return its stdout.
fn dry_run_plan(script: &str) -> String {
    let dir = project(script);
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

/// The app container's auto-picked host port must be published on LOOPBACK ONLY.
///
/// A bare `-p '<port>:<cport>'` makes Docker bind 0.0.0.0, so every deployed app answers
/// plaintext HTTP at a predictable port to anyone who can reach the host — bypassing the proxy
/// that actually enforces TLS, `cfg.domain` host matching and `proxy_maintenance`'s 503 (and
/// bypassing a host firewall too: Docker's published-port DNAT runs before the INPUT chain).
/// Nothing in a deploy needs the port off-host: both proxy backends run `--network host`, and
/// the health gate curls the host's own `localhost:<port>` over SSH.
#[test]
fn new_container_host_port_is_published_on_loopback_only() {
    let stdout = dry_run_plan(DEPLOY_SCRIPT);

    for host in ["web1", "web2"] {
        let run_line = stdout
            .lines()
            .find(|l| l.contains(host) && l.contains("--name 'app-web-v42-"))
            .unwrap_or_else(|| panic!("missing per-host new-container run for {host}:\n{stdout}"));
        assert!(
            run_line.contains("-p '127.0.0.1:13000:3000'"),
            "the new container's host port must be published on loopback only on {host}: {run_line}"
        );
        assert!(
            !run_line.contains("-p '13000:3000'"),
            "the new container must NOT be published on all interfaces (0.0.0.0) on {host}: \
             {run_line}"
        );
    }

    // The proxy target is unchanged by the bind address — kamal-proxy runs with `--network host`,
    // so `localhost:<port>` still reaches the loopback-bound container.
    for host in ["web1", "web2"] {
        assert!(
            stdout
                .lines()
                .any(|l| l.contains(host) && l.contains("--target 'localhost:13000'")),
            "proxy target must stay localhost:<picked_port> for {host}:\n{stdout}"
        );
    }
}

/// `cfg.publish_all_interfaces: true` is the explicit opt-in back to the 0.0.0.0 bind, for a host
/// that really does need the container reachable from off-box.
#[test]
fn publish_all_interfaces_opts_in_to_the_all_interfaces_bind() {
    let stdout = dry_run_plan(
        r#"
import "lib/deploy" as deploy;
deploy::deploy(["web1"], "ghcr.io/org/app:v42", "app", #{
    container_port: 3000,
    skip_build: true,
    skip_push: true,
    publish_all_interfaces: true,
});
"#,
    );

    let run_line = stdout
        .lines()
        .find(|l| l.contains("web1") && l.contains("--name 'app-web-v42-"))
        .unwrap_or_else(|| panic!("missing new-container run line:\n{stdout}"));
    assert!(
        run_line.contains("-p '13000:3000'") && !run_line.contains("127.0.0.1"),
        "publish_all_interfaces: true must publish the bare host port: {run_line}"
    );
}

/// Accessories are NOT touched by the loopback default: a database on its own host is legitimately
/// reached across the fleet on its published port, so `accessory_run` publishes exactly the
/// `ports` map the caller wrote (see `lib/examples/Energize.rhai`, where the web hosts connect to
/// `DB_HOST:5432`).
#[test]
fn accessory_publishes_exactly_the_ports_the_caller_asked_for() {
    let stdout = dry_run_plan(
        r#"
import "lib/deploy" as deploy;
deploy::accessory_run("db1", "app-db", "postgres:16", #{ ports: #{ "5432": "5432" } });
"#,
    );

    let run_line = stdout
        .lines()
        .find(|l| l.contains("db1") && l.contains("--name 'app-db'"))
        .unwrap_or_else(|| panic!("missing accessory run line:\n{stdout}"));
    assert!(
        run_line.contains("-p '5432:5432'"),
        "an accessory's publish map must pass through unchanged: {run_line}"
    );
    assert!(
        !run_line.contains("127.0.0.1"),
        "accessory publishes must NOT be rewritten to loopback — a cross-host database \
         connection depends on them: {run_line}"
    );
}

#[test]
fn deploy_registers_two_rollbacks_before_each_proxy_switch() {
    // The atomic-unwind WIRING: per host, deploy() must register exactly two
    // rollback compensations (restore-proxy + rm-new) BEFORE switching traffic.
    // We assert ordering on the dry-run plan: every "kamal-proxy deploy app"
    // switch is preceded by two "register compensation" lines since the last
    // switch (or the start). The unwind MECHANISM is proven in tests/transaction.rs.
    let dir = project(DEPLOY_SCRIPT);

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

    let stdout = String::from_utf8_lossy(&out.stdout);

    // Only look at the plan body (after the header) so banner text doesn't count.
    let plan = stdout
        .split_once("PLAN (dry run")
        .map(|(_, p)| p)
        .unwrap_or(&stdout);

    let mut comps_since_switch = 0usize;
    let mut switches = 0usize;
    for line in plan.lines() {
        if line.contains("register compensation") {
            comps_since_switch += 1;
        }
        // A proxy switch for our service in the rolling loop (target localhost:...).
        if line.contains("kamal-proxy deploy 'app'") && line.contains("--target 'localhost:") {
            assert_eq!(
                comps_since_switch,
                2,
                "each proxy switch must be preceded by exactly 2 rollback registrations; \
                 got {comps_since_switch} before switch #{}:\n{stdout}",
                switches + 1
            );
            switches += 1;
            comps_since_switch = 0;
        }
    }
    assert_eq!(
        switches, 2,
        "expected one rolling proxy switch per host:\n{stdout}"
    );
}
