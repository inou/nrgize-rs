//! `nrg setup --host h [--host h]... [--proxy kamal|caddy] [--proxy-version v] [--network name]
//! [--yes] [--dry-run]` — bootstrap a fresh host for its first deploy (roadmap 1.5 step 1):
//! install a container runtime if absent, create the network (if `--network` is given), and
//! boot the proxy. Complements `nrg remove` (step 2) and `nrg doctor --host` (step 3) — together
//! these three cover "fresh Ubuntu box -> ready for `deploy()`" without hand-installing anything.
//!
//! Scope: deliberately does NOT start accessories. There's no manifest anywhere in this codebase
//! recording which accessories a given service needs — `accessory_run` calls are entirely
//! project-script-defined (see `docs/deploy.md`'s accessory lifecycle section) — so there is
//! nothing generic for a native command to auto-invoke. A project that wants accessories
//! bootstrapped alongside `nrg setup` can define its own function (e.g. `fn setup_accessories()`)
//! and call it separately via `nrg run setup_accessories` once this succeeds.
//!
//! Also deliberately only auto-installs Docker, never Podman/nerdctl: Docker's official
//! convenience script (`https://get.docker.com`) is the one well-known, stable, universally
//! documented command for exactly this "fresh box, no container runtime yet" case. Installing a
//! different runtime is left to the operator — a genuine, narrower-than-the-roadmap-wording
//! scoping decision, matching the same "deliberately scoped narrower" precedent `nrg remove` set.
//! This narrowing runs deeper than just the install step, though (Fable review): the
//! network/proxy-boot phase always calls through the stdlib's `rt::container_cmd()`, which
//! defaults to `"docker"` unless a project script calls `rt::set_runtime(...)` first — and this
//! command never evaluates the project's script, so there's no way for that override to run. A
//! Podman/nerdctl-only host is probed as "runtime present" but will still fail at network/proxy
//! boot; `execute()` prints a warning up front when it detects this.
//!
//! Hosts are required via `--host` (repeatable): unlike `nrg doctor`/`nrg remove`, a fresh host
//! has no recorded state yet to auto-discover a target from — that is the entire scenario this
//! command exists for.
//!
//! Reuses `nrg rollback`'s exact architecture: a native (non-Rhai) preflight over raw SSH for the
//! runtime-install step (the ONE part that must run before any Rhai-side container primitive
//! could possibly work), then the shared `execute_with`/`wire_run` machinery (state lock,
//! `--dry-run` overlay, audit-trail logging) to invoke `eval::run_setup`, which synthesizes a
//! direct call into the stdlib's `docker::docker_network_create_all`/`proxy::proxy_boot_all` (or
//! `caddy::proxy_boot_all`) — reusing the SAME proxy-boot logic `deploy()` itself uses, rather
//! than reimplementing "start kamal-proxy" in Rust.

use crate::cli::exec::{execute_with, resolve_file, AuditMeta};
use crate::engine::eval;
use crate::engine::runner::{CommandRunner, RealRunner};
use clap::Args;

#[derive(Args)]
pub struct SetupArgs {
    /// Host to bootstrap. Repeatable; at least one is required.
    #[arg(long = "host", required = true)]
    pub hosts: Vec<String>,

    /// Proxy backend to boot: "kamal" (default, kamal-proxy) or "caddy".
    #[arg(long, default_value = "kamal")]
    pub proxy: String,

    /// Pin the proxy container's own image tag (e.g. "v0.9.2" for kamal-proxy, "2.8.4" for
    /// Caddy) — see `deploy()`'s own `cfg.proxy_version`. Defaults to each backend's own default
    /// (kamal-proxy: "latest", with a mutable-tag warning; Caddy: "2").
    #[arg(long)]
    pub proxy_version: Option<String>,

    /// Create this network on every host (idempotent — a no-op if it already exists). Nothing
    /// created here uses it automatically: pass the same name as `cfg.network` to `deploy()`/
    /// `accessory_run` calls that should join it.
    #[arg(long)]
    pub network: Option<String>,

    /// Actually install Docker on a host that's missing a container runtime (runs the official
    /// https://get.docker.com convenience script over SSH, as root). Without this flag, a
    /// missing runtime is reported but nothing is installed and nothing else proceeds — this is
    /// a consequential, hard-to-undo, root-level action, so it doesn't run on the first ask
    /// (same convention as `nrg remove`'s `--yes`).
    #[arg(long)]
    pub yes: bool,

    /// Show what would happen without executing anything: no install, no lock, no state writes.
    #[arg(long)]
    pub dry_run: bool,

