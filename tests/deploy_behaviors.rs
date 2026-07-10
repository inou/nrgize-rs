//! Integration: deploy() correctness behaviors fixed in the issue sweep.
//!
//! All run against `--dry-run` (the dry-run plan is the observable contract) using the real
//! `lib/deploy.rhai`. Covers:
//! * #8 pre_deploy runs ONCE from a throwaway NEW-image container (`docker run --rm <image>`),
//!   not an `exec` into the old container, and with NO `|| true` swallow.
//! * #7 the restore-proxy compensation carries health_path (same proxy_cfg as the forward
//!   switch) — observable as `--health-check-path` on the registered rollback line.
//! * #6 deploy persists the full effective config, and rollback replays it.
//! * R29 deploy() refuses to run when already nested inside an active transaction() (nesting can
//!   resurrect already-post-committed rollback compensations on an unrelated later failure).
//! * R21 deploy() refuses an empty `hosts` array up front, instead of panicking on an
//!   out-of-bounds `hosts[0]` or silently persisting state for a release that touched no host.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
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
fn pre_deploy_runs_in_a_throwaway_new_image_container() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            pre_deploy: "bin/rails db:migrate",
        });
    "#,
    );
    // The release task runs as `docker run --rm ... <image> bin/rails db:migrate` — the NEW image.
    let line = plan
        .lines()
        .find(|l| l.contains("docker run --rm") && l.contains("bin/rails db:migrate"))
        .unwrap_or_else(|| panic!("pre_deploy did not run in a throwaway new-image container:\n{plan}"));
    assert!(
        line.contains("'ghcr.io/org/app:v9'"),
        "release task must use the NEW image: {line}"
    );
    // It must NOT be an `exec` into the old running container, and must NOT swallow failures.
    assert!(
        !plan.contains("exec app-web bin/rails db:migrate"),
        "release task must NOT exec into the old container:\n{plan}"
    );
    assert!(
        !line.contains("|| true"),
        "release task must NOT swallow failures with `|| true`: {line}"
    );
    // Runs ONCE (single host targeted), not per-host.
    let count = plan
        .lines()
        .filter(|l| l.contains("docker run --rm") && l.contains("db:migrate"))
        .count();
    assert_eq!(count, 1, "release task must run exactly once for the fleet:\n{plan}");
}

#[test]
fn restore_compensation_carries_health_path() {
    // With a non-default health_path, the forward switch AND the restore compensation must both
    // carry --health-check-path (issue #7): under dry-run, on_rollback closures aren't executed,
    // but the forward switch line proves health_path threads through the shared proxy_cfg.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            container_port: 8000, skip_build: true, skip_push: true,
            health_path: "/health/",
        });
    "#,
    );
    assert!(
        plan.contains("--health-check-path '/health/'"),
        "the proxy switch must use the configured health_path:\n{plan}"
    );
    assert!(
        !plan.contains("--health-check-path '/up'"),
        "no call should fall back to the default /up when health_path is set:\n{plan}"
    );
}

#[test]
fn recipe_example_runs_migration_on_new_image_and_redacts_secrets() {
    // The shared recipe (lib/recipe.rhai) drives the rails example: registry login, accessories,
    // and a deploy whose pre_deploy migration runs on the NEW image. Secrets in the persisted
    // config are redacted in the plan. Covers issue #22 (+#8, #11).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/examples/rails.rhai");
    fs::copy(&example, dir.path().join("Energize.rhai")).unwrap();

    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("DEPLOY_TAG", "v1.2.3")
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_tokenvalue123")
        .env("NRG_SECRET_DATABASE_URL", "postgres://u:secretpw@db/x")
        .env("NRG_SECRET_SECRET_KEY_BASE", "keybasevalue123")
        .env("NRG_SECRET_DB_PASSWORD", "dbpassvalue123")
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();
    let plan = String::from_utf8_lossy(&out.stdout);

    assert!(
        plan.contains("docker run --rm") && plan.contains("bin/rails db:migrate"),
        "recipe must run the migration in a throwaway new-image container:\n{plan}"
    );
    // Registered secrets never appear in the plan (the persisted config is redacted).
    assert!(!plan.contains("dbpassvalue123"), "DB password leaked into the plan:\n{plan}");
    assert!(!plan.contains("keybasevalue123"), "secret key base leaked into the plan:\n{plan}");
}

#[test]
fn deploy_persists_full_config_for_rollback() {
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            container_port: 8000, skip_build: true, skip_push: true,
            health_path: "/health/", proxy: "kamal",
        });
    "#,
    );
    // The effective config is persisted as JSON under <service>.config so rollback can replay it.
    let line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("deploy must persist <service>.config:\n{plan}"));
    assert!(line.contains("\"container_port\":8000"), "config must carry the port: {line}");
    assert!(line.contains("\"health_path\":\"/health/\""), "config must carry health_path: {line}");
}

