//! `nrg doctor` — sanity checks: the orchestration file compiles, and the external tools
//! the stdlib shells out to are on `PATH`.

use crate::cli::exec::resolve_file;
use crate::engine::eval;
use clap::Args;
use crossterm::style::Stylize;

#[derive(Args)]
pub struct DoctorArgs {
    /// Path to the `.rhai` file. Defaults to Energize.rhai.
    #[arg(long)]
    pub file: Option<String>,
}

pub fn execute(args: &DoctorArgs) -> i32 {
    println!("\n{}\n", "Energize Doctor".bold());

    let mut all_ok = true;

    // Check 1: the orchestration file exists and compiles (parse-time validation — Rhai is
    // dynamically typed, so this catches syntax errors, not runtime/config errors).
    match resolve_file(&args.file, "").ok() {
        Some(path) => {
            check_pass(&format!("Orchestration file found: {}", path));
            match eval::list_functions(std::path::Path::new(&path)) {
                Ok(fns) => {
                    check_pass(&format!(
                        "{} compiles ({} function(s) defined)",
                        path,
                        fns.len()
                    ));
                }
                Err(e) => {
                    check_fail(&e);
                    all_ok = false;
                }
            }
        }
        None => {
            check_fail("No Energize.rhai found (run `nrg init`).");
            all_ok = false;
        }
    }

    // Check 2: external tools the stdlib relies on.
    println!("\n  {}", "Tools:".bold());
    let required = ["age", "ssh"];
    for tool in required {
        if tool_available(tool) {
            check_pass(&format!("{} found", tool));
        } else {
            check_fail(&format!("{} not found on PATH", tool));
            all_ok = false;
        }
    }
    // At least one tool from each of these groups is enough.
    check_group(&mut all_ok, "file transfer", &["rsync", "scp"]);
    check_group(&mut all_ok, "container runtime", &["docker", "podman"]);

    println!();

    if all_ok {
        println!("{} All checks passed!", "✓".green());
        0
    } else {
        println!("{} Some checks failed.", "⚠".yellow());
        1
    }
}

/// Pass if any tool in the group is available; otherwise fail and flip `all_ok`.
fn check_group(all_ok: &mut bool, label: &str, tools: &[&str]) {
    let found: Vec<&str> = tools.iter().copied().filter(|t| tool_available(t)).collect();
    if found.is_empty() {
        check_fail(&format!("{}: none of {} found on PATH", label, tools.join("/")));
        *all_ok = false;
    } else {
        check_pass(&format!("{}: {} found", label, found.join(", ")));
    }
}

/// Whether `tool` is resolvable on `PATH` (via `command -v`).
fn tool_available(tool: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", tool))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_pass(msg: &str) {
    println!("  {} {}", "✓".green(), msg);
}

fn check_fail(msg: &str) {
    println!("  {} {}", "✗".red(), msg);
}