    /// Path to the `.rhai` file whose directory anchors `import "lib/docker"`/`"lib/proxy"`
    /// resolution — its CONTENTS are never read or run, only its directory. Defaults to the
    /// project's Energize.rhai, same convention as `nrg rollback --file`.
    #[arg(long)]
    pub file: Option<String>,
}

pub fn execute(args: &SetupArgs) -> i32 {
    if let Some(bad) = args.hosts.iter().find(|h| h.trim().is_empty()) {
        eprintln!("Error: a --host value cannot be empty or blank (got {bad:?}).");
        return 1;
    }
    if args.proxy != "kamal" && args.proxy != "caddy" {
        eprintln!("Error: --proxy must be \"kamal\" or \"caddy\" (got {:?}).", args.proxy);
        return 1;
    }

    let path = match resolve_file(
        &args.file,
        "nrg setup only reads this file's DIRECTORY (to resolve lib/docker.rhai and lib/proxy.rhai\n\
         imports, if you've vendored them) — its contents are never compiled or run. Run `nrg init`\n\
         first to create one, or pass an existing file:\n  nrg setup --host h --file deploy.rhai",
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    // `resolve_file` doesn't check the path actually exists (see `nrg rollback`'s identical
    // comment) — `nrg setup` never compiles this file either, only anchors module resolution to
    // its directory, so nothing else would ever catch a typo'd path.
    if !std::path::Path::new(&path).is_file() {
        eprintln!("Error: {path:?} does not exist (or is not a file).");
        return 1;
    }

    let runner = RealRunner;
    let checks = probe_hosts(&runner, &args.hosts);
    let unreachable: Vec<&str> =
        checks.iter().filter(|c| !c.reachable).map(|c| c.host.as_str()).collect();
    if !unreachable.is_empty() {
        eprintln!("Not reachable via SSH: {}", unreachable.join(", "));
        return 1;
    }

    // Fable review: probing accepts Podman/nerdctl as "runtime present", but the network/proxy
    // boot step below always runs through `docker::docker_network_create_all`/`proxy::proxy_boot_all`
    // via `rt::container_cmd()`, which defaults to `"docker"` — and this command never evaluates
    // the project's own script, so there's no way for a project-defined `rt::set_runtime(...)`
    // call to run first. A Podman-only host would fail confusingly further down; warn up front.
    let non_docker: Vec<&str> = checks
        .iter()
        .filter(|c| c.runtime.as_deref().is_some_and(|r| !r.to_lowercase().contains("docker")))
        .map(|c| c.host.as_str())
        .collect();
    if !non_docker.is_empty() {
        eprintln!(
            "Warning: {} — the network/proxy boot step below always runs `docker ...` commands \
             (this command never installs or targets Podman/nerdctl); it will fail on a host \
             without Docker specifically.",
            non_docker.join(", ")
        );
    }

    let missing: Vec<&str> =
        checks.iter().filter(|c| c.runtime.is_none()).map(|c| c.host.as_str()).collect();
    if !missing.is_empty() {
        if args.dry_run {
            println!("Would install Docker (https://get.docker.com) on: {}", missing.join(", "));
        } else if !args.yes {
            println!(
                "No container runtime found on: {}. Re-run with --yes to install Docker there \
                 (runs the official https://get.docker.com convenience script over SSH, as \
                 root). Nothing else was attempted.",
                missing.join(", ")
            );
            return 0;
        } else {
            let mut all_ok = true;
            for host in &missing {
                match install_docker(&runner, host) {
                    Ok(()) => println!("{host}: Docker installed"),
                    Err(e) => {
                        eprintln!("{host}: {e}");
                        all_ok = false;
                    }
                }
            }
            // All-or-nothing, matching `deploy()`'s own fleet-atomic philosophy: proceeding to
            // network/proxy boot on hosts that just failed to get a runtime installed would only
            // produce a second, more confusing failure right behind the first.
            if !all_ok {
                return 1;
            }
        }
    } else {
        for check in &checks {
            println!(
                "{}: container runtime already present ({})",
                check.host,
                check.runtime.as_deref().unwrap_or("?")
            );
        }
    }

    let audit_args: Vec<String> = args.hosts.iter().map(|h| format!("--host={h}")).collect();
    let meta = AuditMeta { command: "setup", target: None, args: &audit_args };
    execute_with(&path, args.dry_run, None, None, meta, |path, ctx| {
        // Same fallback as `nrg rollback` (see `eval.rs::build_for`'s identical comment): a bare
        // relative `--file` has a `parent()` of `Some("")`, not `None`, which must be treated as
        // CWD or module resolution renders a confusing `"" has no lib/proxy.rhai"` error.
        let import_root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| ".".into());
        eval::run_setup(
            &import_root,
            &args.hosts,
            &args.proxy,
            args.proxy_version.as_deref(),
            args.network.as_deref(),
            ctx,
        )
    })
}

