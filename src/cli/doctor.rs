//! `nrg doctor` — sanity checks: the orchestration file compiles, the external tools the
//! stdlib shells out to are on `PATH`, and (with `--host`, or auto-discovered from state)
//! that each deploy target is reachable over SSH, has a container runtime installed, and (roadmap
//! 2.5's remaining "registry auth" gap) can actually resolve the currently-deployed image of
//! every service recorded against it.
//!
//! Registry-auth scope: this only re-checks images ALREADY recorded in `.energize/state.json`
//! (each service's `<svc>.image`, set by `deploy()`) via `docker manifest inspect <image>` over
//! SSH — a lightweight registry-API round trip, not a full pull, that fails exactly the way a
//! real `deploy()`/`accessory_run` pull would if credentials are missing or wrong on that host.
//! There's no separate "which registry does this project use" concept anywhere in this codebase
//! to check against instead, so a fresh project with nothing deployed yet has nothing to check
//! here (same "skip, not a failure" shape as the runtime/reachability checks already have for a
//! project with no state). Only runs when the host's detected runtime looks like Docker — `docker
//! manifest inspect` is Docker-specific syntax, and this repo already has a precedent (`nrg
//! setup`'s Fable-review fix) for skipping Docker-only remote operations on a Podman/nerdctl host
//! rather than running a command that would fail for the wrong reason.

use crate::cli::exec::resolve_file;
use crate::engine::eval;
use crate::engine::runner::{CommandRunner, RealRunner};
use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use clap::Args;
use crossterm::style::Stylize;
use std::collections::HashMap;

#[derive(Args)]
pub struct DoctorArgs {
    /// Path to the `.rhai` file. Defaults to Energize.rhai.
    #[arg(long)]
    pub file: Option<String>,

    /// Host to preflight (SSH reachability + container runtime presence). Repeatable. Defaults
    /// to every host recorded in `.energize/state.json` (if any have been deployed before).
    #[arg(long = "host")]
    pub hosts: Vec<String>,
}

pub fn execute(args: &DoctorArgs) -> i32 {
    println!("\n{}\n", "Energize Doctor".bold());

    let mut all_ok = true;

    // Check 1: the orchestration file exists and compiles (parse-time validation — Rhai is
    // dynamically typed, so this catches syntax errors, not runtime/config errors).
    match resolve_file(&args.file, "").ok() {
        Some(path) => {
            check_pass(&format!("Orchestration file found: {}", path));
            match eval::list_functions(std::path::Path::new(&path)) {
                Ok(fns) => {
                    check_pass(&format!(
                        "{} compiles ({} function(s) defined)",
                        path,
                        fns.len()
                    ));
                }
                Err(e) => {
                    check_fail(&e);
                    all_ok = false;
                }
            }
        }
        None => {
            check_fail("No Energize.rhai found (run `nrg init`).");
            all_ok = false;
        }
    }

    // Check 2: external tools the stdlib relies on.
    println!("\n  {}", "Tools:".bold());
    let required = ["age", "ssh"];
    for tool in required {
        if tool_available(tool) {
            check_pass(&format!("{} found", tool));
        } else {
            check_fail(&format!("{} not found on PATH", tool));
            all_ok = false;
        }
    }
    // At least one tool from each of these groups is enough.
    check_group(&mut all_ok, "file transfer", &["rsync", "scp"]);
    check_group(&mut all_ok, "container runtime", &["docker", "podman"]);

    // Check 3: remote hosts, if any are known — the failures the LOCAL checks above can't
    // catch (an unreachable host, or a host missing a container runtime entirely). Most
    // first-deploy failures are remote, not local.
    match resolve_hosts(&args.hosts) {
        Ok(hosts) if !hosts.is_empty() => {
            println!("\n  {}", "Hosts:".bold());
            let runner = RealRunner;
            // Best-effort: a project with no state yet (or --host pointing at hosts state
            // doesn't know about) yields an empty map, not an error — the registry check simply
            // has nothing to check for that host, same as "nothing deployed" already means
            // "skip" for the whole Hosts section above. A genuinely corrupt state file is already
            // caught by `resolve_hosts` itself (see its own doc comment) whenever hosts were
            // auto-discovered; when hosts are explicit via `--host`, state is a bonus lookup here,
            // not a requirement, so a load failure just means no images to check, not a crash.
            let images = images_by_host(&hosts);
            for check in probe_hosts(&runner, &hosts, &images) {
                print_host_check(&check);
                if !check.reachable
                    || check.runtime.is_none()
                    || check.registry.iter().any(|r| !r.ok)
                {
                    all_ok = false;
                }
            }
        }
        Ok(_) => {} // no project root yet, or nothing deployed — nothing to check, not a failure
        Err(e) => {
            println!("\n  {}", "Hosts:".bold());
            check_fail(&e);
            all_ok = false;
        }
    }

    println!();

    if all_ok {
        println!("{} All checks passed!", "✓".green());
        0
    } else {
        println!("{} Some checks failed.", "⚠".yellow());
        1
    }
}

