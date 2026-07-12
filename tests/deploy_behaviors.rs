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
use predicates::prelude::PredicateBooleanExt;
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
fn deploy_omits_keep_images_from_persisted_config_when_never_set() {
    // Robustness review R22 (found during Opus review of this slice — the persisted config's
    // `keep_images` handling had no direct regression test): `keep_images` defaults to an internal
    // -1 "not set at all" sentinel, distinct from a caller-chosen 0. If that sentinel were EVER
    // persisted into <service>.config, every future rollback() would replay a cfg that
    // `.contains("keep_images")` with value -1 — and since deploy()'s own validation guard is
    // `cfg.contains("keep_images") && keep_images < 0`, that would make EVERY subsequent rollback
    // of that service throw "negative cfg.keep_images", permanently breaking rollback. So the key
    // must be entirely ABSENT from the persisted config whenever the caller never set it.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            skip_build: true, skip_push: true,
        });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("deploy must persist <service>.config:\n{plan}"));
    assert!(
        !line.contains("keep_images"),
        "keep_images must be entirely absent from the persisted config when never set — \
         persisting the -1 sentinel would permanently break every future rollback(): {line}"
    );
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
fn rollback_refuses_a_negative_keep_images_override_without_first_mutating_prev_state() {
    // Robustness review R22 (found during this slice's own FINAL review — Fable): rollback()
    // persists `<service>.prev = <current image>` as a real side effect BEFORE calling deploy(),
    // which is where cfg.keep_images's own negative-value validation lives. Without rollback()
    // carrying its OWN up-front copy of that same guard, a caller-supplied
    // `#{keep_images: -1}` override would still corrupt `.prev` to the CURRENT (possibly broken)
    // image before deploy()'s validation throws — a caller who fixed the typo and retried
    // `rollback(hosts, service)` with no override would then "roll back" to the very image they
    // were trying to escape, the real target permanently lost. Same R21/R29-style fix: checked as
    // rollback()'s own first statement, not just inherited via deploy()'s check. Runs LIVE (not
    // --dry-run, which never persists state) and asserts `.prev` is completely unchanged after the
    // refused call.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app", #{ keep_images: -1 });
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
        .stderr(predicates::str::contains("negative cfg.keep_images"))
        .stderr(predicates::str::contains("robustness review R22"));

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(
        state.contains("\"app.prev\": \"ghcr.io/org/app:v1\""),
        "the refused rollback() must NOT have advanced .prev to the current image: {state}"
    );
}

#[test]
fn rollback_refuses_a_replayed_domain_on_kamal_proxy_without_first_mutating_prev_state() {
    // Fable's final review of the domain/kamal-proxy fail-loud fix: deploy() itself refuses a
    // `domain` set on the kamal-proxy backend, but rollback() replays the PERSISTED config (plus
    // any caller override) through deploy() only AFTER already persisting `.prev = <current
    // image>` as a real side effect — the same R21/R29/keep_images-style hazard. A service last
    // deployed with `proxy: "caddy", domain: "..."` whose persisted config is then rolled back
    // with an override that switches it to kamal-proxy (or a service whose persisted config
    // itself somehow carries a domain without caddy) must be refused BEFORE `.prev` moves, not
    // after — matching every other rollback() precondition already fixed this same way.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        state_set("app.config", to_json(#{ proxy: "caddy", domain: "app.example.com" }));
        deploy::rollback(["web1"], "app", #{ proxy: "kamal" });
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
        .stderr(predicates::str::contains("does not support domain-based routing"));

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(
        state.contains("\"app.prev\": \"ghcr.io/org/app:v1\""),
        "the refused rollback() must NOT have advanced .prev to the current image: {state}"
    );
}

#[test]
fn rollback_refuses_when_the_lock_is_already_held_without_first_mutating_prev_state() {
    // Deferred robustness finding (found reviewing the `nrg rollback` CLI, round 5): rollback()
    // now takes the SAME cross-machine deploy lock (R15) as deploy() itself, but BEFORE mutating
    // `.prev` — not only relying on deploy()'s own acquire, which used to run only AFTER `.prev`
    // had already been overwritten. This is the SAME hazard class as the R29/R21/keep_images/
    // domain guards above, just for R15's lock instead of a pure-Rhai precondition. This LIVE run
    // (dry-run never persists state) uses a fake `ssh` reporting the lock directory as already
    // held (`mkdir ... File exists`), and asserts `.prev` is completely unchanged afterward.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app", #{});
    "#,
    )
    .unwrap();

    let bin = tempfile::tempdir().unwrap();
    let script = "#!/bin/sh\ncase \"$*\" in\n  *mkdir*nrg-deploy-lock-app*) echo \"mkdir: cannot create directory 'nrg-deploy-lock-app': File exists\" >&2; exit 1 ;;\n  *) exit 0 ;;\nesac\n";
    let ssh_bin = bin.path().join("ssh");
    fs::write(&ssh_bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&ssh_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&ssh_bin, perms).unwrap();
    }
    let path_env = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("already locked"));

    let state = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(
        state.contains("\"app.prev\": \"ghcr.io/org/app:v1\""),
        "the refused rollback() must NOT have advanced .prev when the lock was already held: {state}"
    );
}

