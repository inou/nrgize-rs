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
fn accessory_restart_keeps_the_sim_consistent_for_a_later_read_in_the_same_script() {
    // Fable final review: accessory_restart used to hand-build a raw ssh_exec, bypassing the sim
    // overlay entirely. `docker restart` always leaves the container running (even bringing a
    // stopped one back up) — but with no sim-routed mutation, a later read in the SAME dry-run
    // script (e.g. a subsequent accessory_stop) would see the PRE-restart reality instead of the
    // outcome a live run would actually produce. Stop it, restart it, then stop it again: the
    // second stop must see it as running (because the restart brought it back up), not skip as
    // an already-stopped no-op.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_run("host1", "myapp-db", "postgres:16");
        deploy::accessory_stop("host1", "myapp-db");
        deploy::accessory_restart("host1", "myapp-db");
        deploy::accessory_stop("host1", "myapp-db");
    "#,
    );
    assert_eq!(
        plan.matches("stop -t").count(),
        2,
        "the restart must be reflected in the sim, so the SECOND accessory_stop sees the \
         container as running again (from the restart) and actually issues its own stop — not \
         silently no-op because it still thinks the container is stopped from the FIRST stop:\n{plan}"
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
    let rm_pos = plan
        .find("rm -f 'myapp-db'")
        .unwrap_or_else(|| panic!("must remove the old container:\n{plan}"));
    assert!(
        !plan.contains("rm -f -v"),
        "must remove the old container WITHOUT -v, so its named volume survives \
         (Opus review: a looser \"rm -f\" check alone wouldn't catch a regression that \
         started passing -v/--volumes to the removal):\n{plan}"
    );
    // Fable final review: the upgrade's own pull command also contains "postgres:17" (that's what
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
    // Opus review: an earlier version stopped and removed the OLD container BEFORE ever
    // referencing the new image, so a bad tag / unpushed image / registry-auth failure would
    // throw only after the old, working accessory was already destroyed, with no rollback. The
    // fix pulls the new image FIRST (mirroring deploy()'s own pull-before-transaction ordering),
    // so that failure mode now throws with the old container still up. Assert the ordering
    // directly: the pull for the NEW image must appear in the plan before the stop/rm of the OLD
    // container.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
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
fn accessory_upgrade_actually_starts_the_new_image_when_the_old_accessory_was_running() {
    // Fable final review: SimState::set_stopped/remove used to be no-ops on a (host, name) that
    // had never been read/seeded in THIS script — which is exactly accessory_upgrade's own
    // shape (it stops+removes before ever reading). That left the entity unseeded, so
    // accessory_run's own running-check fell through to a real probe of a reachable host instead
    // of reflecting the simulated stop/remove — reporting the OLD, pre-upgrade "still running"
    // reality and short-circuiting as a no-op, silently DROPPING the docker run of the new image
    // from the plan. Seeding the accessory as running first (via accessory_run, the same
    // in-script seeding pattern every other test in this file relies on) reproduces that exact
    // shape: accessory_upgrade must still actually start the new image, not silently no-op.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_run("host1", "myapp-db", "postgres:16");
        deploy::accessory_upgrade("host1", "myapp-db", "postgres:17");
    "#,
    );
    let rm_pos = plan
        .find("rm -f 'myapp-db'")
        .unwrap_or_else(|| panic!("must remove the old container:\n{plan}"));
    let after_rm = &plan[rm_pos..];
    assert!(
        after_rm.contains("run -d") && after_rm.contains("postgres:17"),
        "the upgrade must still start the NEW image even when the accessory was already \
         running before the upgrade began — it must not silently no-op as \"already running\":\n{plan}"
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
    //
    // Opus review: the real `docker_stop` command shape is `docker stop -t '30' 'myapp-db' ...`
    // (a timeout token sits BETWEEN "stop" and the quoted name), so the bare substring
    // `"stop 'myapp-db'"` never appears in any plan this codebase generates regardless of
    // behavior — a prior version of this assertion (`!plan.contains("stop 'myapp-db'")`) was a
    // tautology that could never fail, even for a mutant that made accessory_stop call
    // docker_stop unconditionally. Assert against the exact command shape instead, matching the
    // fix already applied to accessory_stop_stops_a_running_accessory above.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::accessory_stop("host1", "myapp-db");
        print("stop returned without throwing");
    "#,
    );
    assert!(
        !plan.contains("stop -t"),
        "an accessory that was never started should have nothing to stop:\n{plan}"
    );
}
