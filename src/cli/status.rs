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
    /// Exit nonzero unless every recorded host is running and not unhealthy.
    #[arg(long, conflicts_with = "offline")]
    pub check: bool,
    /// Emit structured service and host status.
    #[arg(long)]
    pub json: bool,
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
    let runner = RealRunner::default();
    let mut reports = Vec::new();
    let mut ok = !services.is_empty();
    for svc in services {
        let runtime = store
            .get(&format!("{svc}.runtime.cmd"))
            .or_else(|| store.get("nrg.runtime.cmd"))
            .unwrap_or_else(|| "docker".into());
        let mut hosts = Vec::new();
        for host in store.hosts_for(&svc) {
            let target = store.get(&format!("{svc}.target.{host}"));
            let probe = if args.offline {
                ProbeResult::Offline
            } else {
                probe_container(&runner, &host, &runtime, &format!("{svc}-web"))
            };
            ok &= matches!(
                probe,
                ProbeResult::Running {
                    healthy: None | Some(true)
                }
            );
            hosts.push(HostReport {
                host,
                target,
                probe,
            });
        }
        ok &= !hosts.is_empty();
        reports.push(ServiceReport {
            version: store.get(&format!("{svc}.version")),
            image: store.get(&format!("{svc}.image")),
            deployed_at: store.get(&format!("{svc}.deployed_at")),
            previous: store.get(&format!("{svc}.prev")),
            service: svc,
            hosts,
        });
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports).unwrap());
    } else if reports.is_empty() {
        println!("No deployed services found in state (run a `deploy()` first).");
    } else {
        for report in reports {
            println!("{}", safe(&report.service).bold());
            println!(
                "  version:      {}",
                safe(
                    report
                        .version
                        .as_deref()
                        .unwrap_or("(none — no deploy recorded)")
                )
            );
            for (label, value) in [
                ("image", report.image),
                ("deployed_at", report.deployed_at),
                ("previous", report.previous),
            ] {
                if let Some(v) = value {
                    println!("  {label}:      {}", safe(&v));
                }
            }
            if report.hosts.is_empty() {
                println!("  hosts:        none recorded");
            } else {
                println!("  hosts:");
            }
            for h in report.hosts {
                println!(
                    "    {:<28} target {:<22} [{}]",
                    safe(&h.host),
                    safe(h.target.as_deref().unwrap_or("(unknown)")),
                    describe(h.probe)
                );
            }
        }
    }
    if args.check && !ok {
        1
    } else {
        0
    }
}

fn safe(value: &str) -> String {
    super::audit::display_safe(value)
}

#[derive(serde::Serialize)]
struct ServiceReport {
    service: String,
    version: Option<String>,
    image: Option<String>,
    deployed_at: Option<String>,
    previous: Option<String>,
    hosts: Vec<HostReport>,
}
#[derive(serde::Serialize)]
struct HostReport {
    host: String,
    target: Option<String>,
    probe: ProbeResult,
}

/// The live state of a probed container, distinguishing "not running" from "no such container"
/// from "couldn't even ask" — a down host, a container that was simply never deployed there, and
/// a cleanly stopped container are three different operator-facing facts, not one.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
enum ProbeResult {
    Running {
        healthy: Option<bool>,
    },
    Stopped,
    Offline,
    Failed {
        exit_code: i64,
        message: String,
    },
    /// The host answered SSH, but no container by that name exists there (e.g. `docker inspect`
    /// returned "No such object") — reachable host, nothing deployed under that name.
    NotDeployed,
    /// SSH itself could not reach the host (connection refused/timed out/auth failure).
    Unreachable(String),
}

fn describe(probe: ProbeResult) -> String {
    match probe {
        ProbeResult::Offline => "offline".into(),
        ProbeResult::Failed { exit_code, message } => {
            format!("probe failed (exit {exit_code}): {}", safe(&message))
        }
        ProbeResult::Running {
            healthy: Some(true),
        } => "running, healthy".to_string().green().to_string(),
        ProbeResult::Running {
            healthy: Some(false),
        } => "running, unhealthy".to_string().yellow().to_string(),
        ProbeResult::Running { healthy: None } => "running".to_string().green().to_string(),
        ProbeResult::Stopped => "stopped".to_string().red().to_string(),
        ProbeResult::NotDeployed => "not deployed here".to_string().yellow().to_string(),
        ProbeResult::Unreachable(msg) => format!("{}: {}", "unreachable".red(), safe(&msg)),
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
        let lower = stderr.to_ascii_lowercase();
        if exit_code == 1
            && (lower.contains("no such object:") || lower.contains("no such container:"))
        {
            return ProbeResult::NotDeployed;
        }
        return ProbeResult::Failed {
            exit_code,
            message: stderr.chars().take(512).collect(),
        };
    }
    let out = stdout.trim();
    let mut parts = out.splitn(2, '|');
    let first = parts.next();
    if !matches!(first, Some("true" | "false")) {
        return ProbeResult::Failed {
            exit_code,
            message: "unexpected inspect output".into(),
        };
    }
    let health = parts.next();
    if !matches!(health, Some("healthy" | "unhealthy" | "starting" | "none")) {
        return ProbeResult::Failed {
            exit_code,
            message: "unexpected inspect health output".into(),
        };
    }
    if first == Some("false") {
        return ProbeResult::Stopped;
    }
    ProbeResult::Running {
        healthy: match health {
            Some("healthy") => Some(true),
            Some("none") => None,
            _ => Some(false),
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