#[test]
fn rollback_acquires_and_releases_the_lock_exactly_once_not_once_per_nested_deploy_call() {
    // Companion to the refusal test above: rollback() now holds the R15 lock for its OWN whole
    // duration (the `.prev` mutation AND the nested deploy() call), with `replay.skip_lock` forced
    // true so the nested deploy() doesn't try to acquire the SAME lock a second time — which would
    // throw "already locked" against itself, since rollback() is already holding it. This dry-run
    // plan proves exactly one acquire/release pair for the whole rollback, not one per level.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app", #{});
    "#,
    );
    let mkdir_count =
        plan.lines().filter(|l| l.contains("mkdir") && l.contains("nrg-deploy-lock-app")).count();
    let rm_count =
        plan.lines().filter(|l| l.contains("rm -rf") && l.contains("nrg-deploy-lock-app")).count();
    assert_eq!(mkdir_count, 1, "expected exactly one lock acquire (not one per level): {plan}");
    assert_eq!(rm_count, 1, "expected exactly one lock release (not one per level): {plan}");
}

#[test]
fn rollback_releases_the_lock_even_when_the_nested_deploy_call_fails_after_acquiring_it() {
    // Opus review (lock-order slice, round 5): neither test above exercises the try/catch's
    // release-on-FAILURE path specifically — the refusal test above fails at ACQUIRE (deploy()
    // never runs at all), and the "exactly once" test above only exercises the trailing
    // SUCCESS-path release. This is the one path those two leave uncovered: the lock is acquired
    // successfully, but a LATER step inside the nested deploy() call (the image pull) fails —
    // rollback() must still release the lock before re-throwing the original error, not leak it
    // (confirmed by reverting just the try/catch wrapping — the OTHER two tests still passed).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web1"], "app", #{});
    "#,
    )
    .unwrap();

    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("ssh_argv.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log:?}\ncase \"$*\" in\n  *pull*) echo 'Error: rate limit exceeded' >&2; exit 1 ;;\n  *) exit 0 ;;\nesac\n"
    );
    let ssh_bin = bin.path().join("ssh");
    fs::write(&ssh_bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&ssh_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&ssh_bin, perms).unwrap();
    }
    let path_env = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", &path_env)
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("pull failed"));

    let invoked = fs::read_to_string(&log).unwrap();
    assert!(
        invoked.lines().any(|l| l.contains("mkdir") && l.contains("nrg-deploy-lock-app")),
        "the lock must have been acquired before the pull failure: {invoked}"
    );
    assert!(
        invoked.lines().any(|l| l.contains("rm -rf") && l.contains("nrg-deploy-lock-app")),
        "the lock must still be released after a LATER deploy() step fails: {invoked}"
    );
}

