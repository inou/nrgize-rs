//! `nrg status [service]` — show the deployed version/image and per-host state for a service
//! (or every service found in state), by reading `.energize/state.json` and, unless
//! `--offline`, probing each host's canonical container (`<service>-web`, per `lib/deploy.rhai`'s
//! naming convention) over SSH.

use crate::engine::runner::{CommandRunner, RealRunner};
use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use clap::Args;
use crossterm::style::Stylize;

#[derive(Args)]
pub struct StatusArgs {
    /// Service name (the `service` argument passed to `deploy()`). Shows every service found
    /// in state if omitted.
    pub service: Option<String>,

    /// Skip the live per-host container probe; show only what's recorded in state.json.
    #[arg(long)]
    pub offline: bool,
}

pub fn execute(args: &StatusArgs) -> i32 {
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let store = match StateStore::load(&root).map(|s| s.with_dest(crate::cli::destination())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let services = match &args.service {
        Some(s) => vec![s.clone()],
        None => store.services(),
    };
    if services.is_empty() {
        println!("No deployed services found in state (run a `deploy()` first).");
        return 0;
    }

    let runner: Option<RealRunner> = if args.offline { None } else { Some(RealRunner) };

    for (i, svc) in services.iter().enumerate() {
        let container_cmd = store
            .get(&format!("{svc}.runtime.cmd"))
            .or_else(|| store.get("nrg.runtime.cmd"))
            .unwrap_or_else(|| "docker".to_string());
        if i > 0 {
            println!();
        }
        print_service(
            &store,
            svc,
            &container_cmd,
            runner.as_ref().map(|r| r as &dyn CommandRunner),
        );
    }
    0
}

fn print_service(
    store: &StateStore,
    service: &str,
    container_cmd: &str,
    runner: Option<&dyn CommandRunner>,
) {
    println!("{}", service.to_string().bold());

    match store.get(&format!("{service}.version")) {
        Some(v) => println!("  version:      {v}"),
        None => println!("  version:      (none — no deploy recorded)"),
    }
    if let Some(image) = store.get(&format!("{service}.image")) {
        println!("  image:        {image}");
    }
    if let Some(deployed_at) = store.get(&format!("{service}.deployed_at")) {
        println!("  deployed_at:  {deployed_at}");
    }
    if let Some(prev) = store.get(&format!("{service}.prev")) {
        println!("  previous:     {prev}  (rollback target)");
    }

    let hosts = store.hosts_for(service);
    if hosts.is_empty() {
        println!("  hosts:        none recorded");
        return;
    }

    println!("  hosts:");
    let container = format!("{service}-web");
    for host in &hosts {
        let target = store
            .get(&format!("{service}.target.{host}"))
            .unwrap_or_else(|| "(unknown)".to_string());
        let label = match runner {
            None => "offline".to_string(),
            Some(r) => describe(probe_container(r, host, container_cmd, &container)),
        };
        println!("    {host:<28} target {target:<22} [{label}]");
    }
}

/// The live state of a probed container, distinguishing "not running" from "no such container"
/// from "couldn't even ask" — a down host, a container that was simply never deployed there, and
/// a cleanly stopped container are three different operator-facing facts, not one.
#[derive(Debug, PartialEq)]
enum ProbeResult {
    Running {
        healthy: Option<bool>,
    },
    Stopped,
    /// The host answered SSH, but no container by that name exists there (e.g. `docker inspect`
    /// returned "No such object") — reachable host, nothing deployed under that name.
    NotDeployed,
    /// SSH itself could not reach the host (connection refused/timed out/auth failure).
    Unreachable(String),
}

fn describe(probe: ProbeResult) -> String {
    match probe {
        ProbeResult::Running {
            healthy: Some(true),
        } => "running, healthy".to_string().green().to_string(),
        ProbeResult::Running {
            healthy: Some(false),
        } => "running, unhealthy".to_string().yellow().to_string(),
        ProbeResult::Running { healthy: None } => "running".to_string().green().to_string(),
        ProbeResult::Stopped => "stopped".to_string().red().to_string(),
        ProbeResult::NotDeployed => "not deployed here".to_string().yellow().to_string(),
        ProbeResult::Unreachable(msg) => format!("{}: {msg}", "unreachable".red()),
    }
}

/// `docker inspect` (or the configured runtime's binary) via a single templated call — one SSH
/// round-trip per host, not two — so the running+health facts can never disagree if the
/// container's state changes between two separate probes.
fn probe_container(
    runner: &dyn CommandRunner,
    host: &str,
    container_cmd: &str,
    name: &str,
) -> ProbeResult {
    let template =
        "{{.State.Running}}|{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}";
    let cmd = format!(
        "{container_cmd} inspect -f {} {}",
        posix_quote(template),
        posix_quote(name)
    );
    let out = runner.run_ssh(host, &cmd);
    parse_probe_output(out.exit_code, &out.stdout, &out.stderr)
}

/// SSH's own reserved exit code when IT fails to connect/authenticate — as opposed to a
/// successful connection whose REMOTE command (`docker inspect`) exits non-zero on its own
/// (e.g. "No such object"). Not airtight (a remote shell could theoretically also exit 255),
/// but it's the documented ssh(1) convention and the only signal we have without a second
/// round-trip.
const SSH_CONNECTION_FAILURE_EXIT: i64 = 255;

fn parse_probe_output(exit_code: i64, stdout: &str, stderr: &str) -> ProbeResult {
    if exit_code == SSH_CONNECTION_FAILURE_EXIT {
        let msg = stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("probe failed")
            .to_string();
        return ProbeResult::Unreachable(msg);
    }
    if exit_code != 0 {
        return ProbeResult::NotDeployed;
    }
    let out = stdout.trim();
    let mut parts = out.splitn(2, '|');
    let running = parts.next() == Some("true");
    if !running {
        return ProbeResult::Stopped;
    }
    match parts.next() {
        Some("healthy") => ProbeResult::Running {
            healthy: Some(true),
        },
        Some("none") | None => ProbeResult::Running { healthy: None },
        Some(_) => ProbeResult::Running {
            healthy: Some(false),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn parses_running_and_healthy() {
        assert_eq!(
            parse_probe_output(0, "true|healthy\n", ""),
            ProbeResult::Running {
                healthy: Some(true)
            }
        );
    }

    #[test]
    fn parses_running_without_healthcheck() {
        assert_eq!(
            parse_probe_output(0, "true|none\n", ""),
            ProbeResult::Running { healthy: None }
        );
    }

    #[test]
    fn parses_running_unhealthy() {
        assert_eq!(
            parse_probe_output(0, "true|unhealthy\n", ""),
            ProbeResult::Running {
                healthy: Some(false)
            }
        );
    }

    #[test]
    fn parses_stopped() {
        assert_eq!(
            parse_probe_output(0, "false|none\n", ""),
            ProbeResult::Stopped
        );
    }

    #[test]
    fn ssh_level_failure_exit_255_is_unreachable() {
        let r = parse_probe_output(
            255,
            "",
            "ssh: connect to host web1 port 22: Connection refused\n",
        );
        assert_eq!(
            r,
            ProbeResult::Unreachable(
                "ssh: connect to host web1 port 22: Connection refused".to_string()
            )
        );
    }

    #[test]
    fn reachable_host_missing_container_is_not_deployed_not_unreachable() {
        // `docker inspect` on a name that was never deployed there exits non-zero (1), but the
        // SSH connection itself succeeded — must not be conflated with a down host (255).
        let r = parse_probe_output(1, "", "Error: No such object: app-web\n");
        assert_eq!(r, ProbeResult::NotDeployed);
    }

    #[test]
    fn probe_container_builds_one_quoted_ssh_call() {
        let runner = FakeRunner::new(); // default canned output: exit 0, empty stdout/stderr
        let _ = probe_container(&runner, "web1", "docker", "app-web; rm -rf /");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        // The container name is single-quote escaped, so an embedded `;` can never break out
        // of the inspect argument (same shell-safety contract as the rest of the stdlib).
        assert!(
            calls[0].contains("'app-web; rm -rf /'"),
            "got: {}",
            calls[0]
        );
    }
}
