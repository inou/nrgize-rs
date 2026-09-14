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
    /// Emit structured JSON instead of terminal text.
    #[arg(long)]
    pub json: bool,
    /// Include successful steps in the terminal view.
    #[arg(long)]
    pub verbose: bool,
    /// Show journal events for this run ID.
    #[arg(long, conflicts_with = "incomplete")]
    pub run: Option<String>,
    /// Show runs with no completion event (possibly still running).
    #[arg(long)]
    pub incomplete: bool,
}

pub fn execute(args: &AuditArgs) -> i32 {
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    if args.incomplete || args.run.is_some() {
        let mut events = crate::engine::journal::read(&root);
        let complete: std::collections::HashSet<_> = events
            .iter()
            .filter(|e| e.event == "run_finished")
            .map(|e| e.run_id.clone())
            .collect();
        events.retain(|e| {
            args.run.as_ref().is_none_or(|id| &e.run_id == id)
                && (!args.incomplete || !complete.contains(&e.run_id))
        });
        if let Some(needle) = &args.filter {
            events.retain(|e| {
                e.run_id.contains(needle)
                    || e.step.as_ref().is_some_and(|s| {
                        s.name.contains(needle)
                            || s.host.contains(needle)
                            || s.operation.contains(needle)
                    })
            });
        }
        if args.limit > 0 && events.len() > args.limit {
            events.drain(..events.len() - args.limit);
        }
        if args.json {
            println!("{}", serde_json::to_string_pretty(&events).unwrap());
        } else {
            if args.incomplete {
                println!("Runs without a completion event may still be running. Inspect before retrying.");
            }
            for e in events {
                let detail = e
                    .step
                    .as_ref()
                    .map(|s| {
                        if e.event == "step_started" {
                            format!(
                                "{} on {} ({}) — execution pending",
                                s.name, s.host, s.operation
                            )
                        } else {
                            format!(
                                "{} on {} ({}) exited {}: {}",
                                s.name,
                                s.host,
                                s.operation,
                                s.exit_code
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "unknown".into()),
                                s.excerpt
                            )
                        }
                    })
                    .unwrap_or_default();
                println!(
                    "{} {} {} {}",
                    display_safe(&e.run_id),
                    e.timestamp_ms,
                    display_safe(&e.event),
                    display_safe(&detail)
                );
            }
        }
        return 0;
    }
    let mut entries = audit::read_all(&root);
    if let Some(needle) = &args.filter {
        entries.retain(|e| matches_filter(e, needle));
    }
    entries.reverse(); // most recent first

    if args.json {
        if args.limit > 0 {
            entries.truncate(args.limit);
        }
        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        return 0;
    }
    if entries.is_empty() {
        println!("No audit history yet — it's written on the first LIVE `nrg exec`/`nrg run`.");
        return 0;
    }

    let shown = if args.limit == 0 {
        entries.len()
    } else {
        args.limit.min(entries.len())
    };
    for entry in &entries[..shown] {
        print_entry(entry);
        if let Some(id) = &entry.run_id {
            println!("  run: {}", display_safe(id));
        }
        for step in entry
            .steps
            .iter()
            .filter(|s| args.verbose || s.exit_code != 0)
        {
            println!(
                "  step {} on {} ({}) exited {}: {}",
                display_safe(&step.name),
                if step.host.is_empty() {
                    "local".into()
                } else {
                    display_safe(&step.host)
                },
                display_safe(&step.operation),
                step.exit_code,
                display_safe(&step.excerpt)
            );
        }
    }
    if shown < entries.len() {
        println!(
            "... {} more (raise --limit to see them)",
            entries.len() - shown
        );
    }
    0
}

fn matches_filter(entry: &AuditEntry, needle: &str) -> bool {
    entry.run_id.as_ref().is_some_and(|id| id.contains(needle))
        || entry.steps.iter().any(|s| {
            s.name.contains(needle)
                || s.host.contains(needle)
                || s.operation.contains(needle)
                || s.excerpt.contains(needle)
        })
        || entry.file.contains(needle)
        || entry.target.as_deref().is_some_and(|t| t.contains(needle))
        || entry.args.iter().any(|a| a.contains(needle))
}

