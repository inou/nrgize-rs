//! Integration: Apple's `container` tool (macOS 26+, Apple Silicon) as a LOCAL-only build/push
//! runtime — `rt::local_build_cmd()`/`rt::set_local_build_runtime()` (lib/runtime.rhai), wired
//! into `docker_build`'s local branch and `docker_push`'s local-machine overload
//! (lib/docker.rhai). Asserted against the dry-run plan, same convention as tests/multi_arch.rs.
//!
//! This environment never has Apple's tool installed (it's macOS-only), so every test here
//! drives the CHOICE via `rt::set_local_build_runtime("container")` explicitly rather than
//! relying on real auto-detection — the auto-detect path itself is covered by
//! `docker_build_local_default_without_any_runtime_call_still_uses_plain_docker_build` below,
//! which locks in that `local_build_cmd()`'s macOS/health probes safely fall through to the
//! existing docker-first default under `--dry-run` (see lib/runtime.rhai's own DRY-RUN CAVEAT
//! comment on `apple_container_healthy()`).

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
fn docker_build_local_default_without_any_runtime_call_still_uses_plain_docker_build() {
    // Locks in the DRY-RUN CAVEAT documented on apple_container_healthy(): local_exec always
    // synthesizes empty stdout under --dry-run, so the macOS/health probes never spuriously
    // "detect" Apple's tool here, and local_build_cmd() falls through to container_cmd()'s
    // existing default ("docker") — this environment has neither macOS nor Apple's tool, so this
    // also happens to be true live, but the point of this test is the DRY-RUN safety property.
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
    assert!(line.contains("docker build -t "), "expected plain docker build: {line}");
}

#[test]
fn docker_build_with_container_runtime_uses_plain_build_no_buildx() {
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{});
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(line.contains("container build -t "), "got: {line}");
    assert!(!line.contains("buildx"), "Apple's container tool has no buildx concept: {line}");
}

#[test]
fn docker_build_with_container_runtime_and_single_platform_uses_native_platform_flag() {
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{ platform: "linux/arm64" });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(
        line.contains("container build --platform 'linux/arm64' -t "),
        "Apple's container build takes --platform natively, no buildx wrapper: {line}"
    );
}

#[test]
fn docker_build_with_container_runtime_and_multi_platform_passes_the_raw_value_through() {
    // No buildx-equivalent multi-platform manifest-list build is confirmed for Apple's tool —
    // this is deliberately left to fail at the shell (same as the existing nerdctl+buildx
    // caveat), not guessed at or pre-flight-rejected. This test locks in exactly what gets
    // planned/sent to the shell, not that it succeeds.
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{ platform: "linux/amd64,linux/arm64" });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(
        line.contains("container build --platform 'linux/amd64,linux/arm64' -t "),
        "got: {line}"
    );
    assert!(!line.contains("buildx"), "got: {line}");
}

#[test]
fn docker_build_with_container_runtime_still_honors_build_args_and_dockerfile() {
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{
            dockerfile: "Dockerfile.prod", build_args: #{VERSION: "42"},
        });
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no build line in plan:\n{plan}"));
    assert!(line.contains("-f 'Dockerfile.prod'"), "got: {line}");
    assert!(line.contains("--build-arg 'VERSION=42'"), "got: {line}");
}

#[test]
fn docker_build_with_container_runtime_and_build_host_still_builds_remotely_with_docker() {
    // The core safety property: Apple's tool can NEVER apply to a build_host (a remote Linux
    // box), even when set_local_build_runtime("container") was called — build_host always goes
    // through rt::container_cmd() (default "docker" here), completely unaffected.
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{ context: ".", build_host: "builder1" });
    "#,
    );
    let build = plan
        .lines()
        .find(|l| l.contains(".nrg-build-ctx") && l.contains("build "))
        .unwrap_or_else(|| panic!("no remote build line in plan:\n{plan}"));
    assert!(build.starts_with("  ssh") && build.contains("builder1"), "{build}");
    assert!(build.contains("docker build"), "remote build must stay on docker, not Apple's container tool: {build}");
    assert!(!build.contains("container build"), "{build}");
}

#[test]
fn docker_push_local_with_container_runtime_uses_image_push_namespacing() {
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_push("ghcr.io/org/app:v1");
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(
        line.contains("container image push 'ghcr.io/org/app:v1'"),
        "Apple's container tool namespaces image ops under `image`: {line}"
    );
}

#[test]
fn docker_push_remote_with_container_runtime_still_uses_docker() {
    // Same core safety property as the build_host build test: a remote host's push must never
    // switch to Apple's tool just because the LOCAL build runtime was set to "container".
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_push("builder1", "ghcr.io/org/app:v1");
    "#,
    );
    let line = plan
        .lines()
        .find(|l| l.contains("'ghcr.io/org/app:v1'"))
        .unwrap_or_else(|| panic!("no push line in plan:\n{plan}"));
    assert!(line.starts_with("  ssh") && line.contains("builder1"), "{line}");
    assert!(line.contains("docker push 'ghcr.io/org/app:v1'"), "got: {line}");
    assert!(!line.contains("container image push"), "got: {line}");
}

#[test]
fn set_local_build_runtime_accepts_docker_podman_nerdctl_and_container() {
    for runtime in ["docker", "podman", "nerdctl", "container"] {
        let (ok, out) = {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join(".energize")).unwrap();
            link_lib(dir.path());
            fs::write(
                dir.path().join("Energize.rhai"),
                format!(
                    r#"
                    import "lib/runtime" as rt;
                    rt::set_local_build_runtime("{runtime}");
                    "#
                ),
            )
            .unwrap();
            let out = Command::cargo_bin("nrg")
                .unwrap()
                .current_dir(dir.path())
                .arg("exec")
                .arg("--dry-run")
                .arg("Energize.rhai")
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        assert!(ok, "set_local_build_runtime(\"{runtime}\") must be accepted:\n{out}");
    }
}

#[test]
fn set_local_build_runtime_rejects_an_unknown_value() {
    let (_plan, stderr) = plan_and_stderr_error(
        r#"
        import "lib/runtime" as rt;
        rt::set_local_build_runtime("orbstack");
    "#,
    );
    assert!(
        stderr.contains("Unknown local build runtime") && stderr.contains("orbstack"),
        "expected a clear named-value error:\n{stderr}"
    );
}

fn plan_and_stderr_error(script: &str) -> (bool, String) {
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
        .failure()
        .get_output()
        .clone();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn local_build_runtime_choice_is_sticky_across_multiple_calls_within_one_run() {
    // local_build_cmd() caches its resolution in session state on first call — confirm a second
    // docker_build call in the SAME run doesn't need set_local_build_runtime called again and
    // still reflects the earlier explicit choice.
    let plan = plan_for(
        r#"
        import "lib/runtime" as rt;
        import "lib/docker" as docker;
        rt::set_local_build_runtime("container");
        docker::docker_build("ghcr.io/org/app:v1", #{});
        docker::docker_build("ghcr.io/org/app:v2", #{});
    "#,
    );
    let lines: Vec<&str> = plan
        .lines()
        .filter(|l| l.contains("container build -t"))
        .collect();
    assert_eq!(lines.len(), 2, "both builds must use the container runtime:\n{plan}");
}
