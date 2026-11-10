//! Integration: `accessory_stop` / `accessory_restart` / `accessory_upgrade`.
//! `accessory_run` (already shipped) starts an accessory if absent, but its own idempotency check
//! is BY NAME only — a running `myapp-db` blocks it from ever noticing an image bump. These three
//! functions give a service's databases/caches a supported stop/restart/upgrade path.

use assert_cmd::Command;
use std::fs;

fn plan_for(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
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
fn accessory_restart_runs_docker_restart_in_place() {
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
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
        import "std/deploy" as deploy;
        deploy::accessory_upgrade("host1", "myapp-db", "postgres:17", #{
            ports: #{ "5432": "5432" },
            volumes: #{ "myapp-db-data": "/var/lib/postgresql/data" },
        });
    "#,
    );
    assert!(
        plan.contains("stop -t"),
        "must stop the old container:\n{plan}"
    );
    let rm_pos = plan
        .find("rm -f 'myapp-db'")
        .unwrap_or_else(|| panic!("must remove the old container:\n{plan}"));
    assert!(
        !plan.contains("rm -f -v"),
        "must remove the old container WITHOUT -v, so its named volume survives \
         (a looser \"rm -f\" check alone wouldn't catch a regression that \
         started passing -v/--volumes to the removal):\n{plan}"
    );
    // The upgrade's own pull command also contains "postgres:17" (that's what
    // the ordering test above checks), so a plain `plan.contains("postgres:17")` here would pass
    // even if the actual `docker run` never got the new image. Require the run itself — assert
    // "run -d" (docker_run's command prefix) appears AFTER the removal, and specifically that a
    // "postgres:17" occurrence exists in that post-removal tail.
    let after_rm = &plan[rm_pos..];
    assert!(
        after_rm.contains("run -d") && after_rm.contains("postgres:17"),
        "must actually START the new image via docker run AFTER removing the old container, not \
         just reference the tag in the earlier pull:\n{plan}"
    );
    assert!(
        after_rm.contains("myapp-db-data"),
        "must reuse the same named volume so data survives the upgrade:\n{plan}"
    );
}

#[test]
fn accessory_upgrade_pulls_the_new_image_before_touching_the_old_container() {
    // An earlier version stopped and removed the OLD container BEFORE ever
    // referencing the new image, so a bad tag / unpushed image / registry-auth failure would
    // throw only after the old, working accessory was already destroyed, with no rollback. The
    // fix pulls the new image FIRST (mirroring deploy()'s own pull-before-transaction ordering),
    // so that failure mode now throws with the old container still up. Assert the ordering
    // directly: the pull for the NEW image must appear in the plan before the stop/rm of the OLD
    // container.
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::accessory_upgrade("host1", "myapp-db", "postgres:17");
    "#,
    );
    let pull_pos = plan
        .find("pull 'postgres:17'")
        .unwrap_or_else(|| panic!("must pull the new image before upgrading:\n{plan}"));
    let stop_pos = plan
        .find("stop -t")
        .unwrap_or_else(|| panic!("must still stop the old container:\n{plan}"));
    assert!(
        pull_pos < stop_pos,
        "the new image must be pulled BEFORE the old container is stopped/removed, so a bad \
         tag fails safe with the old accessory still up:\n{plan}"
    );
}

#[test]
fn accessory_upgrade_defaults_cfg_to_empty_via_the_3_arg_overload() {
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::accessory_upgrade("host1", "myapp-cache", "redis:8");
    "#,
    );
    assert!(
        plan.contains("redis:8"),
        "3-arg overload must still start the new image:\n{plan}"
    );
}

#[test]
fn accessory_stop_is_idempotent_on_an_already_stopped_accessory() {
    // Nothing is "running" in a dry run with no container on the (fake) host, so this must
    // succeed as a no-op rather than erroring — matching docker_stop's own `|| true` semantics
    // one level up.
    //
    // The real `docker_stop` command shape is `docker stop -t '30' 'myapp-db' ...`
    // (a timeout token sits BETWEEN "stop" and the quoted name), so the bare substring
    // `"stop 'myapp-db'"` never appears in any plan this codebase generates regardless of
    // behavior — a prior version of this assertion (`!plan.contains("stop 'myapp-db'")`) was a
    // tautology that could never fail, even for a mutant that made accessory_stop call
    // docker_stop unconditionally. Assert against the exact command shape instead, matching the
    // fix already applied to accessory_stop_stops_a_running_accessory above.
    let plan = plan_for(
        r#"
        import "std/deploy" as deploy;
        deploy::accessory_stop("host1", "myapp-db");
        print("stop returned without throwing");
    "#,
    );
    assert!(
        !plan.contains("stop -t"),
        "an accessory that was never started should have nothing to stop:\n{plan}"
    );
}
