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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};

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

// ---------------------------------------------------------------------------
// Running the REAL build-context-sync commands, inside a sandbox
// ---------------------------------------------------------------------------
//
// The sync tests below lift each command VERBATIM from the dry-run plan and then execute it, so
// what they cover is the shell a live `cfg.build_host` build would really run, not a hand-copied
// approximation that can drift from lib/docker.rhai. A live run puts the context in
// `/tmp/.nrg-build-ctx-<tag>` — a fixed, shared, predictable path — and creates, extracts into
// and `rm -rf`s it. A test must never touch that: it would clobber a concurrent real build and
// leave droppings on the developer's machine. So every such path (the dir AND its `.local.tgz`
// sibling, which share the same prefix) is rewritten into a per-test `tempfile::tempdir()` first,
// and `redirect_into_sandbox` refuses to hand back a command with any of them left in it.

/// The command text of a dry-run plan line, which is printed as `  {kind:<7} {host:<22} {detail}`.
fn cmd_of(line: &str) -> &str {
    let after_kind = line
        .trim_start()
        .split_once(char::is_whitespace)
        .unwrap_or_else(|| panic!("plan line has no kind column: {line}"))
        .1;
    after_kind
        .trim_start()
        .split_once(char::is_whitespace)
        .unwrap_or_else(|| panic!("plan line has no host column: {line}"))
        .1
        .trim_start()
}

/// Rewrite every `/tmp/.nrg-build-ctx-…` path in a plan-lifted command into `sandbox`, and assert
/// none survived — a leak here would mean the test is about to write to the real shared path.
fn redirect_into_sandbox(cmd: &str, sandbox: &Path) -> String {
    let redirected = cmd.replace(
        "/tmp/.nrg-build-ctx-",
        &format!("{}/.nrg-build-ctx-", sandbox.display()),
    );
    assert!(
        !redirected.contains("/tmp/.nrg-build-ctx"),
        "a real /tmp build-context path leaked into a command this test executes:\n{redirected}"
    );
    redirected.replace(
        "/tmp/.nrg-archive-",
        &format!("{}/.nrg-archive-", sandbox.display()),
    )
}

/// The four commands `docker_build` plans for a `cfg.build_host` build, sandboxed and ready to run.
struct SyncPlan {
    prep: String,
    archive: String,
    extract: String,
    build: String,
    /// Where the (redirected) synced context lands.
    dir: PathBuf,
}

fn sync_plan(sandbox: &Path) -> SyncPlan {
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ context: ".", build_host: "builder1" });
    "#,
    );
    let pick = |needle: &str| {
        let line = plan
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no plan line containing {needle:?}:\n{plan}"));
        redirect_into_sandbox(cmd_of(line), sandbox)
    };
    SyncPlan {
        prep: format!(
            "mkdir -m 700 -p {}",
            sandbox.join(".nrg-build-ctx-dry-run").display()
        ),
        archive: pick("tar -czf"),
        extract: pick("tar -xzf"),
        build: pick("build -t"),
        dir: sandbox.join(".nrg-build-ctx-dry-run"),
    }
}

