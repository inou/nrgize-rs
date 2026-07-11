//! `nrg remove <service> [--host h] [--yes] [--purge-state]` — stop and remove a service's own
//! canonical container (`<service>-web`, per `lib/deploy.rhai`'s naming convention) from each
//! host it's deployed to, per `.energize/state.json`.
//!
//! Scope (roadmap 1.5, step 2): this deliberately does NOT touch the host's shared proxy
//! (`kamal-proxy`/`caddy` — one instance serves every service on a host, so removing it here would
//! take down unrelated services) or accessories (there's no service-to-accessory mapping recorded
//! anywhere to remove them safely). It's the counterpart to `deploy()` for the one thing `deploy()`
//! alone owns per service: the app container itself. Proxy-route/accessory teardown remain a
//! separate, explicitly-deferred follow-up (see `docs/roadmap.md`).

use crate::engine::runner::{CommandRunner, RealRunner};
use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use clap::Args;

#[derive(Args)]
pub struct RemoveArgs {
    /// Service name (the `service` argument passed to `deploy()`).
    pub service: String,

    /// Only remove the container on this host, instead of every host recorded in state.
    #[arg(long)]
    pub host: Option<String>,

    /// Actually perform the removal. Without this flag, `nrg remove` only prints what it WOULD
    /// remove and exits — this is a destructive, hard-to-undo operation, so it doesn't run on the
    /// first ask.
    #[arg(long)]
    pub yes: bool,

    /// Also delete this service's entries from `.energize/state.json` (version, image, previous,
    /// deployed_at, and the per-host proxy target for every host actually removed) once the
    /// container removal succeeds everywhere it was attempted.
    #[arg(long)]
    pub purge_state: bool,
}

pub fn execute(args: &RemoveArgs) -> i32 {
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let mut store = match StateStore::load(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let hosts = match &args.host {
        Some(h) => vec![h.clone()],
        None => store.hosts_for(&args.service),
    };
    if hosts.is_empty() {
        println!(
            "No hosts recorded for {:?} (has it been deployed?); pass --host to target one directly.",
            args.service
        );
        return 0;
    }

    let container_cmd = store.get("nrg.runtime.cmd").unwrap_or_else(|| "docker".to_string());
    let container = format!("{}-web", args.service);

    if !args.yes {
        println!("Would remove {container:?} from:");
        for host in &hosts {
            println!("  {host}");
        }
        println!("Re-run with --yes to actually remove it. This does not touch the shared proxy or any accessories on these hosts.");
        return 0;
    }

    let runner = RealRunner;
    let mut all_ok = true;
    let mut removed_hosts = Vec::new();
    for host in &hosts {
        match remove_container(&runner, host, &container_cmd, &container) {
            Ok(()) => {
                println!("{host}: removed");
                removed_hosts.push(host.clone());
            }
            Err(e) => {
                eprintln!("{host}: {e}");
                all_ok = false;
            }
        }
    }

    if args.purge_state {
        if all_ok {
            purge_state(&mut store, &args.service, &removed_hosts);
            println!("Purged state for {:?}.", args.service);
        } else {
            eprintln!(
                "Skipping --purge-state: at least one host failed, so state would no longer match reality."
            );
        }
    }

    if all_ok {
        0
    } else {
        1
    }
}

/// `docker rm -f <container>` over SSH. Treats "no such container" as success (idempotent — the
/// end state the caller wants, an absent container, already holds), matching `docker rm`'s own
/// exit-127/1 distinction from `RealRunner`'s SSH-transport-failure convention elsewhere in this
/// codebase (exit 255 is ssh itself failing to connect, never a real `docker rm` outcome).
fn remove_container(runner: &dyn CommandRunner, host: &str, container_cmd: &str, name: &str) -> Result<(), String> {
    let cmd = format!("{container_cmd} rm -f {}", posix_quote(name));
    let out = runner.run_ssh(host, &cmd);
    if out.exit_code == 255 {
        let msg = out
            .stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("ssh failed")
            .to_string();
        return Err(format!("unreachable: {msg}"));
    }
    if out.exit_code != 0 && !out.stderr.contains("No such container") {
        let msg = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("remove failed");
        return Err(msg.to_string());
    }
    Ok(())
}

/// Delete every state key `deploy()`/`rollback()` write for `service`, scoped to only the hosts
/// this run actually removed the container from — a host that failed above keeps its
/// `<service>.target.<host>` entry, since the container (and thus the fact it's "deployed there")
/// may still be real.
fn purge_state(store: &mut StateStore, service: &str, removed_hosts: &[String]) {
    for suffix in ["version", "image", "prev", "deployed_at"] {
        let _ = store.del(&format!("{service}.{suffix}"));
    }
    for host in removed_hosts {
        let _ = store.del(&format!("{service}.target.{host}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn remove_container_succeeds_on_a_clean_removal() {
        let runner = FakeRunner::new();
        assert!(remove_container(&runner, "web1", "docker", "app-web").is_ok());
        assert!(runner.calls()[0].contains("docker rm -f 'app-web'"));
    }

    #[test]
    fn remove_container_treats_already_absent_as_success() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 1, "Error: No such container: app-web");
        assert!(remove_container(&runner, "web1", "docker", "app-web").is_ok());
    }

    #[test]
    fn remove_container_reports_a_real_docker_failure() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 1, "Error: permission denied");
        let err = remove_container(&runner, "web1", "docker", "app-web").unwrap_err();
        assert!(err.contains("permission denied"), "got: {err}");
    }

    #[test]
    fn remove_container_distinguishes_ssh_transport_failure_from_a_docker_error() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 255, "ssh: connect to host web1 port 22: Connection refused");
        let err = remove_container(&runner, "web1", "docker", "app-web").unwrap_err();
        assert!(err.starts_with("unreachable:"), "got: {err}");
        assert!(err.contains("Connection refused"), "got: {err}");
    }

    #[test]
    fn remove_container_quotes_the_container_name_defending_against_injection() {
        let runner = FakeRunner::new();
        let _ = remove_container(&runner, "web1", "docker", "app-web; rm -rf /");
        let calls = runner.calls();
        assert!(calls[0].contains("'app-web; rm -rf /'"), "got: {}", calls[0]);
    }
}
