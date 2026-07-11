//! `nrg vendor [--force]` — materialize the embedded stdlib (roadmap 3.2) onto disk as
//! `lib/*.rhai`, for a project that wants to customize a module. NOT required for normal use:
//! `import "std/X"` already works with zero vendoring, resolving from the exact same source this
//! command writes out. `import "lib/X"` only ever reads from disk (never falls back to the
//! embedded copy), so a project switching an import from `"std/X"` to `"lib/X"` needs the file
//! `nrg vendor` writes to actually be there.

use crate::engine::stdlib;
use clap::Args;
use crossterm::style::Stylize;
use std::path::Path;

#[derive(Args)]
pub struct VendorArgs {
    /// Overwrite an existing lib/<name>.rhai instead of refusing (any local customization in it
    /// is lost).
    #[arg(long)]
    pub force: bool,
}

pub fn execute(args: &VendorArgs) -> i32 {
    let dir = Path::new("lib");
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("{} cannot create {}: {e}", "Error:".red().bold(), dir.display());
        return 1;
    }

    let mut wrote = Vec::new();
    let mut skipped = Vec::new();
    for (name, source) in stdlib::embedded_modules() {
        let path = dir.join(format!("{name}.rhai"));
        if path.exists() && !args.force {
            skipped.push(path);
            continue;
        }
        if let Err(e) = std::fs::write(&path, source) {
            eprintln!("{} cannot write {}: {e}", "Error:".red().bold(), path.display());
            return 1;
        }
        wrote.push(path);
    }

    for p in &wrote {
        println!("{} wrote {}", "✓".green(), p.display());
    }
    if !skipped.is_empty() {
        println!(
            "{} skipped {} existing file(s) — pass --force to overwrite:",
            "i".blue(),
            skipped.len()
        );
        for p in &skipped {
            println!("    {}", p.display());
        }
    }
    if wrote.is_empty() && skipped.is_empty() {
        // Unreachable in practice (embedded_modules() is never empty), but no reason to leave a
        // silent 0-file no-op unexplained if it ever were.
        println!("Nothing to vendor.");
    }
    0
}
