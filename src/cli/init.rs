//! `nrg init` — scaffold a starter `Energize.rhai` orchestration file.

use clap::{Args, ValueEnum};
use crossterm::style::Stylize;
use std::path::Path;

#[derive(Args)]
pub struct InitArgs {
    /// Scaffold a framework-specific starter (roadmap 3.4) instead of the generic template.
    #[arg(long, value_enum)]
    pub template: Option<Template>,
}

/// The framework starters `lib/examples/*.rhai` already ships — full sample `Energize.rhai`
/// files, previously only reachable by hand-copying + `nrg vendor`ing `lib/` as a sibling
/// directory (see docs/getting-started.md). `nrg init --template <name>` writes the same
/// content but with its `recipe` import switched to the embedded stdlib (`"std/recipe"`), so
/// the result works with **zero vendoring** — closing the fast-follow noted when 3.2 shipped
/// the embedded stdlib.
#[derive(Clone, Copy, ValueEnum)]
pub enum Template {
    Rails,
    Django,
    Nextjs,
    Phoenix,
    Laravel,
}

impl Template {
    fn source(self) -> &'static str {
        match self {
            Template::Rails => include_str!("../../lib/examples/rails.rhai"),
            Template::Django => include_str!("../../lib/examples/django.rhai"),
            Template::Nextjs => include_str!("../../lib/examples/nextjs.rhai"),
            Template::Phoenix => include_str!("../../lib/examples/phoenix.rhai"),
            Template::Laravel => include_str!("../../lib/examples/laravel.rhai"),
        }
    }

    /// Every `lib/examples/*.rhai` framework starter imports the recipe helper with this
    /// exact line (verified by a unit test below) — swapped for the embedded stdlib's own
    /// import so `nrg init --template` needs no `nrg vendor` step.
    fn rendered(self) -> String {
        self.source().replacen(
            "import \"lib/recipe\" as recipe;",
            "import \"std/recipe\" as recipe;",
            1,
        )
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

pub fn execute(args: &InitArgs) -> i32 {
    if Path::new(DEFAULT_FILE).exists() {
        eprintln!("{} {} already exists.", "Error:".red().bold(), DEFAULT_FILE);
        return 1;
    }

    let content = match args.template {
        Some(t) => t.rendered(),
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

    // Every framework starter must contain the exact on-disk import line `rendered()` replaces
    // — if `lib/examples/*.rhai` ever rewords it, this fails loudly instead of `rendered()`
    // silently no-op'ing and shipping a template that still requires vendoring.
    #[test]
    fn every_template_source_contains_the_exact_recipe_import_line() {
        for t in [
            Template::Rails,
            Template::Django,
            Template::Nextjs,
            Template::Phoenix,
            Template::Laravel,
        ] {
            assert!(
                t.source().contains("import \"lib/recipe\" as recipe;"),
                "template source missing the expected import line to swap"
            );
        }
    }

    #[test]
    fn rendered_swaps_the_recipe_import_to_the_embedded_stdlib() {
        for t in [
            Template::Rails,
            Template::Django,
            Template::Nextjs,
            Template::Phoenix,
            Template::Laravel,
        ] {
            let rendered = t.rendered();
            assert!(rendered.contains("import \"std/recipe\" as recipe;"));
            // Broader than just the recipe import: if a future lib/examples/*.rhai ever grows a
            // SECOND on-disk import (e.g. `import "lib/docker"`), rendered() as written today
            // wouldn't swap it either — this catches that regression instead of only checking
            // the one import name this function actually knows to replace.
            assert!(
                !rendered.contains("import \"lib/"),
                "rendered template still references the on-disk lib/ convention:\n{rendered}"
            );
        }
    }
}
