//! `nrg remove <service> [--host h] [--yes] [--purge-state]` — force-remove a service's own
//! canonical container (`<service>-web`, per `lib/deploy.rhai`'s naming convention) from each
//! host it's deployed to, per `.energize/state.json`.
//!
//! `docker rm -f` (immediate SIGKILL, no graceful stop first) — the exact same idiom
//! `lib/docker.rhai`'s `docker_remove` and `deploy()`'s own old-container cleanup already use
//! everywhere else in this codebase, not a departure from it. Since nothing here touches the
//! proxy (below), a container removed while its host's proxy is still routing to it can drop
//! in-flight requests — same risk any manual `docker rm -f` on a live backend carries.
//!
//! Scope (roadmap 1.5, step 2): this deliberately does NOT touch the host's shared proxy
//! (`kamal-proxy`/`caddy` — one instance serves every service on a host, so removing it here would
//! take down unrelated services) or accessories (there's no service-to-accessory mapping recorded
//! anywhere to remove them safely). It's the counterpart to `deploy()` for the one thing `deploy()`
//! alone owns per service: the app container itself. Proxy-route/accessory teardown remain a
//! separate, explicitly-deferred follow-up (see `docs/roadmap.md`) — until then, the proxy keeps
//! routing the service's domain to a now-gone backend after a successful `nrg remove`.

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

    /// Also delete this service's entries from `.energize/state.json` once the container removal
    /// succeeds everywhere it was attempted: always the per-host proxy target for every host
    /// actually removed, and the shared version/image/previous/deployed_at keys too — but ONLY if
    /// every host the service is recorded as deployed to was covered by this run (a `--host`
    /// targeting a strict subset of a multi-host service keeps those shared keys, since another
    /// host may still be running that version).
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

    // Captured once, before any state mutation, regardless of `--host`: this is the FULL fleet
    // `deploy()` itself believes the service is on — used below to decide whether it's safe to
    // purge the service-wide (not per-host) state keys.
    let recorded_hosts = store.hosts_for(&args.service);

    let hosts = match &args.host {
        Some(h) => vec![h.clone()],
        None => recorded_hosts.clone(),
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

    if !removed_hosts.is_empty() {
        println!(
            "Reminder: the shared proxy on these hosts still routes {:?}'s domain to the \
             now-removed container until its route is removed separately.",
            args.service
        );
    }

    if args.purge_state {
        if all_ok {
            // Only wipe the service-wide keys (version/image/prev/deployed_at) if THIS run
            // removed the container from every host `deploy()` believes it's on — not just
            // every host it was ATTEMPTED on. `--host` can target a strict subset of a
            // multi-host service; purging the global keys in that case would make `nrg status`
            // report "no deploy recorded" for a service a DIFFERENT host is still running and
            // serving traffic on, with no way to tell that host apart afterward (Opus review,
            // round 5).
            let purge_globals = recorded_hosts.iter().all(|h| removed_hosts.contains(h));
            purge_state(&mut store, &args.service, &removed_hosts, purge_globals);
            if purge_globals {
                println!("Purged state for {:?}.", args.service);
            } else {
                println!(
                    "Removed per-host state for {} of {} host(s) recorded for {:?}; kept its \
                     version/image/prev/deployed_at, since it's still deployed elsewhere.",
                    removed_hosts.len(),
                    recorded_hosts.len(),
                    args.service
                );
            }
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
/// end state the caller wants, an absent container, already holds), matching `RealRunner`'s
/// SSH-transport-failure convention elsewhere in this codebase (exit 255 is ssh itself failing to
/// connect, never a real `docker rm` outcome). The "no such" match is lowercased, mirroring
/// `sim.rs`'s `probe_absent_or_err` (robustness review R4/R31): Docker says `No such container`,
/// Podman says `no such container` — a case-sensitive match would silently misreport this as a
/// real failure under Podman (`nrg.runtime.cmd` supports both), breaking the idempotency this
/// exists for on a supported runtime (Opus review, round 5).
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
    if out.exit_code != 0 && !out.stderr.to_lowercase().contains("no such") {
        let msg = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("remove failed");
        return Err(msg.to_string());
    }
    Ok(())
}

/// Delete state keys for `service`: the per-host `<service>.target.<host>` entry for every host
/// this run actually removed the container from (a host that failed keeps its entry, since the
/// container — and thus the fact it's "deployed there" — may still be real), and, only when
/// `purge_globals` is true, the service-wide `version`/`image`/`prev`/`deployed_at` keys too.
/// `purge_globals` must be false whenever `--host` targeted a strict subset of the hosts the
/// service is actually deployed to — otherwise `nrg status` would report no deploy at all for a
/// service another, untouched host is still running.
fn purge_state(store: &mut StateStore, service: &str, removed_hosts: &[String], purge_globals: bool) {
    if purge_globals {
        for suffix in ["version", "image", "prev", "deployed_at"] {
            let _ = store.del(&format!("{service}.{suffix}"));
        }
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
    fn remove_container_treats_podmans_lowercase_wording_as_absent_too() {
        // Opus review, round 5: a case-sensitive match on Docker's exact "No such container"
        // capitalization silently misreported Podman's lowercase "no such container" as a real
        // failure — breaking the documented idempotency guarantee on a supported runtime
        // (`nrg.runtime.cmd` can be "podman", not just "docker").
        let runner = FakeRunner::new();
        runner.fail_host("web1", 1, "Error: app-web: no container with name or ID \"app-web\" found: no such container");
        assert!(remove_container(&runner, "web1", "podman", "app-web").is_ok());
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

    fn seeded_store() -> StateStore {
        let mut store = StateStore::ephemeral();
        store.set("app.version", "v42").unwrap();
        store.set("app.image", "ghcr.io/org/app:v42").unwrap();
        store.set("app.prev", "v41").unwrap();
        store.set("app.deployed_at", "2026-07-10T00:00:00Z").unwrap();
        store.set("app.target.web1", "localhost:13000").unwrap();
        store.set("app.target.web2", "localhost:13001").unwrap();
        store
    }

    #[test]
    fn purge_state_with_globals_clears_everything_removed() {
        let mut store = seeded_store();
        purge_state(&mut store, "app", &["web1".to_string(), "web2".to_string()], true);
        assert_eq!(store.get("app.version"), None);
        assert_eq!(store.get("app.image"), None);
        assert_eq!(store.get("app.prev"), None);
        assert_eq!(store.get("app.deployed_at"), None);
        assert_eq!(store.get("app.target.web1"), None);
        assert_eq!(store.get("app.target.web2"), None);
    }

    #[test]
    fn purge_state_without_globals_keeps_the_shared_keys_for_hosts_still_deployed() {
        // Opus review, round 5: `--host web1` on a service also deployed to web2 must NOT erase
        // app.version/image/prev/deployed_at — web2 is still running that version.
        let mut store = seeded_store();
        purge_state(&mut store, "app", &["web1".to_string()], false);
        assert_eq!(store.get("app.version").as_deref(), Some("v42"));
        assert_eq!(store.get("app.image").as_deref(), Some("ghcr.io/org/app:v42"));
        assert_eq!(store.get("app.prev").as_deref(), Some("v41"));
        assert_eq!(store.get("app.deployed_at").as_deref(), Some("2026-07-10T00:00:00Z"));
        // The host that WAS removed still loses its own per-host entry.
        assert_eq!(store.get("app.target.web1"), None);
        // The untouched host keeps its entry.
        assert_eq!(store.get("app.target.web2").as_deref(), Some("localhost:13001"));
    }
}
