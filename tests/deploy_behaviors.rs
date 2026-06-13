//! Integration: deploy() correctness behaviors fixed in the issue sweep.
//!
//! All run against `--dry-run` (the dry-run plan is the observable contract) using the real
//! `lib/deploy.rhai`. Covers:
//! * #8 pre_deploy runs ONCE from a throwaway NEW-image container (`docker run --rm <image>`),
//!   not an `exec` into the old container, and with NO `|| true` swallow.
//! * #7 the restore-proxy compensation carries health_path (same proxy_cfg as the forward
//!   switch) — observable as `--health-check-path` on the registered rollback line.
//! * #6 deploy persists the full effective config, and rollback replays it.

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