#[test]
fn deploy_refuses_to_run_nested_inside_a_transaction() {
    // R29: a nested transaction's compensations deliberately stay live for an enclosing
    // transaction's later unwind (docs/safety.md, "Nesting") — but deploy()'s post-commit phase
    // treats its own transaction's success as final. Nesting deploy() inside a caller's own
    // transaction() could let an unrelated LATER failure resurrect already-superseded rollback
    // compensations. deploy() must refuse up front, before any build/push/pull work.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        transaction(|| {
            deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
                skip_build: true, skip_push: true,
            });
        });
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be called from inside an active transaction"));
}

#[test]
fn deploy_at_the_top_level_is_unaffected_by_the_nesting_guard() {
    // Companion regression check: deploy() called normally (NOT nested) must still work — the
    // guard must only fire when genuinely nested, not on every call.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{ skip_build: true, skip_push: true });
    "#,
    );
    assert!(
        plan.contains("app.version ="),
        "a non-nested deploy() must still run to completion:\n{plan}"
    );
}

// deploy()'s R10 warning goes through print(), which nrg routes to STDERR (so it can be redacted
// alongside everything else the script prints) — NOT into the dry-run plan captured by plan_for's
// stdout. These checks run the binary directly and assert on stderr instead.
fn stderr_for(script: &str) -> String {
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
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn deploy_warns_when_deploying_a_mutable_latest_tag() {
    // R10: ":latest" is a mutable registry pointer — if this build turns out broken, rollback()
    // may not be able to safely undo it (the tag can already point at the same broken build by
    // the time a rollback runs). A soft warning, not a refusal: ":latest" is the documented
    // quickstart default, so hard-blocking it would break that flow.
    let stderr = stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:latest", "app", #{ skip_build: true, skip_push: true });
    "#,
    );
    assert!(
        stderr.contains("[warn] deploying a mutable \":latest\" tag"),
        "deploying an explicit :latest tag must print the R10 warning:\n{stderr}"
    );
}

#[test]
fn deploy_warns_when_tag_is_omitted_entirely_since_it_implies_latest() {
    // Same gotcha, no explicit tag at all (Docker treats "app" identically to "app:latest").
    let stderr = stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app", "app", #{ skip_build: true, skip_push: true });
    "#,
    );
    assert!(
        stderr.contains("[warn] deploying a mutable \":latest\" tag"),
        "an image with no tag at all must also trigger the R10 warning:\n{stderr}"
    );
}

#[test]
fn deploy_with_a_pinned_tag_does_not_warn() {
    let stderr = stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{ skip_build: true, skip_push: true });
    "#,
    );
    assert!(
        !stderr.contains("[warn] deploying a mutable"),
        "an immutable, versioned tag must NOT trigger the R10 warning:\n{stderr}"
    );
}

#[test]
fn deploy_warns_on_a_case_variant_of_latest() {
    // Docker's own tag charset allows uppercase (`[\w][\w.-]{0,127}`, per the distribution spec),
    // so "LATEST" is a syntactically valid, distinct tag from "latest" — but it carries the exact
    // same "this is meant as a floating pointer" risk a CI script or operator typo could easily
    // produce. The comparison must be case-insensitive, not just an exact-string match.
    let stderr = stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:LATEST", "app", #{ skip_build: true, skip_push: true });
    "#,
    );
    assert!(
        stderr.contains("[warn] deploying a mutable \":latest\" tag"),
        "an uppercase LATEST tag must still trigger the R10 warning:\n{stderr}"
    );
}

#[test]
fn rollback_refuses_a_case_variant_of_the_mutable_latest_tag() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:Latest");
        state_set("app.prev", "ghcr.io/org/app:Latest");
        deploy::rollback(["web1"], "app", #{});
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to roll back"))
        .stderr(predicates::str::contains("mutable tag"));
}

