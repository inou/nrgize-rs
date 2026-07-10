//! `nrg ssh <host>` — open an interactive SSH session to a host. Passes `host` straight through
//! to the real `ssh` binary (which resolves any `~/.ssh/config` alias itself, in full — Port,
//! IdentityFile, ProxyJump, etc. included), so the same names the orchestration scripts use work
//! here exactly as a plain `ssh <alias>` would (robustness review R9).

use clap::Args;
use crossterm::style::Stylize;
use std::os::unix::process::CommandExt;

use crate::ssh::config::SshConfig;

#[derive(Args)]
pub struct SshArgs {
    /// Host to connect to (an `~/.ssh/config` alias, or `user@hostname`).
    pub host: String,
}

pub fn execute(args: &SshArgs) -> i32 {
    // Reject a host that `ssh` would parse as an option (e.g. `-oProxyCommand=...`), which would
    // run an arbitrary command on THIS machine before connecting. The `--` below is a second
    // layer, but we still refuse rather than connect to an attacker-shaped alias.
    if args.host.starts_with('-') {
        eprintln!(
            "{} refusing to connect to a host that looks like an option: {:?}",
            "Error:".red().bold(),
            args.host
        );
        return 1;
    }

    // Display-only (robustness review R9): this resolver understands only HostName/User from
    // `~/.ssh/config`, so it's shown here purely as an informational hint of where `args.host`
    // maps to. The ACTUAL connection (below) passes the ALIAS itself to ssh, so ssh's own config
    // parsing applies IN FULL — Port, IdentityFile, ProxyJump, ProxyCommand, Host * wildcards,
    // Match blocks, etc. — instead of only the subset this resolver understands.
    let display_host = SshConfig::load_default().resolve_host(&args.host);
    println!("Connecting to {}...", display_host);

    // Replace the current process with ssh, passing the ORIGINAL alias — letting the real `ssh`
    // binary do its own, complete config resolution (matching a plain interactive `ssh <alias>`).
    // If `exec` returns, it failed. The literal `--` ensures the host is never interpreted as an
    // ssh option.
    let err = std::process::Command::new("ssh").arg("--").arg(&args.host).exec();

    eprintln!("{} Failed to execute ssh: {}", "Error:".red().bold(), err);
    1
}
