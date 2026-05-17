use clap::Args;
use crossterm::style::Stylize;

use crate::cli::ui;
use crate::parsing;
use crate::ssh::config::SshConfig;

#[derive(Args)]
pub struct DoctorArgs {
    /// Explicit file path
    #[arg(long)]
    pub path: Option<String>,

    /// Filename in current directory
    #[arg(long)]
    pub conf: Option<String>,
}

pub async fn execute(args: &DoctorArgs) -> i32 {
    println!("\n{}\n", "Energize Doctor".bold());

    let mut all_ok = true;

    // Check 1: Task file exists
    let file_path = match parsing::resolve_file(args.path.as_deref(), args.conf.as_deref()) {
        Ok(p) => {
            check_pass(&format!("Task file found: {}", p.display()));
            p
        }
        Err(e) => {
            check_fail(&format!("Task file: {}", e));
            return 1;
        }
    };

    // Check 2: File is parseable
    let config = match parsing::parse_file(&file_path, &std::collections::HashMap::new()) {
        Ok(c) => {
            check_pass("Task file parses successfully");
            c
        }
        Err(e) => {
            check_fail(&format!("Parse error: {}", e));
            return 1;
        }
    };

    // Check 3: Servers defined
    if config.servers.is_empty() {
        check_fail("No servers defined");
        all_ok = false;
    } else {
        check_pass(&format!("{} server(s) defined", config.servers.len()));
    }

    // Check 4: Tasks defined
    if config.tasks.is_empty() {
        check_fail("No tasks defined");
        all_ok = false;
    } else {
        check_pass(&format!("{} task(s) defined", config.tasks.len()));
    }

    // Check 5: Macro references resolve
    let mut broken_refs = Vec::new();
    for (name, macro_def) in &config.macros {
        for task_name in &macro_def.tasks {
            if !config.tasks.contains_key(task_name) {
                broken_refs.push(format!("macro '{}' references unknown task '{}'", name, task_name));
            }
        }
    }

    if broken_refs.is_empty() {
        if !config.macros.is_empty() {
            check_pass("All macro references resolve");
        }
    } else {
        for msg in &broken_refs {
            check_fail(msg);
        }
        all_ok = false;
    }

    // Check 6: SSH connectivity
    let ssh_config = SshConfig::load_default();
    let remote_servers: Vec<_> = config
        .servers
        .values()
        .filter(|s| !s.is_local())
        .collect();

    if !remote_servers.is_empty() {
        println!("\n  {}", "SSH Connectivity:".bold());

        for server in &remote_servers {
            for host in &server.hosts {
                let resolved = ssh_config.resolve_host(host);
                let start = std::time::Instant::now();

                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    check_ssh_connectivity(&resolved),
                )
                .await;

                let elapsed = start.elapsed();

                match result {
                    Ok(true) => {
                        check_pass(&format!(
                            "{} ({}) — {:.0}ms",
                            server.name,
                            resolved,
                            elapsed.as_millis()
                        ));
                    }
                    Ok(false) => {
                        check_fail(&format!("{} ({}) — connection failed", server.name, resolved));
                        all_ok = false;
                    }
                    Err(_) => {
                        check_fail(&format!("{} ({}) — timeout (5s)", server.name, resolved));
                        all_ok = false;
                    }
                }
            }
        }
    }

    println!();

    if all_ok {
        ui::render_success("All checks passed!");
        0
    } else {
        ui::render_warning("Some checks failed.");
        1
    }
}

async fn check_ssh_connectivity(host: &str) -> bool {
    let result = tokio::process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(host)
        .arg("echo ok")
        .output()
        .await;

    matches!(result, Ok(output) if output.status.success())
}

fn check_pass(msg: &str) {
    println!("  {} {}", "✓".green(), msg);
}

fn check_fail(msg: &str) {
    println!("  {} {}", "✗".red(), msg);
}