/// Pass if any tool in the group is available; otherwise fail and flip `all_ok`.
fn check_group(all_ok: &mut bool, label: &str, tools: &[&str]) {
    let found: Vec<&str> = tools.iter().copied().filter(|t| tool_available(t)).collect();
    if found.is_empty() {
        check_fail(&format!("{}: none of {} found on PATH", label, tools.join("/")));
        *all_ok = false;
    } else {
        check_pass(&format!("{}: {} found", label, found.join(", ")));
    }
}

/// Hosts to preflight: an explicit `--host` (repeatable) always wins; otherwise every host
/// recorded across every service in state (deduped, sorted). An empty result (no project root
/// yet, or no state file yet — a legitimately fresh project) means "skip the host checks", NOT
/// a failure. A state file that EXISTS but is corrupt or a future schema version is a genuine
/// `Err` that `doctor` must surface, same as `nrg status`/`nrg logs`/`nrg app exec` all treat it
/// as fatal — silently returning empty here would hide exactly the failure `doctor` exists to
/// catch.
fn resolve_hosts(explicit: &[String]) -> Result<Vec<String>, String> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    let Ok(root) = state::find_project_root() else {
        return Ok(Vec::new());
    };
    let store = StateStore::load(&root)?;
    Ok(hosts_from_store(&store))
}

/// Every host recorded across every service in `store`, deduped and sorted. Pure and
/// independently testable (unlike the `find_project_root`/`StateStore::load` calls above,
/// which touch real process CWD and disk) — this is the part of `resolve_hosts` actually worth
/// unit-testing directly.
fn hosts_from_store(store: &StateStore) -> Vec<String> {
    let mut hosts: Vec<String> = store.services().iter().flat_map(|s| store.hosts_for(s)).collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// For every host in `hosts`, every distinct, non-empty `<svc>.image` recorded against a service
/// deployed there (deduped, sorted) — the registry-auth check's input. Best-effort: no project
/// root or a state load failure both yield an empty map rather than an `Err`, since (unlike
/// `resolve_hosts`) this function's caller only ever runs after `hosts` is already known to be
/// non-empty, and a project with `--host` pointed at hosts state doesn't know about is a normal
/// case, not a corruption signal.
fn images_by_host(hosts: &[String]) -> HashMap<String, Vec<String>> {
    let Ok(root) = state::find_project_root() else { return HashMap::new() };
    let Ok(store) = StateStore::load(&root) else { return HashMap::new() };
    images_by_host_from_store(&store, hosts)
}

/// The pure part of `images_by_host` — independently unit-testable against an in-memory
/// `StateStore` (unlike `images_by_host` itself, which touches real process CWD and disk via
/// `find_project_root`/`StateStore::load`), same split `hosts_from_store` already established.
fn images_by_host_from_store(store: &StateStore, hosts: &[String]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for service in store.services() {
        let Some(image) = store.get(&format!("{service}.image")) else { continue };
        if image.trim().is_empty() {
            continue;
        }
        for host in store.hosts_for(&service) {
            if hosts.contains(&host) {
                map.entry(host).or_default().push(image.clone());
            }
        }
    }
    for images in map.values_mut() {
        images.sort();
        images.dedup();
    }
    map
}

/// The result of one image's registry-auth check on one host: whether `docker manifest inspect
/// <image>` succeeded, and (only on failure) the first line of its combined output as the reason.
struct RegistryCheck {
    image: String,
    ok: bool,
    reason: Option<String>,
}

/// The result of preflighting one host: SSH reachability, which container runtime binary (if
/// any) is on its `PATH` (only checked if reachable), and registry-auth results for every image
/// deployed there (only checked if reachable AND the runtime looks like Docker).
struct HostCheck {
    host: String,
    reachable: bool,
    runtime: Option<String>,
    registry: Vec<RegistryCheck>,
}

/// Preflight every host IN PARALLEL (like `ssh_exec_all`'s fan-out) — a `doctor` run
/// shouldn't take `hosts.len() * ConnectTimeout` seconds serially. Results are returned in the
/// SAME order as `hosts` (not completion order), so the printed report stays deterministic.
fn probe_hosts(
    runner: &dyn CommandRunner,
    hosts: &[String],
    images: &HashMap<String, Vec<String>>,
) -> Vec<HostCheck> {
    let empty: Vec<String> = Vec::new();
    std::thread::scope(|scope| {
        hosts
            .iter()
            .map(|h| {
                let host_images = images.get(h).unwrap_or(&empty);
                scope.spawn(move || probe_host(runner, h, host_images))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| HostCheck {
                    host: "<unknown>".to_string(),
                    reachable: false,
                    runtime: None,
                    registry: Vec::new(),
                })
            })
            .collect()
    })
}