fn run_sh(cmd: &str, cwd: &Path) -> Output {
    StdCommand::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn run_sh_stdin(cmd: &str, cwd: &Path, stdin: &[u8]) -> Output {
    let mut child = StdCommand::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// Run the real prep + archive + extract commands for `ctx`, leaving the synced context in
/// `p.dir`. Callers then inspect the extracted TREE (rather than a `tar -t` listing), which keeps
/// them independent of how a given tar quotes odd filenames when it prints them.
fn sync_top_level(p: &SyncPlan, ctx: &Path, sandbox: &Path) {
    let prep = run_sh(&p.prep, sandbox);
    assert!(
        prep.status.success(),
        "preparing the context dir failed: {prep:?}"
    );
    let archived = run_sh(&p.archive, ctx);
    assert!(
        archived.status.success(),
        "archiving the context failed: {archived:?}"
    );
    let extracted = run_sh_stdin(&p.extract, sandbox, &archived.stdout);
    assert!(
        extracted.status.success(),
        "extracting the context failed: {extracted:?}"
    );
}

fn top_level_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
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
    assert!(
        line.contains(" build -t "),
        "expected a plain build: {line}"
    );
    assert!(
        !line.contains("buildx"),
        "must not use buildx when no platform is set: {line}"
    );
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
    assert!(
        line.contains("buildx build --platform 'linux/amd64' --load"),
        "got: {line}"
    );
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
    let line = plan
        .lines()
        .find(|l| l.contains("buildx"))
        .unwrap_or_else(|| panic!("got:\n{plan}"));
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
    assert!(
        !line.contains("--load"),
        "a multi-platform build must not use --load: {line}"
    );
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
        !plan
            .lines()
            .any(|l| l.contains(" push ") && l.contains("'ghcr.io/org/app:v1'")),
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
    // command itself run via ssh on build_host, cd'd into the synced dir — which also deletes
    // the synced context again as its last step. Still 4 actions: the cleanup rides along on the
    // build command rather than adding a fifth.
    let plan = plan_for(
        r#"
        import "lib/docker" as docker;
        docker::docker_build("ghcr.io/org/app:v1", #{ context: ".", build_host: "builder1" });
    "#,
    );
    let remote_dir = "/tmp/.nrg-build-ctx-dry-run";
    assert!(
        plan.contains("5 action(s)"),
        "the sync must include exclusive allocation plus four build actions:\n{plan}"
    );

    let prep = plan
        .lines()
        .find(|l| l.contains("mktemp -d"))
        .unwrap_or_else(|| panic!("no remote-context prep line in plan:\n{plan}"));
    assert!(
        prep.starts_with("  ssh") && prep.contains("builder1"),
        "prep must run via ssh on build_host: {prep}"
    );
    assert!(
        prep.contains("/tmp/.nrg-build-ctx-XXXXXXXXXX"),
        "prep must target the remote context dir: {prep}"
    );
    assert!(
        prep.contains("mktemp -d"),
        "prep must recreate the dir after wiping it, and create it PRIVATE in one step — `/tmp` on \
         a build host is shared with every other local user there: {prep}"
    );

    let archive = plan
        .lines()
        .find(|l| l.contains("tar -czf") && l.contains("base64"))
        .unwrap_or_else(|| panic!("no local archive line in plan:\n{plan}"));
    assert!(
        archive.starts_with("  local"),
        "archiving the context must run LOCALLY, not on build_host: {archive}"
    );
    assert!(
        archive.contains("--exclude-from=.dockerignore"),
        "must conditionally honor a context/.dockerignore via tar --exclude-from: {archive}"
    );
    assert!(
        archive.contains("umask 077"),
        "the local temp archive holds the same context bytes and lands in /tmp too — it must not \
         be created world-readable: {archive}"
    );
    for cred in [".nrg-key", ".nrg-key.pub", ".energize", ".env"] {
        assert!(
            archive.contains(cred),
            "the archive command must name {cred} as an entry to skip — nrg's own credentials are \
             never shipped to a build host: {archive}"
        );
    }
    assert!(
        !archive.contains("--exclude="),
        "the skip must be a list of tar OPERANDS, not `tar --exclude=`: an exclude pattern is \
         unanchored in bsdtar, so it would also drop a nested `sub/.env` that IS a build input: \
         {archive}"
    );

    let sync = plan
        .lines()
        .find(|l| l.contains("base64 -d") && l.contains("tar -xzf"))
        .unwrap_or_else(|| panic!("no remote extract line in plan:\n{plan}"));
    assert!(
        sync.starts_with("  ssh-stdin") && sync.contains("builder1"),
        "extract must run via ssh_exec_stdin on build_host: {sync}"
    );
    assert!(
        sync.contains(remote_dir),
        "extract must target the synced remote dir: {sync}"
    );

    let build = plan
        .lines()
        .find(|l| l.contains(remote_dir) && l.contains("docker build"))
        .unwrap_or_else(|| panic!("no remote build line in plan:\n{plan}"));
    assert!(
        build.starts_with("  ssh") && build.contains("builder1"),
        "the build itself must run via ssh on build_host: {build}"
    );
    assert!(
        build.contains("cd '") && build.contains("&&"),
        "must cd into the synced dir before building: {build}"
    );
    assert!(
        build.contains("-t 'ghcr.io/org/app:v1'"),
        "must still pass the tag: {build}"
    );
    assert!(
        build.contains(&format!(" . ; rc=$?; rm -rf '{remote_dir}'; exit $rc")),
        "build context arg must still be '.' (relative to the cd'd remote dir), and the synced \
         context must be deleted afterwards without masking the build's own exit code: {build}"
    );

    assert!(
        !plan.contains("chmod"),
        "nothing here may chmod a path under a world-writable /tmp: `chmod` follows symlinks, so a \
         local user on the build host who wins the race between `rm -rf` and `mkdir` could aim it \
         at a directory of their choosing:\n{plan}"
    );
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
    assert!(
        build_line.starts_with("  local"),
        "the build must still run locally: {build_line}"
    );
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
    assert!(
        push.starts_with("  ssh") && push.contains("builder1"),
        "push must run via ssh on build_host, not locally: {push}"
    );
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
    assert!(
        push.starts_with("  local"),
        "push must still run locally with no build_host set: {push}"
    );
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
    assert!(
        build.starts_with("  ssh") && build.contains("builder1"),
        "the multi-platform build+push must run on build_host: {build}"
    );
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
    assert!(
        push.starts_with("  ssh") && push.contains("builder1"),
        "docker_push(host, tag) must run via ssh: {push}"
    );
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
    assert!(
        push.starts_with("  local"),
        "docker_push(\"\", tag) must still run locally: {push}"
    );
}

#[test]
fn dockerignore_conditional_exclude_shell_snippet_is_correct() {
    // The plan can only ever show the LITERAL shell conditional (dry-run never actually invokes
    // `sh`, see the tests above) — this proves the conditional itself is correct shell, by running
    // the REAL archive command from the plan against both a context WITH and WITHOUT a
    // .dockerignore. No real build_host is available in this environment (same documented limit as
    // check_arch_mismatch's own live-only coverage, docs/robustness-review.md R8), so only the
    // local half runs here; its `/tmp` paths are redirected into a sandbox first.
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let with_ignore = tempfile::tempdir().unwrap();
    fs::write(with_ignore.path().join(".dockerignore"), "ignored.txt\n").unwrap();
    fs::write(with_ignore.path().join("ignored.txt"), "nope").unwrap();
    fs::write(with_ignore.path().join("kept.txt"), "yes").unwrap();
    sync_top_level(&p, with_ignore.path(), sandbox.path());
    let names = top_level_names(&p.dir);
    assert!(
        !names.iter().any(|n| n == "ignored.txt") && names.iter().any(|n| n == "kept.txt"),
        "a present .dockerignore must exclude the listed file: {names:?}"
    );

    let without_ignore = tempfile::tempdir().unwrap();
    fs::write(without_ignore.path().join("kept.txt"), "yes").unwrap();
    sync_top_level(&p, without_ignore.path(), sandbox.path());
    let names = top_level_names(&p.dir);
    assert!(
        names.iter().any(|n| n == "kept.txt"),
        "must still archive files when .dockerignore is absent: {names:?}"
    );
}

#[test]
fn sync_build_context_tar_failure_is_not_masked_by_base64() {
    // Opus review: piping tar's stdout straight into `base64` masked tar's own exit code — plain
    // `sh -c` has no `pipefail`, so a pipeline's exit status is the LAST stage's, and `base64`
    // essentially never fails; a tar that died partway through still emitted a valid archive of
    // whatever it read before dying, so the whole command looked like a success. sync_build_context
    // (lib/docker.rhai) now routes tar's output through a local temp file instead, capturing tar's
    // OWN exit status (`rc`) before base64-encoding (gated on `rc == 0`) or cleaning up runs.
    // Runs the real archive command, forcing tar to fail by pointing the sandbox — and so the temp
    // archive's path — at a directory that doesn't exist; that fails regardless of privilege level,
    // unlike a permission-denied file (this suite runs as root in CI, which bypasses those checks).
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(&sandbox.path().join("nonexistent-subdir"));

    let ctx = tempfile::tempdir().unwrap();
    fs::write(ctx.path().join("kept.txt"), "yes").unwrap();

    let out = run_sh(&p.archive, ctx.path());
    assert!(
        !out.status.success(),
        "tar writing to a nonexistent directory must fail the WHOLE command, not be silently masked: {out:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "must not emit any base64 output at all when tar itself failed: {:?}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// What the sync ships, and what it leaves behind (F9)
// ---------------------------------------------------------------------------

#[test]
fn synced_context_never_ships_nrg_credentials() {
    // A bare `tar -czf … .` archived EVERYTHING at the context root — including `.nrg-key`, the
    // unpassphrased age identity that decrypts every `ENC[...]` secret, plus `.nrg-key.pub`,
    // `.energize/` (deploy state, which can hold secret plaintext) and `.env`. With
    // `cfg.build_host` set that shipped all of them over SSH into `/tmp` on a THIRD machine, for a
    // build that has no use for any of it.
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let ctx = tempfile::tempdir().unwrap();
    fs::write(ctx.path().join(".nrg-key"), "AGE-SECRET-KEY-1EXAMPLE").unwrap();
    fs::write(ctx.path().join(".nrg-key.pub"), "age1example").unwrap();
    fs::write(ctx.path().join(".env"), "DATABASE_URL=postgres://u:p@h/db").unwrap();
    fs::create_dir_all(ctx.path().join(".energize")).unwrap();
    fs::write(ctx.path().join(".energize/state.json"), "{}").unwrap();
    fs::write(ctx.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    fs::write(ctx.path().join("app.txt"), "src").unwrap();
    // Same names NESTED are ordinary build inputs, not nrg's own credentials — only the context
    // ROOT is skipped, which is why the skip is a list of tar operands and not a `tar --exclude=`.
    fs::create_dir_all(ctx.path().join("config")).unwrap();
    fs::write(ctx.path().join("config/.env"), "APP_ENV=prod").unwrap();
    fs::write(ctx.path().join("config/.nrg-key"), "not nrg's").unwrap();

    sync_top_level(&p, ctx.path(), sandbox.path());
    let names = top_level_names(&p.dir);
    for cred in [".nrg-key", ".nrg-key.pub", ".energize", ".env"] {
        assert!(
            !names.iter().any(|n| n == cred),
            "{cred} must never reach a build host, but the synced context has: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == "Dockerfile"),
        "the build must still get its Dockerfile: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "app.txt"),
        "the build must still get the source: {names:?}"
    );
    assert!(
        p.dir.join("config/.env").exists() && p.dir.join("config/.nrg-key").exists(),
        "only the context ROOT is skipped — a nested config/.env the app really builds against \
         must still be synced"
    );
}

#[test]
fn synced_context_keeps_hostile_top_level_names_intact() {
    // Top-level entries are enumerated with shell GLOBS into `"$@"` (never `$(ls)`, whose output
    // can't be parsed for names with spaces or newlines) and handed to tar as `./name`, so a name
    // containing a space, a newline, or a glob character survives, and one starting with `-` or
    // `@` stays a path instead of being read by tar as an option or an archive reference.
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let ctx = tempfile::tempdir().unwrap();
    let hostile = [
        "file with space.txt",
        "weird\nname.txt",
        "star*name",
        "[b]racket",
        "-leading-dash",
        "@atsign",
        ".hidden",
        "..dotdot",
    ];
    for name in hostile {
        fs::write(ctx.path().join(name), "x").unwrap();
    }

    sync_top_level(&p, ctx.path(), sandbox.path());
    let names = top_level_names(&p.dir);
    for name in hostile {
        assert!(
            names.iter().any(|n| n == name),
            "{name:?} must be archived verbatim, not split/re-globbed/eaten as an option: {names:?}"
        );
    }
}

#[test]
fn synced_context_with_nothing_but_credentials_fails_locally() {
    // Skipping the four credential names can empty a context out entirely. That must fail HERE,
    // with a message that says why, rather than syncing an empty directory and surfacing as a
    // confusing "no Dockerfile" failure on the build host.
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let creds_only = tempfile::tempdir().unwrap();
    fs::write(
        creds_only.path().join(".nrg-key"),
        "AGE-SECRET-KEY-1EXAMPLE",
    )
    .unwrap();
    fs::write(creds_only.path().join(".env"), "A=b").unwrap();
    fs::create_dir_all(creds_only.path().join(".energize")).unwrap();

    let empty = tempfile::tempdir().unwrap();

    for ctx in [creds_only.path(), empty.path()] {
        let out = run_sh(&p.archive, ctx);
        assert!(
            !out.status.success(),
            "a context with nothing to sync must fail: {out:?}"
        );
        assert!(
            out.stdout.is_empty(),
            "must not emit an archive for an empty context: {out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("never synced to a remote builder"),
            "the failure must explain itself: {out:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn synced_context_dir_is_private_and_extraction_does_not_widen_it() {
    // `mkdir -m 700` creates the dir private in ONE step — no window in which a shared `/tmp` on
    // the build host exposes the project's source to every other local user there, and no `chmod`
    // afterwards (a `chmod` follows symlinks, so a local user who wins the race between `rm -rf`
    // and `mkdir` could aim it at a directory of their choosing).
    //
    // Nothing may undo that mode either. A bare `tar -czf … .` archives a `./` member carrying
    // the LOCAL context dir's own mode, and extracting it restores that mode onto the destination
    // — a perfectly ordinary 0755 context would widen the 0700 dir straight back out. Enumerated
    // operands archive no `./` member at all, so there is nothing left to restore.
    use std::os::unix::fs::PermissionsExt;

    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let ctx = tempfile::tempdir().unwrap();
    fs::set_permissions(ctx.path(), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(ctx.path().join("app.txt"), "src").unwrap();

    let prep = run_sh(&p.prep, sandbox.path());
    assert!(
        prep.status.success(),
        "preparing the context dir failed: {prep:?}"
    );
    let mode = fs::metadata(&p.dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "the synced context dir must be created private, got {mode:o}"
    );

    let archived = run_sh(&p.archive, ctx.path());
    assert!(
        archived.status.success(),
        "archiving the context failed: {archived:?}"
    );
    let extracted = run_sh_stdin(&p.extract, sandbox.path(), &archived.stdout);
    assert!(
        extracted.status.success(),
        "extracting the context failed: {extracted:?}"
    );
    assert!(
        p.dir.join("app.txt").exists(),
        "the context must actually land in the synced dir"
    );
    let mode = fs::metadata(&p.dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "extraction must not widen the synced context dir, got {mode:o}"
    );
}

#[cfg(unix)]
#[test]
fn local_temp_archive_is_not_world_readable() {
    // The local temp archive lands in `/tmp` as well and holds the same context bytes, so a
    // default-umask 0644 would leave every other user on the BUILD machine a readable copy for
    // the life of the sync. `umask 077` inside the command makes it 0600.
    //
    // The command deletes the archive itself, so the mode is captured mid-run by an `rm` shim
    // early on PATH that hard-links the file (same inode, same mode) before handing off to the
    // real /bin/rm. The ambient umask is forced to a permissive 022 so this measures the
    // command's OWN umask rather than whatever the test runner inherited.
    use std::os::unix::fs::PermissionsExt;

    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let bin = sandbox.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let kept = sandbox.path().join("kept-archive");
    fs::write(
        bin.join("rm"),
        "#!/bin/sh\nfor a in \"$@\"; do :; done\n[ -f \"$a\" ] && ln \"$a\" \"$NRG_TEST_KEEP\"\nexec /bin/rm \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("rm"), fs::Permissions::from_mode(0o755)).unwrap();

    let ctx = tempfile::tempdir().unwrap();
    fs::write(ctx.path().join("app.txt"), "src").unwrap();

    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(format!("umask 022; {}", p.archive))
        .current_dir(ctx.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("NRG_TEST_KEEP", &kept)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "archiving the context failed: {out:?}"
    );

    let mode = fs::metadata(&kept).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the local temp archive must not be world-readable, got {mode:o}"
    );
}

#[test]
fn remote_build_command_deletes_the_synced_context_and_keeps_the_builds_own_result() {
    // The synced context is a full copy of the project's source sitting in a shared `/tmp` on the
    // build host; it used to stay there until the next sync to the same tag. Deleting it is the
    // last step of the SAME build command, so it runs whether the build passed or failed — and
    // must not swallow the build's exit code, stdout or stderr on the way out.
    //
    // Runs the real command from the plan against a `docker` stand-in on PATH (no container
    // runtime, and no build_host, exists in this environment) — the shell around the build is
    // what's under test.
    let sandbox = tempfile::tempdir().unwrap();
    let p = sync_plan(sandbox.path());

    let bin = sandbox.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("docker"),
        "#!/bin/sh\necho build-said-this\necho build-warned-this >&2\nexit ${NRG_TEST_BUILD_RC:-0}\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for rc in ["0", "7"] {
        fs::create_dir_all(&p.dir).unwrap();
        fs::write(p.dir.join("Dockerfile"), "FROM scratch\n").unwrap();

        let out = StdCommand::new("sh")
            .arg("-c")
            .arg(&p.build)
            .current_dir(sandbox.path())
            .env("PATH", &path)
            .env("NRG_TEST_BUILD_RC", rc)
            .output()
            .unwrap();

        assert_eq!(
            out.status.code(),
            Some(rc.parse().unwrap()),
            "the build's own exit code must survive the cleanup: {out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("build-said-this"),
            "the build's stdout must come through untouched: {out:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("build-warned-this"),
            "the build's stderr must come through untouched: {out:?}"
        );
        assert!(
            !p.dir.exists(),
            "the synced context must be deleted after the build (build rc={rc}), not left in /tmp"
        );
    }
}
