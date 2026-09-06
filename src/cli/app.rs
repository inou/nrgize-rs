//! `nrg app exec <service> [--host <host>] [-i] [cmd...]` — run a command inside a service's
//! LIVE container (the running `<service>-web`, per `lib/deploy.rhai`'s naming convention),
//! found by looking up the service's hosts in `.energize/state.json`.

use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use crate::ssh::config::SshConfig;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct AppArgs {
    #[command(subcommand)]
    pub command: AppCommand,
}

#[derive(Subcommand)]
pub enum AppCommand {
    /// Run a command inside a service's live container
    Exec(AppExecArgs),
}

#[derive(Args)]
pub struct AppExecArgs {
    /// Service name (the `service` argument passed to `deploy()`).
    pub service: String,

    /// Which host to exec into. Required if the service is deployed to more than one host.
    #[arg(long)]
    pub host: Option<String>,

    /// Allocate a TTY and hand over the terminal — for an interactive shell or console (e.g.
    /// `bin/rails console`). Without it, the command runs to completion non-interactively and
    /// its exit code is propagated as `nrg`'s own.
    #[arg(short, long)]
    pub interactive: bool,

    /// Command to run inside the container (default: `sh`). A token starting with `-` must
    /// follow a literal `--` separator.
    pub cmd: Vec<String>,
}

pub fn execute(args: &AppArgs) -> i32 {
    match &args.command {
        AppCommand::Exec(a) => execute_exec(a),
    }
}

fn execute_exec(args: &AppExecArgs) -> i32 {
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

    let host = match pick_host(&args.host, &store.hosts_for(&args.service)) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    if host.starts_with('-') {
        eprintln!("Error: refusing to connect to a host that looks like an option: {host:?}");
        return 1;
    }

    let container_cmd = store
        .get(&format!("{}.runtime.cmd", args.service))
        .or_else(|| store.get("nrg.runtime.cmd"))
        .unwrap_or_else(|| "docker".to_string());
    let container = format!("{}-web", args.service);
    let remote_cmd = build_remote_cmd(&container_cmd, &container, &args.cmd, args.interactive);

    // Display-only (robustness review R9): this resolver understands only HostName/User from
    // `~/.ssh/config`, so it's shown here purely as an informational hint of where `host` maps
    // to. The ACTUAL connection (below) passes the ALIAS itself to ssh, so ssh's own config
    // parsing applies IN FULL — Port, IdentityFile, ProxyJump, ProxyCommand, Host * wildcards,
    // Match blocks, etc. — instead of only the subset this resolver understands.
    let display_host = SshConfig::load_default().resolve_host(&host);

    // stderr, not stdout: the non-interactive path is documented as script/CI-safe (its stdout
    // is the container command's real output), so a banner on stdout would corrupt captured
    // output like `out=$(nrg app exec app -- rails db:migrate:status)`.
    if display_host == host {
        eprintln!("Connecting to {container} on {host}...");
    } else {
        // Say "resolves to", not "on" — this hint may be missing Port/ProxyJump/IdentityFile
        // that ssh's own, fuller config parsing will still apply on the real connection below.
        eprintln!(
            "Connecting to {container} on {host} (resolves to {display_host} per ~/.ssh/config)..."
        );
    }

    // Wait for SSH so the CLI can record the operation outcome, preserving its exit code.
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(ssh_extra_args(args.interactive));
    cmd.arg("--").arg(&host).arg(&remote_cmd);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("Error: failed to execute ssh: {e}");
            1
        }
    }
}

/// Extra options for the OUTER `ssh` connection. Interactive requests a TTY (`-t`), needed for a
/// real shell/console. Non-interactive sets `BatchMode=yes` instead: that path is documented as
/// script/CI-safe, so it must never sit waiting on a password/keyboard-interactive prompt with
/// nothing attached to answer it — an interactive session deliberately skips this, since a human
/// may need to answer a host-key or auth prompt (matching `nrg ssh`'s own plain interactive
/// style).
///
/// Both modes also get a keep-alive (robustness review R5 — same fix as `RealRunner::ssh_command`
/// and `nrg logs`'s `ssh_stream_command`): this call doesn't hold `nrg`'s own project state lock
/// the way `nrg exec`/`nrg run` do, but the non-interactive path is documented CI-safe, so a
/// connection that silently goes dead shouldn't leave an unattended CI job hanging forever either.
fn ssh_extra_args(interactive: bool) -> Vec<&'static str> {
    let mut args = if interactive {
        vec!["-t"]
    } else {
        vec!["-o", "BatchMode=yes"]
    };
    args.extend([
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
    ]);
    args
}