/// Probe one host: SSH reachability first (a cheap `true`), then — only if that succeeds — which
/// container runtime binary is on its `PATH`, and finally — only if reachable AND that runtime
/// looks like Docker — a registry-auth check for every image in `images`. No point spending a
/// round-trip on a check whose precondition already failed.
fn probe_host(runner: &dyn CommandRunner, host: &str, images: &[String]) -> HostCheck {
    let ssh = runner.run_ssh(host, "true");
    if ssh.exit_code != 0 {
        return HostCheck { host: host.to_string(), reachable: false, runtime: None, registry: Vec::new() };
    }
    let rt = runner.run_ssh(host, "command -v docker || command -v podman || command -v nerdctl");
    // The FIRST non-empty line, not just the first line: a non-interactive login shell can
    // print unrelated banner/profile output (e.g. a sourced .zshenv) ahead of the real path, and
    // taking only rt.stdout.lines().next() would either report that noise as the "runtime" or,
    // if that noise line happens to be blank, miss the real path entirely (same pattern as
    // status.rs's probe_container output parsing).
    let runtime = if rt.exit_code == 0 {
        rt.stdout.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
    } else {
        None
    };

    let looks_like_docker = runtime.as_deref().is_some_and(|r| r.to_lowercase().contains("docker"));
    let registry = if looks_like_docker {
        images
            .iter()
            .map(|image| {
                let out = runner.run_ssh(host, &format!("docker manifest inspect {}", posix_quote(image)));
                if out.exit_code == 0 {
                    RegistryCheck { image: image.clone(), ok: true, reason: None }
                } else {
                    let combined = format!("{}\n{}", out.stdout, out.stderr);
                    RegistryCheck {
                        image: image.clone(),
                        ok: false,
                        reason: Some(first_reason(&combined, "docker manifest inspect failed")),
                    }
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    HostCheck { host: host.to_string(), reachable: true, runtime, registry }
}

/// The first non-blank line of `s`, or `fallback` — the same idiom `nrg setup`/`nrg remove`/`nrg
/// lock` each keep their own copy of (no shared helper module exists for this yet).
fn first_reason(s: &str, fallback: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(fallback).to_string()
}

fn print_host_check(check: &HostCheck) {
    if !check.reachable {
        check_fail(&format!("{}: not reachable via SSH", check.host));
        return;
    }
    check_pass(&format!("{}: reachable via SSH", check.host));
    match &check.runtime {
        Some(bin) => check_pass(&format!("{}: container runtime found ({bin})", check.host)),
        None => check_fail(&format!("{}: no docker/podman/nerdctl found on PATH", check.host)),
    }
    for reg in &check.registry {
        if reg.ok {
            check_pass(&format!("{}: registry auth OK for {}", check.host, reg.image));
        } else {
            check_fail(&format!(
                "{}: registry auth failed for {}: {}",
                check.host,
                reg.image,
                reg.reason.as_deref().unwrap_or("unknown reason")
            ));
        }
    }
}

/// Whether `tool` is resolvable on `PATH` (via `command -v`).
fn tool_available(tool: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", tool))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_pass(msg: &str) {
    println!("  {} {}", "✓".green(), msg);
}

fn check_fail(msg: &str) {
    println!("  {} {}", "✗".red(), msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::{FakeRunner, RawOutput};

    #[test]
    fn probe_host_reports_unreachable_and_skips_the_runtime_round_trip() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 255, "Connection refused");
        let c = probe_host(&runner, "web1", &[]);
        assert!(!c.reachable);
        assert_eq!(c.runtime, None);
        assert_eq!(runner.calls().len(), 1, "must not bother checking for a runtime on an unreachable host");
    }

    #[test]
    fn probe_host_reports_reachable_with_runtime_found() {
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1", &[]);
        assert!(c.reachable);
        assert_eq!(c.runtime.as_deref(), Some("/usr/bin/docker"));
    }

    #[test]
    fn probe_host_ignores_a_leading_blank_line_in_runtime_output() {
        // Regression: `.lines().next()` alone would take the blank line and report "no runtime
        // found" despite exit 0 — a non-interactive shell printing banner/profile output before
        // the real `command -v` result is a real scenario, not a hypothetical one.
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "\n/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1", &[]);
        assert!(c.reachable);
        assert_eq!(c.runtime.as_deref(), Some("/usr/bin/docker"));
    }

    #[test]
    fn probe_host_reachable_but_no_runtime_found() {
        let runner = FakeRunner::new();
        runner.fail_cmd("web1", "command -v", 1, "");
        let c = probe_host(&runner, "web1", &[]);
        assert!(c.reachable);
        assert_eq!(c.runtime, None);
    }

    #[test]
    fn probe_hosts_preserves_input_order_regardless_of_completion_order() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 255, "down");
        let hosts = vec!["web1".to_string(), "web2".to_string(), "web3".to_string()];
        let results = probe_hosts(&runner, &hosts, &HashMap::new());
        let got: Vec<&str> = results.iter().map(|c| c.host.as_str()).collect();
        assert_eq!(got, vec!["web1", "web2", "web3"]);
        assert!(!results[0].reachable);
        assert!(results[1].reachable);
    }

    #[test]
    fn probe_host_checks_registry_auth_for_every_image_when_runtime_is_docker() {
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1", &["ghcr.io/org/app:v1".to_string()]);
        assert!(c.reachable);
        assert_eq!(c.registry.len(), 1);
        assert!(c.registry[0].ok, "a default-passing FakeRunner call must be treated as auth success");
        assert_eq!(c.registry[0].image, "ghcr.io/org/app:v1");
        let invoked = runner.calls().join("\n");
        assert!(invoked.contains("manifest inspect"), "got: {invoked}");
    }

    #[test]
    fn probe_host_reports_a_real_registry_auth_failure_with_its_reason() {
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        runner.fail_cmd("web1", "manifest inspect", 1, "Error: unauthorized: authentication required");
        let c = probe_host(&runner, "web1", &["ghcr.io/org/app:v1".to_string()]);
        assert_eq!(c.registry.len(), 1);
        assert!(!c.registry[0].ok);
        assert_eq!(c.registry[0].reason.as_deref(), Some("Error: unauthorized: authentication required"));
    }

    #[test]
    fn probe_host_skips_registry_check_when_the_runtime_is_not_docker() {
        // Scope narrowing (matching `nrg setup`'s Fable-review fix): `docker manifest inspect` is
        // Docker-specific syntax, so a Podman-only host must not have it run against it at all —
        // running it anyway would fail for the WRONG reason ("docker: command not found").
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/podman\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1", &["ghcr.io/org/app:v1".to_string()]);
        assert!(c.registry.is_empty());
        let invoked = runner.calls().join("\n");
        assert!(!invoked.contains("manifest inspect"), "got: {invoked}");
    }

    #[test]
    fn probe_host_skips_registry_check_when_no_images_are_given() {
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1", &[]);
        assert!(c.registry.is_empty());
    }

    #[test]
    fn images_by_host_collects_only_images_for_the_requested_hosts() {
        let mut store = StateStore::ephemeral();
        store.set("app.version", "v1").unwrap();
        store.set("app.image", "ghcr.io/org/app:v1").unwrap();
        store.set("app.target.web1", "localhost:1").unwrap();
        store.set("app.target.web2", "localhost:2").unwrap();
        store.set("worker.version", "v2").unwrap();
        store.set("worker.image", "ghcr.io/org/worker:v2").unwrap();
        store.set("worker.target.web1", "localhost:3").unwrap();

        let images = images_by_host_from_store(&store, &["web1".to_string()]);
        assert_eq!(
            images.get("web1").cloned().unwrap_or_default(),
            vec!["ghcr.io/org/app:v1".to_string(), "ghcr.io/org/worker:v2".to_string()]
        );
        assert!(!images.contains_key("web2"), "web2 wasn't in the requested host list");
    }

    #[test]
    fn images_by_host_ignores_a_service_with_no_recorded_image() {
        let mut store = StateStore::ephemeral();
        store.set("app.version", "v1").unwrap();
        store.set("app.target.web1", "localhost:1").unwrap();
        let images = images_by_host_from_store(&store, &["web1".to_string()]);
        assert!(images.get("web1").is_none_or(|v| v.is_empty()));
    }

    #[test]
    fn resolve_hosts_explicit_wins_over_state() {
        assert_eq!(resolve_hosts(&["web9".to_string()]).unwrap(), vec!["web9".to_string()]);
    }

    #[test]
    fn hosts_from_store_collects_across_services_deduped_and_sorted() {
        let mut store = StateStore::ephemeral();
        // services() discovers a service from its `.version` key — target keys alone aren't enough.
        store.set("app.version", "v1").unwrap();
        store.set("worker.version", "v1").unwrap();
        store.set("app.target.web2", "localhost:1").unwrap();
        store.set("app.target.web1", "localhost:2").unwrap();
        store.set("worker.target.web1", "localhost:3").unwrap(); // same host as app's, different service
        store.set("worker.target.web3", "localhost:4").unwrap();
        assert_eq!(
            hosts_from_store(&store),
            vec!["web1".to_string(), "web2".to_string(), "web3".to_string()],
            "web1 is shared by two services and must appear exactly once"
        );
    }

    #[test]
    fn hosts_from_store_empty_when_nothing_deployed() {
        assert!(hosts_from_store(&StateStore::ephemeral()).is_empty());
    }
}