fn lock_host_in_plan(plan: &str) -> String {
    plan.lines()
        .find(|l| l.contains("mkdir") && l.contains("nrg-deploy-lock-app"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_else(|| panic!("no deploy-lock mkdir line found in plan:\n{plan}"))
        .to_string()
}

#[test]
fn deploy_anchors_the_lock_on_the_alphabetically_first_host_regardless_of_array_order() {
    // Full-project Fable review: the R15 cross-machine lock used to be taken on `hosts[0]` — an
    // in-flight, order-dependent choice. Two operators deploying the exact same fleet with a
    // differently-ordered host array (e.g. ["web2","web1"] vs ["web1","web2"]) took the lock on
    // DIFFERENT hosts, silently defeating the mutual exclusion the lock exists to provide. Both
    // orders below must anchor on the same (alphabetically-first) host, "web1".
    let reversed = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web2", "web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );
    let forward = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1", "web2"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );

    assert_eq!(
        lock_host_in_plan(&reversed),
        "web1",
        "lock host must be order-independent:\n{reversed}"
    );
    assert_eq!(
        lock_host_in_plan(&forward),
        "web1",
        "lock host must be order-independent:\n{forward}"
    );
}

#[test]
fn deploy_still_rolls_out_to_hosts_in_the_callers_given_order_despite_the_sorted_lock_host() {
    // The lock host anchor is sorted, but the actual rolling-deploy sequence must still follow
    // exactly the order the caller gave `hosts` in — sorting a COPY for the lock, not `hosts`
    // itself, is the whole point (see lock_host_for's doc comment in lib/deploy.rhai).
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web2", "web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
        });
    "#,
    );
    let first_pull_host = plan
        .lines()
        .find(|l| l.contains("docker pull"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap();
    assert_eq!(
        first_pull_host, "web2",
        "the rolling deploy order must still be web2-then-web1 as the caller gave it:\n{plan}"
    );
}

#[test]
fn rollback_anchors_the_lock_on_the_alphabetically_first_host_regardless_of_array_order() {
    // Companion to the deploy() test above: rollback() takes its OWN separate lock (before the
    // `.prev` mutation) via the same `lock_host_for` helper — verify its call site independently.
    let reversed = plan_for(
        r#"
        import "lib/deploy" as deploy;
        state_set("app.image", "ghcr.io/org/app:v2");
        state_set("app.prev", "ghcr.io/org/app:v1");
        deploy::rollback(["web2", "web1"], "app", #{});
    "#,
    );
    assert_eq!(
        lock_host_in_plan(&reversed),
        "web1",
        "rollback's lock host must be order-independent:\n{reversed}"
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

#[test]
fn deploy_refuses_a_negative_keep_images() {
    // Robustness review R22: cfg.keep_images (tagged-image retention) is strictly opt-in — a
    // caller who explicitly sets it must supply a valid non-negative count, or get a clear error
    // rather than a confusingly-behaving prune. Checked up front (before any host work), so this
    // must fail the same under --dry-run as live.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            skip_build: true, skip_push: true, keep_images: -1,
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
        .stderr(predicates::str::contains("negative cfg.keep_images"));
}

#[test]
fn deploy_with_keep_images_zero_is_a_valid_meaningful_value() {
    // 0 is deliberately NOT the same as "unset" — it means "prune every other tag right down to
    // just the protected current/previous versions" (unset means "don't prune tagged images at
    // all"). Must not be rejected by the negative-value guard.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            skip_build: true, skip_push: true, keep_images: 0,
        });
    "#,
    );
    assert!(
        plan.contains("app.version ="),
        "keep_images: 0 must be accepted and let the deploy run to completion:\n{plan}"
    );
}

#[test]
fn standard_deploy_forwards_keep_images_to_deploy() {
    // Robustness review R22: standard_deploy's cfg-forwarding loop must include keep_images like
    // every other real deploy() cfg key, checked via the persisted <service>.config state line
    // (deploy()'s own observable contract for its effective cfg).
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            keep_images: 3,
        });
    "#,
    );
    let config_line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("no persisted app.config state line found:\n{plan}"));
    assert!(config_line.contains("\"keep_images\":3"), "got: {config_line}");
}

#[test]
fn standard_deploy_refuses_missing_required_keys() {
    // Robustness review R23: standard_deploy used to access required keys (service, image_repo,
    // web_hosts, ...) directly with no existence check — a caller who forgot one got no clear
    // message naming what's missing (a missing key silently reads as unit in this Rhai config, not
    // an error, so the actual failure ranged from a fully silent malformed deploy to an opaque
    // "Function not found" error deep in an unrelated module, never one naming the real cause).
    // Checked up front here: a cfg missing "service" entirely.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{ image_repo: "ghcr.io/org/app", web_hosts: ["web1"] });
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
        .stderr(predicates::str::contains("missing required cfg key 'service'"))
        .stderr(predicates::str::contains("robustness review R23"));
}

