//! Integration: `cfg.platform` and `cfg.build_host` on `docker_build`/`deploy()` (roadmap item
//! 1.1, multi-arch builds — steps 3b and 3a respectively), asserted against the dry-run plan (the
//! observable contract for anything routed through `local_exec`/`ssh_exec`/`ssh_exec_stdin`,
//! which are all stubbed under `--dry-run`).
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
    // covers a comma-separated platform list, with no code changes needed there. skip_push must
    // be false here: skip_push: true + a comma-separated platform is refused by a separate
    // fail-fast check (see deploy_with_skip_push_and_comma_separated_platform_fails_fast below).
    let (_plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: false,
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

#[test]
fn deploy_with_skip_push_and_comma_separated_platform_fails_fast() {
    // Fable final review: buildx can only publish a multi-platform manifest list via `--push`
    // at build time (no `--load`-only variant exists for more than one platform), so
    // cfg.skip_push can't actually be honored for a comma-separated platform — a real build
    // this call would write to the registry regardless of skip_push. Silently ignoring an
    // explicit skip_push: true would violate the caller's instruction, so deploy() must refuse
    // this cfg combination up front instead.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    link_lib(dir.path());
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: false, skip_push: true,
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    )
    .unwrap();
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("--dry-run")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cfg.skip_push is set") && stderr.contains("manifest-list build"),
        "expected the fail-fast skip_push/comma-platform error:\n{stderr}"
    );
}

#[test]
fn deploy_with_skip_push_and_skip_build_and_comma_platform_does_not_throw() {
    // With skip_build ALSO set, no build runs this call — nothing for skip_push to silently
    // violate, so this combination is a legitimate no-op and must NOT be refused.
    let (plan, _stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, skip_build: true, skip_push: true,
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    );
    assert!(
        plan.contains("PLAN (dry run"),
        "skip_build + skip_push + a comma platform must not be refused:\n{plan}"
    );
}

// ---------------------------------------------------------------------------
// cfg.build_host (roadmap 1.1 step 3a: remote builder over SSH)
// ---------------------------------------------------------------------------

#[test]
fn docker_build_with_build_host_syncs_context_then_builds_remotely() {
    // The full sync-then-build sequence should show up as 4 planned actions, in order: (1) an
    // ssh rm-rf+mkdir to clean/prepare the remote context dir, (2) a LOCAL tar+base64 archive of
    // the context, (3) an ssh-stdin base64-decode+extract on build_host, (4) the actual build
    // command itself run via ssh on build_host, cd'd into the synced dir.
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ context: ".", build_host: "builder1" });
    "#,
    );
    let remote_dir = "/tmp/.nrg-build-ctx-ghcr.io_org_app_v1";

    let prep = plan
        .lines()
        .find(|l| l.contains("rm -rf") && l.contains(remote_dir))
        .unwrap_or_else(|| panic!("no remote-context prep line in plan:\n{plan}"));
    assert!(prep.starts_with("  ssh") && prep.contains("builder1"), "prep must run via ssh on build_host: {prep}");
    assert!(prep.contains("mkdir -p"), "prep must recreate the dir after wiping it: {prep}");

    let archive = plan
        .lines()
        .find(|l| l.contains("tar -czf") && l.contains("base64"))
        .unwrap_or_else(|| panic!("no local archive line in plan:\n{plan}"));
    assert!(archive.starts_with("  local"), "archiving the context must run LOCALLY, not on build_host: {archive}");
    assert!(
        archive.contains("--exclude-from=.dockerignore"),
        "must conditionally honor a context/.dockerignore via tar --exclude-from: {archive}"
    );

    let sync = plan
        .lines()
        .find(|l| l.contains("base64 -d") && l.contains("tar -xzf"))
        .unwrap_or_else(|| panic!("no remote extract line in plan:\n{plan}"));
    assert!(sync.starts_with("  ssh-stdin") && sync.contains("builder1"), "extract must run via ssh_exec_stdin on build_host: {sync}");
    assert!(sync.contains(remote_dir), "extract must target the synced remote dir: {sync}");

    let build = plan
        .lines()
        .find(|l| l.contains(remote_dir) && l.contains("docker build"))
        .unwrap_or_else(|| panic!("no remote build line in plan:\n{plan}"));
    assert!(build.starts_with("  ssh") && build.contains("builder1"), "the build itself must run via ssh on build_host: {build}");
    assert!(build.contains("cd '") && build.contains("&&"), "must cd into the synced dir before building: {build}");
    assert!(build.contains("-t 'ghcr.io/org/app:v1'"), "must still pass the tag: {build}");
    assert!(build.trim_end().ends_with('.'), "build context arg must be '.' (relative to the cd'd remote dir): {build}");
}

