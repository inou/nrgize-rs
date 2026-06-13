//! `nrg ssh <host>` — open an interactive SSH session to a host, resolving any alias
//! through `~/.ssh/config` (so the same names the orchestration scripts use work here).

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
    let ssh_config = SshConfig::load_default();
    let resolved = ssh_config.resolve_host(&args.host);

    // Reject a host that `ssh` would parse as an option (e.g. `-oProxyCommand=...`), which would
    // run an arbitrary command on THIS machine before connecting. The `--` below is a second
    // layer, but we still refuse rather than connect to an attacker-shaped alias.
    if resolved.starts_with('-') {
        eprintln!(
            "{} refusing to connect to a host that looks like an option: {:?}",
            "Error:".red().bold(),
            resolved
        );
        return 1;
    }

    println!("Connecting to {}...", resolved);

    // Replace the current process with ssh. If `exec` returns, it failed. The literal `--`
    // ensures the host is never interpreted as an ssh option.
    let err = std::process::Command::new("ssh").arg("--").arg(&resolved).exec();

    eprintln!("{} Failed to execute ssh: {}", "Error:".red().bold(), err);
    1
}
