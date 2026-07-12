//! `nrg vendor [--force]` — materialize the embedded stdlib (roadmap 3.2) onto disk as
//! `lib/*.rhai`, for a project that wants to customize a module. NOT required for normal use:
//! `import "std/X"` already works with zero vendoring, resolving from the exact same source this
//! command writes out. `import "lib/X"` only ever reads from disk (never falls back to the
//! embedded copy), so a project switching an import from `"std/X"` to `"lib/X"` needs the file
//! `nrg vendor` writes to actually be there.

use crate::engine::{state, stdlib};
use clap::Args;
use crossterm::style::Stylize;

#[derive(Args)]
pub struct VendorArgs {
    /// Overwrite an existing lib/<name>.rhai instead of refusing (any local customization in it
    /// is lost).
    #[arg(long)]
    pub force: bool,
}

pub fn execute(args: &VendorArgs) -> i32 {
    // Anchor at the project root, same as `nrg exec`/`nrg run`/`nrg rollback` — otherwise
    // running `nrg vendor` from a subdirectory writes `lib/` somewhere those commands' own
    // `import "lib/X"` resolution never looks, so the freshly vendored files are silently unused.
    let root = match state::find_project_root() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("{} {e}", "Error:".red().bold());
            return 1;
        }
    };
    let dir = root.join("lib");
    if let Err(e) = std::fs::create_dir_all(&dir) {
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
        // Write to a temp file in the same directory then rename, so an interrupted vendor
        // (Ctrl-C, disk full mid-write) never leaves a truncated lib/<name>.rhai that a re-run
        // would then refuse to repair (skip-if-exists treats a truncated file as "already
        // vendored").
        let tmp_path = dir.join(format!("{name}.rhai.tmp"));
        if let Err(e) = std::fs::write(&tmp_path, source) {
            let _ = std::fs::remove_file(&tmp_path);
            eprintln!("{} cannot write {}: {e}", "Error:".red().bold(), path.display());
            return 1;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
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