#[test]
fn docker_build_without_build_host_makes_no_ssh_calls() {
    // Regression: an empty (default) build_host must be byte-for-byte the pre-3a local path —
    // no ssh/ssh-stdin lines of any kind should appear.
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ context: "." });
    "#,
    );
    assert!(
        !plan.lines().any(|l| l.starts_with("  ssh")),
        "no build_host set — nothing should run over ssh:\n{plan}"
    );
    let build_line = plan
        .lines()
        .find(|l| l.contains("docker build"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(build_line.starts_with("  local"), "the build must still run locally: {build_line}");
}

#[test]
fn deploy_with_build_host_pushes_from_build_host_not_locally() {
    // The image built via cfg.build_host only exists THERE — deploy()'s separate push step must
    // run on build_host too (roadmap 1.1 step 3a), not on this machine.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, build_host: "builder1",
        });
    "#,
    );
    let push = plan
        .lines()
        .find(|l| l.contains("push") && l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(push.starts_with("  ssh") && push.contains("builder1"), "push must run via ssh on build_host, not locally: {push}");
}

#[test]
fn deploy_without_build_host_still_pushes_locally() {
    // Regression: the default (empty) build_host must leave the pre-3a local push untouched.
    let plan = plan_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{ container_port: 3000 });
    "#,
    );
    let push = plan
        .lines()
        .find(|l| l.contains("push") && l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(push.starts_with("  local"), "push must still run locally with no build_host set: {push}");
}

#[test]
fn deploy_with_build_host_and_multi_platform_does_not_double_push() {
    // build_host and a comma-separated platform compose: the SAME buildx --push command just
    // runs on build_host instead of locally — deploy()'s separate push-skip logic (already
    // covered for the no-build_host case above) must still apply, no duplicate push anywhere.
    let (plan, stderr) = plan_and_stderr_for(
        r#"
        import "lib/deploy" as deploy;
        deploy::deploy(["web1"], "ghcr.io/org/app:v1", "app", #{
            container_port: 3000, build_host: "builder1",
            platform: "linux/amd64,linux/arm64",
        });
    "#,
    );
    assert!(
        stderr.contains("buildx already pushed the manifest list during build"),
        "the existing multi-platform push-skip message must still fire:\n{stderr}"
    );
    let push_lines: Vec<&str> = plan.lines().filter(|l| l.contains(" push ")).collect();
    assert!(
        push_lines.is_empty(),
        "no separate docker push action should be planned at all:\n{plan}"
    );
    let build = plan
        .lines()
        .find(|l| l.contains("buildx build") && l.contains("--push"))
        .unwrap_or_else(|| panic!("no remote buildx --push line in plan:\n{plan}"));
    assert!(build.starts_with("  ssh") && build.contains("builder1"), "the multi-platform build+push must run on build_host: {build}");
}

#[test]
fn docker_push_two_arg_with_host_runs_via_ssh() {
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_push("builder1", "ghcr.io/org/app:v1");
    "#,
    );
    let push = plan
        .lines()
        .find(|l| l.contains("push") && l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(push.starts_with("  ssh") && push.contains("builder1"), "docker_push(host, tag) must run via ssh: {push}");
}

#[test]
fn docker_push_two_arg_with_empty_host_runs_locally() {
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_push("", "ghcr.io/org/app:v1");
    "#,
    );
    let push = plan
        .lines()
        .find(|l| l.contains("push") && l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(push.starts_with("  local"), "docker_push(\"\", tag) must still run locally: {push}");
}