#[test]
fn standard_deploy_refuses_missing_registry_credentials_when_registry_is_set() {
    // A caller who sets cfg.registry (wanting a login step) but forgets registry_user or
    // registry_password used to hit the same opaque property error INSIDE registry_login,
    // instead of a clear message pointing at the actual missing key.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"],
            registry: "ghcr.io", registry_user: "deploy",
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
        .stderr(predicates::str::contains("'registry_password' is missing"))
        .stderr(predicates::str::contains("robustness review R23"));
}

#[test]
fn standard_deploy_refuses_missing_db_host_when_accessories_set() {
    // A caller with accessories configured but no db_host used to hit the same opaque error at
    // `deploy::accessory_run(cfg.db_host, ...)` inside the accessories loop.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"],
            accessories: [ #{ name: "app-db", image: "postgres:16" } ],
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
        .stderr(predicates::str::contains("'db_host' is missing"))
        .stderr(predicates::str::contains("robustness review R23"));
}

#[test]
fn standard_deploy_refuses_an_accessory_entry_missing_required_keys() {
    // Found reviewing R23 itself: the top-level cfg keys were guarded, but each accessory MAP's
    // own required keys (name, image) were still accessed directly one level deeper — the same
    // class of unclear failure (silent malformed value, or an opaque error deep in an unrelated
    // module), just moved down a level instead of eliminated.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"],
            db_host: "db1", accessories: [ #{ image: "postgres:16" } ],
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
        .stderr(predicates::str::contains("missing required key 'name'"))
        .stderr(predicates::str::contains("robustness review R23"));
}

#[test]
fn standard_deploy_forwards_network_to_accessories() {
    // Robustness review R23: cfg.network was already forwarded to the app's own deploy() call,
    // but NOT to accessories — so a caller on a custom Docker network got their app container
    // joined to it while the DB/cache accessory stayed on the default bridge network, unable to
    // resolve each other by container name. Both the accessory's `docker run` and the app's own
    // `docker run` must now carry the SAME `--network` flag.
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app:v9", web_hosts: ["web1"],
            db_host: "db1", network: "appnet", skip_build: true, skip_push: true,
            accessories: [ #{ name: "app-db", image: "postgres:16" } ],
        });
    "#,
    );
    let accessory_line = plan
        .lines()
        .find(|l| l.contains("docker run") && l.contains("app-db"))
        .unwrap_or_else(|| panic!("no docker run line for the accessory found:\n{plan}"));
    assert!(
        accessory_line.contains("--network 'appnet'") || accessory_line.contains("--network appnet"),
        "accessory container must join cfg.network: {accessory_line}"
    );
    let app_line = plan
        .lines()
        .find(|l| l.contains("docker run") && l.contains("app-web"))
        .unwrap_or_else(|| panic!("no docker run line for the app found:\n{plan}"));
    assert!(
        app_line.contains("--network 'appnet'") || app_line.contains("--network appnet"),
        "app container must still join cfg.network (unchanged behavior): {app_line}"
    );
}

#[test]
fn standard_deploy_forwards_health_check_knobs_to_deploy() {
    // Robustness review R12 addendum (found while wiring health_consecutive/health_timeout
    // through standard_deploy): health_attempts/health_interval are documented in
    // docs/examples.md as deploy::deploy()'s own cfg keys, which standard_deploy wraps — but they
    // were NEVER actually forwarded from standard_deploy's cfg to the dcfg it builds for that
    // wrapped call. A caller setting health_attempts: 60 on standard_deploy silently got
    // deploy()'s default 30 instead. The two new R12 knobs (health_consecutive, health_timeout)
    // must forward too. Checked via the persisted `<service>.config` state line in the dry-run
    // plan (deploy()'s own observable contract for its effective cfg).
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            health_attempts: 60, health_interval: 5, health_consecutive: 3, health_timeout: 10,
        });
    "#,
    );
    let config_line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("no persisted app.config state line found:\n{plan}"));
    assert!(config_line.contains("\"health_attempts\":60"), "got: {config_line}");
    assert!(config_line.contains("\"health_interval\":5"), "got: {config_line}");
    assert!(config_line.contains("\"health_consecutive\":3"), "got: {config_line}");
    assert!(config_line.contains("\"health_timeout\":10"), "got: {config_line}");
}