/// Install Docker via its official convenience script. `https://get.docker.com` is Docker's own
/// long-standing, maintained installer entrypoint (documented at
/// https://docs.docker.com/engine/install/ubuntu/#install-using-the-convenience-script) — a
/// stable public URL, not the kind of unverifiable specific reference (e.g. a commit SHA) that
/// would need independent confirmation before use.
///
/// Opus review: a plain `curl ... | sh` pipeline's exit code is `sh`'s, not `curl`'s — if the
/// download itself fails (DNS, a transient 5xx, a network blip on a genuinely fresh box), `sh`
/// reads EOF from the empty pipe and exits 0, so this would report success and let the caller
/// proceed to boot the proxy on a host that never actually got Docker. Downloading to a file
/// first, THEN running it, means a failed `curl` short-circuits the `&&` and its own real exit
/// code is what `$?` (and thus this whole command) reports.
///
/// Fable review: a fixed `/tmp/nrg-get-docker.sh` path is `curl -o`'d with O_TRUNC, not O_EXCL,
/// so another local user could pre-create or symlink it and rewrite its contents in the window
/// between the download finishing and `sh` running it — code the ssh login user (often root)
/// would then execute. `mktemp` gives each run a private, unpredictable path instead.
fn install_docker(runner: &dyn CommandRunner, host: &str) -> Result<(), String> {
    let out = runner.run_ssh(
        host,
        r#"t=$(mktemp) && curl -fsSL https://get.docker.com -o "$t" && sh "$t"; \
         rc=$?; rm -f "$t"; exit $rc"#,
    );
    if out.exit_code == 255 {
        return Err(format!("unreachable: {}", first_reason(&out.stderr, "ssh failed")));
    }
    if out.exit_code != 0 {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        return Err(first_reason(&combined, "docker install failed"));
    }
    Ok(())
}

/// The first non-blank line of `s`, or `fallback` — the same idiom `nrg remove`/`nrg lock` each
/// keep their own copy of (no shared helper module exists for this yet in this codebase).
fn first_reason(s: &str, fallback: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(fallback).to_string()
}

/// The result of preflighting one host: SSH reachability, and — only if reachable — which
/// container runtime binary (if any) is on its `PATH`. Identical in shape to `doctor.rs`'s own
/// `HostCheck`/`probe_host`/`probe_hosts` (no shared module exists for this yet either).
struct HostCheck {
    host: String,
    reachable: bool,
    runtime: Option<String>,
}

/// Preflight every host IN PARALLEL, results returned in the SAME order as `hosts` (not
/// completion order) — see `doctor.rs`'s identical `probe_hosts` for the full rationale.
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

fn probe_host(runner: &dyn CommandRunner, host: &str) -> HostCheck {
    let ssh = runner.run_ssh(host, "true");
    if ssh.exit_code != 0 {
        return HostCheck { host: host.to_string(), reachable: false, runtime: None };
    }
    let rt = runner.run_ssh(host, "command -v docker || command -v podman || command -v nerdctl");
    let runtime = if rt.exit_code == 0 {
        rt.stdout.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
    } else {
        None
    };
    HostCheck { host: host.to_string(), reachable: true, runtime }
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
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn probe_host_reports_reachable_with_runtime_found() {
        let mut runner = FakeRunner::new();
        runner.default =
            RawOutput { stdout: "/usr/bin/docker\n".to_string(), stderr: String::new(), exit_code: 0 };
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
    fn install_docker_succeeds_on_a_clean_install() {
        let runner = FakeRunner::new();
        assert!(install_docker(&runner, "web1").is_ok());
        assert!(runner.calls()[0].contains("get.docker.com"));
    }

    #[test]
    fn install_docker_distinguishes_ssh_transport_failure_from_a_real_install_failure() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 255, "ssh: connect to host web1 port 22: Connection refused");
        let err = install_docker(&runner, "web1").unwrap_err();
        assert!(err.starts_with("unreachable:"), "got: {err}");
        assert!(err.contains("Connection refused"), "got: {err}");
    }

    #[test]
    fn install_docker_reports_a_real_install_failure() {
        let runner = FakeRunner::new();
        runner.fail_host("web1", 1, "E: Unable to locate package docker-ce");
        let err = install_docker(&runner, "web1").unwrap_err();
        assert!(err.contains("docker-ce"), "got: {err}");
    }

    #[test]
    fn first_reason_skips_leading_blank_lines() {
        assert_eq!(first_reason("\n  \nreal error\n", "fallback"), "real error");
    }

    #[test]
    fn first_reason_falls_back_when_everything_is_blank() {
        assert_eq!(first_reason("\n  \n", "fallback"), "fallback");
    }
}