#[test]
fn dockerignore_conditional_exclude_shell_snippet_is_correct() {
    // The plan can only ever show the LITERAL shell conditional (dry-run never actually invokes
    // `sh`, see the tests above) — this proves the conditional itself is correct shell, run for
    // real, against both a context WITH and WITHOUT a .dockerignore. No real build_host is
    // available in this environment (same documented limit as check_arch_mismatch's own live-only
    // coverage, docs/robustness-review.md R8), so this exercises just the tar/--exclude-from
    // fragment in isolation via a plain `sh -c`, not the full sync_build_context path.
    use std::process::Command as StdCommand;

    let with_ignore = tempfile::tempdir().unwrap();
    fs::write(with_ignore.path().join(".dockerignore"), "ignored.txt\n").unwrap();
    fs::write(with_ignore.path().join("ignored.txt"), "nope").unwrap();
    fs::write(with_ignore.path().join("kept.txt"), "yes").unwrap();

    let cmd = format!(
        "cd {} && tar -czf - $([ -f .dockerignore ] && echo --exclude-from=.dockerignore) . | tar -tzf - | sort",
        with_ignore.path().display()
    );
    let out = StdCommand::new("sh").arg("-c").arg(&cmd).output().unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        !listing.contains("ignored.txt") && listing.contains("kept.txt"),
        "a present .dockerignore must exclude the listed file:\n{listing}"
    );

    let without_ignore = tempfile::tempdir().unwrap();
    fs::write(without_ignore.path().join("kept.txt"), "yes").unwrap();
    let cmd2 = format!(
        "cd {} && tar -czf - $([ -f .dockerignore ] && echo --exclude-from=.dockerignore) . | tar -tzf - | sort",
        without_ignore.path().display()
    );
    let out2 = StdCommand::new("sh").arg("-c").arg(&cmd2).output().unwrap();
    assert!(out2.status.success(), "tar must not error when .dockerignore is absent: {:?}", out2);
    let listing2 = String::from_utf8_lossy(&out2.stdout);
    assert!(listing2.contains("kept.txt"), "must still archive files when .dockerignore is absent:\n{listing2}");
}

#[test]
fn sync_build_context_tar_failure_is_not_masked_by_base64() {
    // Opus review: piping tar's stdout straight into `base64` masked tar's own exit code — plain
    // `sh -c` has no `pipefail`, so a pipeline's exit status is the LAST stage's, and `base64`
    // essentially never fails; a tar that died partway through still emitted a valid archive of
    // whatever it read before dying, so the whole command looked like a success. sync_build_context
    // (lib/docker.rhai) now routes tar's output through a local temp file instead, capturing tar's
    // OWN exit status (`rc`) before base64-encoding (gated on `rc == 0`) or cleaning up runs.
    // Exercises the real shell fragment directly (no nrg involved), forcing tar to fail by writing
    // its output into a nonexistent subdirectory — fails regardless of privilege level, unlike a
    // permission-denied file (this suite runs as root in CI, which bypasses those checks).
    use std::process::Command as StdCommand;

    let ctx = tempfile::tempdir().unwrap();
    fs::write(ctx.path().join("kept.txt"), "yes").unwrap();
    let bogus_tar_path = ctx.path().join("nonexistent-subdir").join("out.tgz");

    let cmd = format!(
        "cd {} && tar -czf {} . ; rc=$?; if [ $rc -eq 0 ]; then base64 < {}; fi; rm -f {}; exit $rc",
        ctx.path().display(),
        bogus_tar_path.display(),
        bogus_tar_path.display(),
        bogus_tar_path.display(),
    );
    let out = StdCommand::new("sh").arg("-c").arg(&cmd).output().unwrap();
    assert!(
        !out.status.success(),
        "tar writing to a nonexistent directory must fail the WHOLE command, not be silently masked: {:?}",
        out
    );
    assert!(
        out.stdout.is_empty(),
        "must not emit any base64 output at all when tar itself failed: {:?}",
        out.stdout
    );
}
