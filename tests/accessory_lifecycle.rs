//! Integration: `accessory_stop` / `accessory_restart` / `accessory_upgrade` (roadmap 2.7).
//! `accessory_run` (already shipped) starts an accessory if absent, but its own idempotency check
//! is BY NAME only — a running `myapp-db` blocks it from ever noticing an image bump. These three
//! functions give a service's databases/caches a supported stop/restart/upgrade path.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

/// Symlink the repo's real `lib/` into `dir` so `import "lib/deploy"` resolves.
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

fn plan_for(script: &str) -> String {
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
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn accessory_stop_stops_a_running_accessory() {
    // accessory_run first, in the SAME script, seeds the dry-run sim world with "myapp-db"
    // running — without that, sim_container_running's dry-run seed probe would attempt (and,
    // sandboxed here, fail) a real inspect and assume the container absent, making accessory_stop
    // a no-op instead of exercising the "stop something that's actually running" branch.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_run("host1", "myapp-db", "postgres:16");
        deploy::accessory_stop("host1", "myapp-db");
    "#,
    );
    assert!(
        plan.contains("stop -t") && plan.contains("'myapp-db'"),
        "missing the docker stop -t command for the accessory (checking for the exact command \
         shape, not just the word \"stop\" — which also appears in the idempotent no-op's own \
         \"already stopped\" message):\n{plan}"
    );
}

#[test]
fn accessory_restart_runs_docker_restart_in_place() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_restart("host1", "myapp-db");
    "#,
    );
    assert!(
        plan.contains("restart 'myapp-db'") || plan.contains("restart myapp-db"),
        "missing docker restart for the accessory:\n{plan}"
    );
}

#[test]
fn accessory_upgrade_stops_removes_and_restarts_on_the_new_image() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_upgrade("host1", "myapp-db", "postgres:17", #{
            ports: #{ "5432": "5432" },
            volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
        });
    "#,
    );
    assert!(plan.contains("stop -t"), "must stop the old container:\n{plan}");
    assert!(plan.contains("rm -f 'myapp-db'") || plan.contains("rm -f myapp-db"), "must remove the old container:\n{plan}");
    assert!(
        plan.contains("postgres:17"),
        "must start the NEW image, not the old one:\n{plan}"
    );
    assert!(
        plan.contains("myapp-db-data"),
        "must reuse the same named volume so data survives the upgrade:\n{plan}"
    );
}

#[test]
fn accessory_upgrade_defaults_cfg_to_empty_via_the_3_arg_overload() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_upgrade("host1", "myapp-cache", "redis:8");
    "#,
    );
    assert!(plan.contains("redis:8"), "3-arg overload must still start the new image:\n{plan}");
}

#[test]
fn accessory_stop_is_idempotent_on_an_already_stopped_accessory() {
    // Nothing is "running" in a fresh dry-run sim, so this must succeed as a no-op rather than
    // erroring — matching docker_stop's own `|| true` semantics one level up.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_stop("host1", "myapp-db");
        print("stop returned without throwing");
    "#,
    );
    assert!(
        plan.contains("(no side effects)") || !plan.contains("stop 'myapp-db'"),
        "an accessory that was never started should have nothing to stop:\n{plan}"
    );
}
