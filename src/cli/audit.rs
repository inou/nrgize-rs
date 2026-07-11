//! `nrg audit [filter]` — print the deploy/run audit trail recorded at
//! `<project-root>/.energize/audit.log` by every LIVE `nrg exec`/`nrg run` invocation.

use crate::audit::{self, AuditEntry};
use crate::engine::state;
use clap::Args;
use crossterm::style::Stylize;

#[derive(Args)]
pub struct AuditArgs {
    /// Only show entries whose target, args, or file contain this substring.
    pub filter: Option<String>,

    /// Maximum number of entries to show (most recent first). 0 shows all.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

pub fn execute(args: &AuditArgs) -> i32 {
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let mut entries = audit::read_all(&root);
    if let Some(needle) = &args.filter {
        entries.retain(|e| matches_filter(e, needle));
    }
    entries.reverse(); // most recent first

    if entries.is_empty() {
        println!("No audit history yet — it's written on the first LIVE `nrg exec`/`nrg run`.");
        return 0;
    }

    let shown = if args.limit == 0 { entries.len() } else { args.limit.min(entries.len()) };
    for entry in &entries[..shown] {
        print_entry(entry);
    }
    if shown < entries.len() {
        println!("... {} more (raise --limit to see them)", entries.len() - shown);
    }
    0
}

fn matches_filter(entry: &AuditEntry, needle: &str) -> bool {
    entry.file.contains(needle)
        || entry.target.as_deref().is_some_and(|t| t.contains(needle))
        || entry.args.iter().any(|a| a.contains(needle))
}

fn print_entry(entry: &AuditEntry) {
    let what = match &entry.target {
        Some(t) => format!("run {t} {}", entry.args.join(" ")),
        // `nrg exec` has no `target`, but MAY still have args (e.g. `--dest=<name>` — see
        // `execute_with` in cli/exec.rs) — these must still be shown, or a fact the audit trail
        // exists to record (which destination a run used) is captured in the JSON log but never
        // surfaced by the one command operators actually read (Fable's final review, round 7).
        None if entry.args.is_empty() => format!("exec {}", entry.file),
        None => format!("exec {} {}", entry.file, entry.args.join(" ")),
    };
    let outcome = if entry.outcome == "success" {
        entry.outcome.clone().green().to_string()
    } else {
        entry.outcome.clone().red().to_string()
    };
    println!(
        "{}  {}@{}  {:<50}  {}",
        entry.ts,
        entry.user,
        entry.host,
        what.trim(),
        outcome
    );
}
