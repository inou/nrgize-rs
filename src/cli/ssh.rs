use clap::Args;
use std::os::unix::process::CommandExt;

use crate::cli::ui;
use crate::parsing;
use crate::ssh::config::SshConfig;

#[derive(Args)]
pub struct SshArgs {
    /// Server name to connect to
    pub name: Option<String>,

    /// Explicit file path
    #[arg(long)]
    pub path: Option<String>,

    /// Filename in current directory
    #[arg(long)]
    pub conf: Option<String>,
}

pub fn execute(args: &SshArgs) -> i32 {
    let file_path = match parsing::resolve_file(args.path.as_deref(), args.conf.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    let config = match parsing::parse_file(&file_path, &std::collections::HashMap::new()) {
        Ok(c) => c,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    // Filter out local servers
    let remote_servers: Vec<_> = config
        .servers
        .iter()
        .filter(|(_, s)| !s.is_local())
        .collect();

    if remote_servers.is_empty() {
        ui::render_error("No remote servers defined.");
        return 1;
    }

    // Determine which server to connect to
    let server = if let Some(name) = &args.name {
        match config.servers.get(name) {
            Some(s) if !s.is_local() => s,
            Some(_) => {
                ui::render_error(&format!("Server '{}' is local.", name));
                return 1;
            }
            None => {
                let available: Vec<_> = remote_servers.iter().map(|(n, _)| n.as_str()).collect();
                ui::render_error(&format!(
                    "Server '{}' not found. Available: {}",
                    name,
                    available.join(", ")
                ));
                return 1;
            }
        }
    } else {
        // Prompt for server selection
        let names: Vec<&str> = remote_servers.iter().map(|(n, _)| n.as_str()).collect();

        if names.len() == 1 {
            remote_servers[0].1
        } else {
            let selection = dialoguer::Select::new()
                .with_prompt("Select a server")
                .items(&names)
                .default(0)
                .interact()
                .unwrap_or(0);

            remote_servers[selection].1
        }
    };

    // If multiple hosts, prompt for host selection
    let host = if server.hosts.len() == 1 {
        &server.hosts[0]
    } else {
        let selection = dialoguer::Select::new()
            .with_prompt("Select a host")
            .items(&server.hosts)
            .default(0)
            .interact()
            .unwrap_or(0);

        &server.hosts[selection]
    };

    // Resolve via SSH config and exec
    let ssh_config = SshConfig::load_default();
    let resolved = ssh_config.resolve_host(host);

    println!("Connecting to {}...", resolved);

    // Replace current process with SSH
    let err = std::process::Command::new("ssh")
        .arg(&resolved)
        .exec();

    // If we get here, exec failed
    ui::render_error(&format!("Failed to execute ssh: {}", err));
    1
}