/// Resolve which host to exec into: an explicit `--host` wins outright (it may name a host that
/// isn't even in state — e.g. troubleshooting a container `deploy()` never touched); otherwise
/// the service must be deployed to EXACTLY one host, or the caller must disambiguate.
fn pick_host(explicit: &Option<String>, recorded: &[String]) -> Result<String, String> {
    if let Some(h) = explicit {
        return Ok(h.clone());
    }
    match recorded {
        [] => Err(
            "no hosts recorded for this service (has it been deployed?); pass --host explicitly"
                .to_string(),
        ),
        [only] => Ok(only.clone()),
        many => Err(format!(
            "deployed to {} hosts ({}); pass --host to pick one",
            many.len(),
            many.join(", ")
        )),
    }
}

/// Build the remote `docker exec` (or configured runtime) invocation. `interactive` adds `-it`
/// (matching the `-t` added to the outer `ssh` call) so a real shell/console gets a working TTY;
/// without it, the command runs to completion and its exit code propagates normally.
fn build_remote_cmd(
    container_cmd: &str,
    container: &str,
    cmd_args: &[String],
    interactive: bool,
) -> String {
    let exec_flags = if interactive { "exec -it" } else { "exec" };
    let mut parts = vec![
        container_cmd.to_string(),
        exec_flags.to_string(),
        posix_quote(container),
    ];
    if cmd_args.is_empty() {
        parts.push("sh".to_string());
    } else {
        parts.extend(cmd_args.iter().map(|c| posix_quote(c)));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_extra_args_interactive_requests_a_tty() {
        assert_eq!(
            ssh_extra_args(true),
            vec![
                "-t",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4"
            ]
        );
    }

    #[test]
    fn ssh_extra_args_non_interactive_sets_batch_mode_so_it_cannot_hang_on_a_prompt() {
        assert_eq!(
            ssh_extra_args(false),
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4"
            ]
        );
    }

    #[test]
    fn pick_host_explicit_wins_even_if_not_recorded() {
        assert_eq!(pick_host(&Some("web9".to_string()), &[]).unwrap(), "web9");
    }

    #[test]
    fn pick_host_defaults_to_the_only_recorded_host() {
        assert_eq!(pick_host(&None, &["web1".to_string()]).unwrap(), "web1");
    }

    #[test]
    fn pick_host_errors_on_no_hosts() {
        assert!(pick_host(&None, &[]).is_err());
    }

    #[test]
    fn pick_host_errors_on_multiple_hosts_without_explicit_choice() {
        let err = pick_host(&None, &["web1".to_string(), "web2".to_string()]).unwrap_err();
        assert!(err.contains("web1") && err.contains("web2"), "got: {err}");
    }

    #[test]
    fn build_remote_cmd_defaults_to_sh_non_interactive() {
        assert_eq!(
            build_remote_cmd("docker", "app-web", &[], false),
            "docker exec 'app-web' sh"
        );
    }

    #[test]
    fn build_remote_cmd_interactive_adds_it_flag() {
        assert_eq!(
            build_remote_cmd("docker", "app-web", &[], true),
            "docker exec -it 'app-web' sh"
        );
    }

    #[test]
    fn build_remote_cmd_quotes_each_arg_defending_against_injection() {
        let cmd = build_remote_cmd(
            "docker",
            "app-web",
            &["rails".to_string(), "console; rm -rf /".to_string()],
            true,
        );
        assert_eq!(cmd, "docker exec -it 'app-web' 'rails' 'console; rm -rf /'");
    }
}