#[test]
fn standard_deploy_forwards_volumes_and_deploy_hook_cmds_to_deploy() {
    // Robustness review R23c (found reviewing the R12 addendum above — same bug class, a bigger
    // sweep): standard_deploy silently dropped several other real deploy() cfg keys it never
    // forwarded. Checked here via the persisted `<service>.config` state line: `volumes`,
    // `pre_deploy_cmd`, `post_deploy_cmd`.
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            volumes: #{ "app-data": "/data" },
            pre_deploy_cmd: "echo before", post_deploy_cmd: "echo after",
        });
    "#,
    );
    let config_line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("no persisted app.config state line found:\n{plan}"));
    assert!(config_line.contains("\"app-data\":\"/data\""), "got: {config_line}");
    assert!(config_line.contains("\"pre_deploy_cmd\":\"echo before\""), "got: {config_line}");
    assert!(config_line.contains("\"post_deploy_cmd\":\"echo after\""), "got: {config_line}");
}

#[test]
fn standard_deploy_forwards_build_and_skip_flags_to_deploy() {
    // Robustness review R23c: standard_deploy also silently dropped build_context/dockerfile/
    // build_args/platform/skip_build/skip_push — none of these are part of the REPLAYED
    // effective_cfg (build/push are forced off on rollback replay regardless), so they're checked
    // via the dry-run plan's own Build section instead of the persisted state line.
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            build_context: "backend", dockerfile: "Dockerfile.prod",
            build_args: #{ "FOO": "bar" }, platform: "linux/arm64",
        });
    "#,
    );
    let build_line = plan
        .lines()
        .find(|l| l.contains("buildx build") || l.contains("docker build"))
        .unwrap_or_else(|| panic!("no docker/buildx build line found:\n{plan}"));
    assert!(build_line.contains("--platform 'linux/arm64'"), "got: {build_line}");
    assert!(build_line.contains("-f 'Dockerfile.prod'"), "got: {build_line}");
    assert!(build_line.contains("--build-arg 'FOO=bar'"), "got: {build_line}");
    assert!(build_line.contains("'backend'"), "got: {build_line}");

    // skip_build/skip_push: no Build/Push section at all when both are set.
    let plan2 = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            skip_build: true, skip_push: true,
        });
    "#,
    );
    assert!(
        !plan2.lines().any(|l| l.contains("buildx build") || l.contains("docker build")),
        "skip_build: true must suppress the build step entirely:\n{plan2}"
    );
    assert!(
        !plan2.lines().any(|l| l.contains("docker push")),
        "skip_push: true must suppress the push step entirely:\n{plan2}"
    );
}

#[test]
fn standard_deploy_logs_into_the_registry_on_build_host_too() {
    // Fable final review (roadmap 1.1 step 3a): with cfg.build_host set, the build (and push)
    // run THERE, not locally or on any web_host — that host needs its own registry session too,
    // or a private base-image pull during the build (and the push afterward) fails live with an
    // opaque "unauthorized". standard_deploy's registry-login step must log in on build_host in
    // addition to "local" and web_hosts.
    let script = r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            registry: "ghcr.io", registry_user: "deploy", registry_password: secret("REGISTRY_PASSWORD"),
            build_host: "builder1",
        });
    "#;
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(dir.path().join("Energize.rhai"), script).unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_tokenvalue123")
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();
    let plan = String::from_utf8_lossy(&out.stdout);
    let login_line = plan
        .lines()
        .find(|l| l.contains("login") && l.contains("builder1"))
        .unwrap_or_else(|| panic!("no registry login line targeting build_host in plan:\n{plan}"));
    assert!(
        login_line.trim_start().starts_with("ssh-stdin"),
        "login on build_host must run via ssh_exec_stdin (off-argv password), not locally: {login_line}"
    );

    // Regression: no build_host set — must NOT try to log in anywhere but local/web_hosts (no
    // stray login line with an empty/missing host).
    let script2 = r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            registry: "ghcr.io", registry_user: "deploy", registry_password: secret("REGISTRY_PASSWORD"),
        });
    "#;
    let dir2 = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir2.path().join(".energize")).unwrap();
    link_lib(dir2.path());
    fs::write(dir2.path().join("Energize.rhai"), script2).unwrap();
    let out2 = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir2.path())
        .env("NRG_SECRET_REGISTRY_PASSWORD", "ghp_tokenvalue123")
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .success()
        .get_output()
        .clone();
    let plan2 = String::from_utf8_lossy(&out2.stdout);
    let login_lines: Vec<&str> = plan2.lines().filter(|l| l.contains("login")).collect();
    assert_eq!(
        login_lines.len(),
        2,
        "expected exactly 2 logins (local + web1) with no build_host set:\n{plan2}"
    );
}