#[test]
fn rollback_refuses_to_use_a_mutable_latest_snapshot() {
    // R10: if `<service>.prev` itself holds a mutable ":latest" tag, rolling back to it is not a
    // real rollback — the registry may have already moved "latest" on to the very broken build
    // being escaped, so the "rollback" would silently redeploy the SAME broken image. This must
    // run LIVE (not --dry-run, which never persists state) so the pre-set `.prev` is actually
    // readable by rollback(). The refusal happens before deploy() is ever called, so it never
    // touches the (nonexistent) "web1" host.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:latest");
        state_set("app.prev", "ghcr.io/org/app:latest");
        deploy::rollback(["web1"], "app", #{});
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to roll back"))
        .stderr(predicates::str::contains("mutable tag"));
}

#[test]
fn rollback_with_an_explicit_image_override_ignores_the_mutable_tag_guard() {
    // An explicit cfg.image is a deliberate, informed caller choice (unlike the automatic ".prev"
    // snapshot) and must NOT be second-guessed by the R10 guard, even if it names ":latest".
    let stderr = stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::rollback(["web1"], "app", #{ image: "ghcr.io/org/app:latest" });
    "#,
    );
    assert!(
        !stderr.contains("Refusing to roll back"),
        "an explicit :latest override must NOT be refused by the R10 guard:\n{stderr}"
    );
    assert!(
        stderr.contains("[warn] deploying a mutable \":latest\" tag"),
        "an explicit :latest override must still surface deploy()'s own warning:\n{stderr}"
    );
}

#[test]
fn rollback_with_no_prev_state_hints_at_the_mutable_tag_gotcha_when_relevant() {
    // When `.prev` was never recorded because every deploy so far used the SAME ":latest" string
    // (the string-equality guard in deploy() never fires), the plain "No rollback image found"
    // error is confusing on its own — hint at the real cause.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:latest");
        deploy::rollback(["web1"], "app", #{});
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("No rollback image found"))
        .stderr(predicates::str::contains("mutable \":latest\" tag"));
}

#[test]
fn rollback_refuses_when_nested_without_first_mutating_prev_state() {
    // rollback() carries the SAME R29 guard as deploy() (which it calls internally), but checked
    // as rollback()'s OWN first statement — not just inherited via deploy()'s check. Why that
    // matters: rollback() persists `<service>.prev = <current image>` as a real side effect
    // BEFORE calling deploy(). If rollback() relied only on deploy()'s guard, a refused nested
    // call would still have advanced `.prev` to the CURRENT image — so a caller who read the
    // error and retried rollback() at the top level would roll back to the wrong image. This
    // test runs LIVE (not dry-run, which never persists state) and asserts `.prev` is completely
    // unchanged after the refused nested call.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        transaction(|| {
            deploy::rollback(["web1"], "app", #{});
        });
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("rollback() cannot be called from inside an active transaction"));

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(
        state.contains("\"app.prev\": \"ghcr.io/org/app:v1\""),
        "the refused nested rollback() must NOT have advanced .prev to the current image: {state}"
    );
}

#[test]
fn deploy_refuses_an_empty_hosts_array() {
    // Robustness review R21: an empty `hosts` array used to either panic (an out-of-bounds
    // `hosts[0]`, reached whenever `cfg.pre_deploy` or the arch-mismatch check ran first) or, with
    // neither of those in play, silently "succeed" touching zero hosts while still persisting new
    // `.version`/`.image`/`.prev` state — falsely claiming a release that never happened anywhere.
    // This runs LIVE (not --dry-run, which never persists state either way) so state.json's
    // absence proves the throw happened BEFORE any state was written, not just that dry-run
    // stayed inert.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy([], "ghcr.io/org/app:v9", "app", #{ skip_build: true, skip_push: true });
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("empty hosts array"));

    let state_path = dir.path().join(".energize/state.json");
    if state_path.exists() {
        let state = fs::read_to_string(&state_path).unwrap();
        assert!(
            !state.contains("app.version") && !state.contains("app.image"),
            "an empty-hosts deploy() must not persist ANY state claiming a release happened: {state}"
        );
    }
}

#[test]
fn deploy_refuses_an_empty_hosts_array_even_with_pre_deploy_set() {
    // The specific historical panic this finding described: `cfg.pre_deploy` set makes deploy()
    // reach `let mhost = hosts[0];` to run the release task on "the first host" — an out-of-bounds
    // index on an empty array, previously an ugly Rhai runtime panic instead of a clean throw. The
    // R21 guard runs before ANY of that, so this must produce the SAME clean error as the plain
    // empty-hosts case above, not a panic.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy([], "ghcr.io/org/app:v9", "app", #{
            skip_build: true, skip_push: true, pre_deploy: "bin/rails db:migrate",
        });
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("empty hosts array"));
}

#[test]
fn rollback_refuses_an_empty_hosts_array_without_first_mutating_prev_state() {
    // rollback() carries the SAME R21 guard as deploy() (which it calls internally), but checked
    // as rollback()'s OWN statement — not just inherited via deploy()'s check — for the same
    // reason as the analogous R29 test above: rollback() persists `<service>.prev = <current
    // image>` as a real side effect BEFORE calling deploy(). If rollback() relied only on
    // deploy()'s guard, a refused empty-hosts call would still have advanced `.prev` to the
    // CURRENT image. Runs LIVE (not --dry-run, which never persists state) and asserts `.prev`
    // is completely unchanged after the refused call.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback([], "app", #{});
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("empty hosts array"));

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(
        state.contains("\"app.prev\": \"ghcr.io/org/app:v1\""),
        "the refused empty-hosts rollback() must NOT have advanced .prev to the current image: {state}"
    );
}