fn print_entry(entry: &AuditEntry) {
    let what = match &entry.target {
        Some(t) => format!("{} {t} {}", entry.command, entry.args.join(" ")),
        // `nrg exec` has no `target`, but MAY still have args (e.g. `--dest=<name>` — see
        // `execute_with` in cli/exec.rs) — these must still be shown, or a fact the audit trail
        // exists to record (which destination a run used) is captured in the JSON log but never
        // surfaced by the one command operators actually read (Fable's final review, round 7).
        None if entry.args.is_empty() => format!("{} {}", entry.command, entry.file),
        None => format!("{} {} {}", entry.command, entry.file, entry.args.join(" ")),
    };
    let what = display_safe(&what);
    let outcome = if entry.outcome == "success" {
        display_safe(&entry.outcome).green().to_string()
    } else {
        display_safe(&entry.outcome).red().to_string()
    };
    println!(
        "{}  {}@{}  {:<50}  {}",
        display_safe(&entry.ts),
        display_safe(&entry.user),
        display_safe(&entry.host),
        what.trim(),
        outcome
    );
}

/// Neutralize terminal-control characters in an audit field before it is printed.
///
/// Audit fields carry bytes an attacker can influence: `outcome` folds in the stderr of remote
/// commands (a thrown Rhai error — see `execute_with` in cli/exec.rs), and `target`/`args` come
/// from whoever typed the earlier invocation. `audit::append` stores them JSON-escaped, so the
/// log FILE is inert, but `audit::read_all` decodes them back into raw bytes; printing those
/// verbatim would let a compromised deploy host emit CR/erase/cursor sequences that overwrite or
/// hide neighbouring entries, or SGR sequences that repaint a `failed:` outcome as a success —
/// forging the one view operators read to answer "who deployed what". Escaping is done HERE, at
/// display time only: the stored record stays byte-faithful (an investigator can still see
/// exactly what the host sent), and entries written by other/older writers are covered too,
/// which a write-time filter alone could not do.
///
/// Only C0/C1 controls, DEL and the bidi override/isolate class are escaped (as `\u{..}`);
/// everything else passes through untouched, so UTF-8 names, IDN hostnames, CJK paths and emoji
/// still render as themselves. (`char::escape_debug` is NOT usable for this: it also escapes
/// `"`, `'`, `\` and leading combining marks, mangling ordinary entries.)
pub(crate) fn display_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // `is_control` is exactly the Cc category: C0 (incl. ESC, CR, BS), DEL, and C1 (incl.
        // the 8-bit CSI U+009B, which some terminals honour just like `ESC [`).
        if c.is_control() || is_bidi_control(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Bidirectional-formatting characters. Not `char::is_control`, but they reorder the rest of the
/// line on a conforming terminal — the same "renders as something other than what was recorded"
/// problem the control escapes above close.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_safe_escapes_control_sequences_used_to_forge_entries() {
        // The exploit shape: CR + erase-line, then a fabricated "success" entry.
        let forged =
            "failed: boom\r\u{1b}[2K2026-07-25T10:00:00Z  alice@web1  run deploy v9  success";
        let safe = display_safe(forged);
        assert!(!safe.contains('\r'), "CR must not survive: {safe:?}");
        assert!(!safe.contains('\u{1b}'), "ESC must not survive: {safe:?}");
        assert_eq!(
            safe,
            "failed: boom\\u{d}\\u{1b}[2K2026-07-25T10:00:00Z  alice@web1  run deploy v9  success"
        );
    }

    #[test]
    fn display_safe_escapes_c1_del_and_bidi_overrides() {
        assert_eq!(display_safe("a\u{7f}b"), "a\\u{7f}b"); // DEL
        assert_eq!(display_safe("a\u{9b}b"), "a\\u{9b}b"); // 8-bit CSI
        assert_eq!(display_safe("a\u{202e}b"), "a\\u{202e}b"); // RIGHT-TO-LEFT OVERRIDE
        assert_eq!(display_safe("a\u{2069}b"), "a\\u{2069}b"); // POP DIRECTIONAL ISOLATE
        assert_eq!(display_safe("a\0b\tc\nd"), "a\\u{0}b\\u{9}c\\u{a}d");
    }

    #[test]
    fn display_safe_leaves_legitimate_text_untouched() {
        // Non-ASCII is not the enemy: operator names, IDN hostnames, CJK paths, emoji and
        // quoting/backslashes in args must all render exactly as recorded.
        for s in [
            "failed: Pre-deploy release command failed on web1",
            "Zoë@münchen.example",
            "run deploy 東京/路径 🚀 v42",
            "e\u{301}cole", // decomposed é — `escape_debug` would mangle this
            r#"exec Energize.rhai --flag="a b" 'c' C:\tmp"#,
            "success",
        ] {
            assert_eq!(display_safe(s), s, "must pass through unchanged: {s:?}");
        }
    }
}