#[test]
fn standard_deploy_forwards_port_rename_and_remaining_deploy_keys() {
    // Robustness review R23c's suggested structural refactor (implemented now): standard_deploy's
    // cfg forwarding switched from a hand-maintained ALLOWLIST of deploy() keys (which drifted out
    // of sync three separate times — R12's health knobs, R23c's nine keys, R22's keep_images, each
    // a real caller-facing bug: a cfg key silently ignored with no error) to a DENYLIST of
    // standard_deploy's OWN ~11 keys, forwarding everything else automatically. This covers the
    // handful of real deploy() cfg keys that had no dedicated standard_deploy forwarding test
    // before this refactor: the `port` -> `container_port` rename, `envs`, `health_path`,
    // `proxy`, `domain`.
    let plan = plan_for(
        r#"
        import "lib/recipe" as recipe;
        recipe::standard_deploy(#{
            service: "app", image_repo: "ghcr.io/org/app", web_hosts: ["web1"], version: "v9",
            port: 4001, envs: #{ "FOO": "bar" }, health_path: "/healthz",
            proxy: "caddy", domain: "app.example.com",
        });
    "#,
    );
    let config_line = plan
        .lines()
        .find(|l| l.contains("app.config ="))
        .unwrap_or_else(|| panic!("no persisted app.config state line found:\n{plan}"));
    assert!(config_line.contains("\"container_port\":4001"), "got: {config_line}");
    assert!(config_line.contains("\"FOO\":\"bar\""), "got: {config_line}");
    assert!(config_line.contains("\"health_path\":\"/healthz\""), "got: {config_line}");
    assert!(config_line.contains("\"proxy\":\"caddy\""), "got: {config_line}");
    assert!(config_line.contains("\"domain\":\"app.example.com\""), "got: {config_line}");
}

#[test]
fn deploy_refuses_a_domain_on_the_default_kamal_proxy_backend() {
    // New finding, found during R12's own Fable review: `cfg.domain` reaches Caddy's route for
    // automatic HTTPS (lib/caddy.rhai), but kamal-proxy's own proxy_deploy (lib/proxy.rhai) never
    // reads `cfg.domain` at all — so setting `domain` while on the default (kamal-proxy) backend
    // used to be silently dropped, with no TLS/host routing and no error. Must fail loud instead,
    // matching R20/R23c's "don't silently drop a cfg key" direction, rather than deploying
    // successfully with the domain quietly ignored.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            domain: "app.example.com",
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
        .stderr(predicates::str::contains("does not support domain-based routing"))
        // Fable's final review: without this assertion, the test can't tell whether the
        // fail-fast check at the TOP of deploy() actually fired, or whether it silently
        // regressed back to only being caught much later by px_deploy's defense-in-depth copy
        // (reached only after build/push/pre_deploy/the first host's container start and health
        // wait had already run) — both produce the identical error message. "==> Deploying" is
        // deploy()'s own first `print()` (routed to stderr), emitted only AFTER the fail-fast
        // check; its absence proves the throw happened before any of that work started.
        .stderr(predicates::str::contains("==> Deploying").not());
}

#[test]
fn wait_healthy_refuses_zero_or_negative_attempts() {
    // Robustness review R26: `attempts <= 0` made wait_healthy's retry loop run zero iterations,
    // leaving its `r` an empty map — the subsequent fail message's `r.status` read then silently
    // produced unit (this engine's default Rhai config, not `fail_on_invalid_map_property`),
    // giving a confusing "Health check failed after 0 attempts: <url> (last status: )" with the
    // status left blank, no hint the real problem was `attempts` itself. Runs live (dry-run's
    // sim_http_healthy always synthesizes a healthy 200 before the loop even matters) so the
    // guard is what's actually reached.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy("http://127.0.0.1:1/up", #{ attempts: 0 });
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
        .stderr(predicates::str::contains("cfg.attempts must be >= 1"))
        .stderr(predicates::str::contains("robustness review R26"))
        .stderr(predicates::str::contains("last status:").not());
}

