//! Integration: `cfg.platform` on `docker_build`/`deploy()` (roadmap item 1.1, multi-arch
//! builds), asserted against the dry-run plan (the observable contract for anything routed
//! through `local_exec`, which is stubbed under `--dry-run`).
//!
//! The arch-MISMATCH check itself (`check_arch_mismatch` in lib/deploy.rhai) only runs on a
//! LIVE deploy — it needs a real `uname -m` on both sides, which `--dry-run` can't provide (see
//! that function's doc comment). Asserting the actual THROW would need a second real host with
//! a genuinely different architecture, which isn't available in this environment; instead these
//! tests cover: (a) the check is REACHED and safely no-ops under `--dry-run` (proving it's wired
//! up, not dead code), (b) it's correctly SKIPPED when the caller already set `cfg.platform`, and
//! (c) `docker_build`'s command construction itself (buildx vs. plain build) is exercised via the
//! plan. This mirrors the existing test suite's documented limit on exercising the live deploy
//! path (see docs/robustness-review.md, R8).

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn link_lib(dir: &Path) {
    let repo_lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo_lib, dir.join("lib")).unwrap();
}

fn plan_for(script: &str) -> String {
    let (stdout, _stderr) = plan_and_stderr_for(script);
    stdout
}

fn plan_and_stderr_for(script: &str) -> (String, String) {
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
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn docker_build_without_platform_uses_plain_build() {
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{});
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(line.contains(" build -t "), "expected a plain build: {line}");
    assert!(!line.contains("buildx"), "must not use buildx when no platform is set: {line}");
}

#[test]
fn docker_build_with_platform_uses_buildx_and_loads_locally() {
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ platform: "linux/amd64" });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(line.contains("buildx build --platform 'linux/amd64' --load"), "got: {line}");
}

#[test]
fn docker_build_platform_value_is_shell_quoted() {
    // Same shell-safety contract as every other caller-supplied value in lib/docker.rhai.
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ platform: "linux/amd64; rm -rf /" });
    "#,
    );
    let line = plan.lines().find(|l| l.contains("buildx")).unwrap_or_else(|| panic!("got:\n{plan}"));
    assert!(line.contains("'linux/amd64; rm -rf /'"), "got: {line}");
}

#[test]
fn deploy_without_platform_reaches_the_arch_check_and_dry_run_skips_it_safely() {
    // Proves check_arch_mismatch is actually WIRED into deploy() (not dead code) and that it
    // no-ops under --dry-run instead of throwing or blocking the plan.
    let (plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: true,
        });
    "#,
    );
    assert!(
        plan.contains("PLAN (dry run"),
        "the check must not abort the plan under --dry-run:\n{plan}"
    );
    assert!(
        stderr.contains("skipping build-arch check"),
        "the check must actually run (and note the dry-run skip), not be silently bypassed:\n{stderr}"
    );
}

#[test]
fn docker_build_with_comma_separated_platform_uses_buildx_and_pushes_the_manifest_list() {
    // Roadmap 1.1 step 3b: a comma-separated platform list is a genuine multi-platform
    // manifest-list build — buildx can't `--load` more than one platform, so this must use
    // `--push` instead, publishing the manifest list straight to the registry during the build.
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ platform: "linux/amd64,linux/arm64" });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(
        line.contains("buildx build --platform 'linux/amd64,linux/arm64' --push"),
        "got: {line}"
    );
    assert!(!line.contains("--load"), "a multi-platform build must not use --load: {line}");
}

#[test]
fn deploy_with_comma_separated_platform_skips_the_separate_push_step() {
    // docker_build already pushed the manifest list via --push during build, so deploy() must
    // not also run a separate `docker push` — nothing local exists under that tag to push, and
    // buildx --push never loads one.
    let (plan, _stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: false,
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    );
    assert!(
        !plan.lines().any(|l| l.contains(" push ") && l.contains("'ghcr.io/org/app:v1'")),
        "must not run a separate docker push for a multi-platform build:\n{plan}"
    );
    let build_line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(build_line.contains("buildx build --platform 'linux/amd64,linux/arm64' --push"));
}

#[test]
fn deploy_with_comma_separated_platform_and_skip_build_does_not_claim_buildx_already_pushed() {
    // Opus review: with cfg.skip_build set, docker_build never ran, so buildx never pushed
    // anything — the informational skip message must not claim it did.
    let (_plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: true, skip_push: false,
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    );
    assert!(
        !stderr.contains("buildx already pushed the manifest list during build"),
        "must not claim buildx pushed anything when skip_build is set:\n{stderr}"
    );
    assert!(
        stderr.contains("cfg.skip_build is set"),
        "expected the skip_build-specific skip message:\n{stderr}"
    );
}

#[test]
fn deploy_with_comma_separated_platform_still_skips_the_arch_check() {
    // Confirms the existing "any non-empty platform skips check_arch_mismatch" guard also
    // covers a comma-separated platform list, with no code changes needed there.
    let (_plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: true,
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    );
    assert!(
        !stderr.contains("skipping build-arch check"),
        "the check must never run when cfg.platform is already set (comma-separated or not):\n{stderr}"
    );
}

#[test]
fn deploy_with_explicit_platform_skips_the_arch_check_entirely_and_passes_it_to_docker_build() {
    // With cfg.platform already set, the caller has made an intentional cross-arch choice —
    // deploy() must not even attempt the check (dry-run or live) — AND must actually pass
    // cfg.platform through to docker_build (a regression here would break the feature with
    // every OTHER test in this file still green, since they all call docker_build directly).
    let (plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: true,
            platform: "linux/amd64",
        });
    "#,
    );
    assert!(
        !stderr.contains("skipping build-arch check"),
        "the check must never even run when cfg.platform is already set:\n{stderr}"
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(
        line.contains("buildx build --platform 'linux/amd64' --load"),
        "deploy() must pass cfg.platform through to docker_build: {line}"
    );
}
