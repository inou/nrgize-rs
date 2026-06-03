//! `nrg init` — scaffold a starter `Energize.rhai` orchestration file.

use clap::Args;
use crossterm::style::Stylize;
use std::path::Path;

#[derive(Args)]
pub struct InitArgs {}

/// Default orchestration filename written by `nrg init`.
const DEFAULT_FILE: &str = "Energize.rhai";

const RHAI_TEMPLATE: &str = r#"// Energize.rhai — Rhai orchestration module.
//
//   nrg run <fn> [args]   call a function defined here
//   nrg exec              run this file top-to-bottom
//   nrg exec --dry-run    show the plan without executing
//
// Builtins: ssh_exec(host, cmd), ssh_exec_all(hosts, cmd), local_exec(cmd),
//           http_get(url), state_get/state_set(key, value), sleep(secs).

let HOSTS = ["user@example.com"];

// `nrg run deploy`
fn deploy() {
    for host in HOSTS {
        let r = ssh_exec(host, "cd /var/www/app && git pull origin main");
        if !r.ok { throw "deploy failed on " + host + ": " + r.stderr; }
    }
    print("Deployed to all hosts.");
}

// `nrg run uptime`
fn uptime() {
    ssh_exec_all(HOSTS, "uptime");
}
"#;

pub fn execute(_args: &InitArgs) -> i32 {
    if Path::new(DEFAULT_FILE).exists() {
        eprintln!("{} {} already exists.", "Error:".red().bold(), DEFAULT_FILE);
        return 1;
    }

    match std::fs::write(DEFAULT_FILE, RHAI_TEMPLATE) {
        Ok(_) => {
            println!("{} Created {}", "✓".green(), DEFAULT_FILE);
            0
        }
        Err(e) => {
            eprintln!("{} Failed to write {}: {}", "Error:".red().bold(), DEFAULT_FILE, e);
            1
        }
    }
}