#[test]
fn wait_port_and_wait_container_healthy_also_refuse_zero_or_negative_attempts() {
    // R26 consistency companions: wait_port/wait_container_healthy don't crash on `attempts <= 0`
    // (no uninitialized-map read like wait_healthy), but they'd otherwise silently "succeed at
    // waiting" for zero attempts and throw a "not open/healthy after 0 attempts" message that
    // masks the real misconfiguration just as much. Same guard, same clear message.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_port("web1", 3000, #{ attempts: 0 });
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
        .stderr(predicates::str::contains("wait_port: cfg.attempts must be >= 1"))
        .stderr(predicates::str::contains("robustness review R26"));

    let dir2 = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir2.path().join(".energize")).unwrap();
    link_lib(dir2.path());
    fs::write(
        dir2.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_container_healthy("web1", "app", #{ attempts: -1 });
    "#,
    )
    .unwrap();
    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir2.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("wait_container_healthy: cfg.attempts must be >= 1"))
        .stderr(predicates::str::contains("robustness review R26"));
}

#[test]
fn wait_healthy_refuses_zero_or_negative_consecutive() {
    // Robustness review R12: cfg.consecutive must be >= 1 — a caller passing 0 or negative would
    // otherwise get an unclear "healthy after 0 consecutive passes" outcome instead of a message
    // naming the actual misconfiguration.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy("http://127.0.0.1:1/up", #{ consecutive: 0 });
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
        .stderr(predicates::str::contains("cfg.consecutive must be >= 1"))
        .stderr(predicates::str::contains("robustness review R12"));
}

#[test]
fn wait_healthy_refuses_zero_or_negative_timeout() {
    // Robustness review R12: cfg.timeout must be >= 1 (seconds) — same reasoning as `consecutive`.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy("http://127.0.0.1:1/up", #{ timeout: -5 });
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
        .stderr(predicates::str::contains("cfg.timeout must be >= 1"))
        .stderr(predicates::str::contains("robustness review R12"));
}

#[test]
fn wait_healthy_on_host_refuses_zero_or_negative_attempts() {
    // Robustness review R7-health: wait_healthy_on_host carries the same input-validation guards
    // as wait_healthy (attempts/consecutive/timeout must all be >= 1).
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy_on_host("web1", 3000, #{ attempts: 0 });
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
        .stderr(predicates::str::contains("wait_healthy_on_host: cfg.attempts must be >= 1"));
}

#[test]
fn wait_healthy_on_host_refuses_zero_or_negative_consecutive() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy_on_host("web1", 3000, #{ consecutive: 0 });
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
        .stderr(predicates::str::contains("wait_healthy_on_host: cfg.consecutive must be >= 1"));
}

#[test]
fn wait_healthy_on_host_refuses_zero_or_negative_timeout() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy_on_host("web1", 3000, #{ timeout: -5 });
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
        .stderr(predicates::str::contains("wait_healthy_on_host: cfg.timeout must be >= 1"));
}

#[test]
fn wait_healthy_all_checks_each_host_via_ssh_not_a_control_machine_url() {
    // Robustness review R7-health: wait_healthy_all had the SAME bug as deploy_one_host's own
    // health gate — building "http://" + host + ":" + port + path and GETting it from the
    // control machine. Confirmed here via the dry-run plan: under dry-run wait_healthy_on_host's
    // probe short-circuits with NO ssh_exec call at all (nothing to record), so the absence of any
    // "http://<host>..." plan line (which the OLD control-machine-GET implementation would have
    // synthesized via sim_http_healthy's own dry-run "[assumed healthy] GET ..." record) proves
    // the control-machine code path is no longer reachable at all.
    let plan = plan_for(
        r#"
        import "lib/healthcheck" as health;
        health::wait_healthy_all(["deploy@web1", "deploy@web2"], 3000, #{ path: "/up" });
    "#,
    );
    assert!(
        !plan.contains("GET http://deploy@web1") && !plan.contains("GET http://deploy@web2"),
        "must never GET a URL built from the raw ssh alias from the control machine:\n{plan}"
    );
}
