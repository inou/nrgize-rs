//! `nrg doctor` — sanity checks: the orchestration file compiles, the external tools the
//! stdlib shells out to are on `PATH`, and (with `--host`, or auto-discovered from state)
//! that each deploy target is reachable over SSH and has a container runtime installed.

use crate::cli::exec::resolve_file;
use crate::engine::eval;
use crate::engine::runner::{CommandRunner, RealRunner};
use crate::engine::state::{self, StateStore};
use clap::Args;
use crossterm::style::Stylize;

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
            for check in probe_hosts(&runner, &hosts) {
                print_host_check(&check);
                if !check.reachable || check.runtime.is_none() {
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

/// The result of preflighting one host: SSH reachability, and — only if reachable — which
/// container runtime binary (if any) is on its `PATH`.
struct HostCheck {
    host: String,
    reachable: bool,
    runtime: Option<String>,
}

/// Preflight every host IN PARALLEL (like `ssh_exec_all`'s fan-out) — a `doctor` run
/// shouldn't take `hosts.len() * ConnectTimeout` seconds serially. Results are returned in the
/// SAME order as `hosts` (not completion order), so the printed report stays deterministic.
fn probe_hosts(runner: &dyn CommandRunner, hosts: &[String]) -> Vec<HostCheck> {
    std::thread::scope(|scope| {
        hosts
            .iter()
            .map(|h| scope.spawn(move || probe_host(runner, h)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| HostCheck {
                    host: "<unknown>".to_string(),
                    reachable: false,
                    runtime: None,
                })
            })
            .collect()
    })
}

/// Probe one host: SSH reachability first (a cheap `true`), and only if that succeeds, which
/// container runtime binary is on its `PATH` — no point spending a second round-trip checking
/// for docker on a host we can't even reach.
fn probe_host(runner: &dyn CommandRunner, host: &str) -> HostCheck {
    let ssh = runner.run_ssh(host, "true");
    if ssh.exit_code != 0 {
        return HostCheck { host: host.to_string(), reachable: false, runtime: None };
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
    HostCheck { host: host.to_string(), reachable: true, runtime }
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
        let c = probe_host(&runner, "web1");
        assert!(!c.reachable);
        assert_eq!(c.runtime, None);
        assert_eq!(runner.calls().len(), 1, "must not bother checking for a runtime on an unreachable host");
    }

    #[test]
    fn probe_host_reports_reachable_with_runtime_found() {
        let mut runner = FakeRunner::new();
        runner.default = RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
        let c = probe_host(&runner, "web1");
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
        let c = probe_host(&runner, "web1");
        assert!(c.reachable);
        assert_eq!(c.runtime.as_deref(), Some("/usr/bin/docker"));
    }

    #[test]
    fn probe_host_reachable_but_no_runtime_found() {
        let runner = FakeRunner::new();
        runner.fail_cmd("web1", "command -v", 1, "");
        let c = probe_host(&runner, "web1");
        assert!(c.reachable);
        assert_eq!(c.runtime, None);
    }

    #[test]
    fn probe_hosts_preserves_input_order_regardless_of_completion_order() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 255, "down");
        let hosts = vec!["web1".to_string(), "web2".to_string(), "web3".to_string()];
        let results = probe_hosts(&runner, &hosts);
        let got: Vec<&str> = results.iter().map(|c| c.host.as_str()).collect();
        assert_eq!(got, vec!["web1", "web2", "web3"]);
        assert!(!results[0].reachable);
        assert!(results[1].reachable);
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
