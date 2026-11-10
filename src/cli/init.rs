//! `nrg init` — scaffold a starter `Energize.rhai` orchestration file.

use clap::{Args, ValueEnum};
use crossterm::style::Stylize;
use std::path::Path;

#[derive(Args)]
pub struct InitArgs {
    /// Scaffold a framework-specific starter instead of the generic template.
    #[arg(long, value_enum)]
    pub template: Option<Template>,
}

/// The framework starters `lib/examples/*.rhai` ships — full sample `Energize.rhai` files,
/// written verbatim by `nrg init --template <name>`.
#[derive(Clone, Copy, ValueEnum)]
pub enum Template {
    Release,
    Rails,
    Django,
    Nextjs,
    Phoenix,
    Laravel,
}

impl Template {
    fn source(self) -> &'static str {
        match self {
            Template::Release => include_str!("../../lib/examples/release.rhai"),
            Template::Rails => include_str!("../../lib/examples/rails.rhai"),
            Template::Django => include_str!("../../lib/examples/django.rhai"),
            Template::Nextjs => include_str!("../../lib/examples/nextjs.rhai"),
            Template::Phoenix => include_str!("../../lib/examples/phoenix.rhai"),
            Template::Laravel => include_str!("../../lib/examples/laravel.rhai"),
        }
    }
}

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

const HOSTS = ["user@example.com"];

// `nrg run deploy`
fn deploy() {
    for host in global::HOSTS {
        let r = ssh_exec(host, "cd /var/www/app && git pull origin main");
        if !r.ok { throw "deploy failed on " + host + ": " + r.stderr; }
    }
    print("Deployed to all hosts.");
}

// `nrg run uptime`
fn uptime() {
    ssh_exec_all(global::HOSTS, "uptime");
}
"#;

pub fn execute(args: &InitArgs) -> i32 {
    if Path::new(DEFAULT_FILE).exists() {
        eprintln!("{} {} already exists.", "Error:".red().bold(), DEFAULT_FILE);
        return 1;
    }

    let content = match args.template {
        Some(t) => t.source().to_string(),
        None => RHAI_TEMPLATE.to_string(),
    };

    match std::fs::write(DEFAULT_FILE, content) {
        Ok(_) => {
            println!("{} Created {}", "✓".green(), DEFAULT_FILE);
            0
        }
        Err(e) => {
            eprintln!(
                "{} Failed to write {}: {}",
                "Error:".red().bold(),
                DEFAULT_FILE,
                e
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_import_the_embedded_stdlib_and_nothing_from_disk() {
        for t in [
            Template::Release,
            Template::Rails,
            Template::Django,
            Template::Nextjs,
            Template::Phoenix,
            Template::Laravel,
        ] {
            assert!(t.source().contains("import \"std/"));
            assert!(!t.source().contains("import \"lib/"));
        }
    }
}
