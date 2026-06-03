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

    println!("Connecting to {}...", resolved);

    // Replace the current process with ssh. If `exec` returns, it failed.
    let err = std::process::Command::new("ssh").arg(&resolved).exec();

    eprintln!("{} Failed to execute ssh: {}", "Error:".red().bold(), err);
    1
}
