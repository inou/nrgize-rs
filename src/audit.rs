//! Append-only audit trail for mutating `nrg exec`/`nrg run` invocations.
//!
//! Every LIVE (non-dry-run) invocation appends one JSON line to
//! `<project-root>/.energize/audit.log` recording who ran what, from where, and whether it
//! succeeded — so a team can answer "who deployed what, when" without digging through shell
//! history. Best-effort: a failure to write the audit log never aborts the run (it is an
//! observability aid, not a correctness gate). Callers MUST redact secrets out of `target`/
//! `args`/`outcome` before constructing an entry (the same boundary `RunCtx::record` uses for
//! the dry-run plan) — this module has no access to the registered-secrets set.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    pub ts: String,
    pub user: String,
    pub host: String,
    pub cwd: String,
    pub command: String,
    pub file: String,
    pub target: Option<String>,
    pub args: Vec<String>,
    pub outcome: String,
}

impl AuditEntry {
    pub fn new(command: &str, file: &str, target: Option<&str>, args: &[String], outcome: String) -> Self {
        AuditEntry {
            ts: now_iso(),
            user: current_user(),
            host: current_host(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string()),
            command: command.to_string(),
            file: file.to_string(),
            target: target.map(|s| s.to_string()),
            args: args.to_vec(),
            outcome,
        }
    }
}

fn audit_path(root: &Path) -> PathBuf {
    root.join(".energize").join("audit.log")
}

/// Append `entry` as one JSON line. Best-effort: errors are swallowed (the run's own exit code
/// must reflect the DEPLOY outcome, not whether we could write a log file).
pub fn append(root: &Path, entry: &AuditEntry) {
    let dir = root.join(".energize");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string(entry) else { return };
    let path = audit_path(root);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{json}");
        set_owner_only(&path); // audit args may echo operator-supplied values; keep it private
    }
}

/// Read every parseable entry, oldest first. A line that fails to parse (hand-edited, or a
/// torn write from a crash mid-append) is skipped rather than making the whole history
/// unreadable.
pub fn read_all(root: &Path) -> Vec<AuditEntry> {
    let Ok(content) = fs::read_to_string(audit_path(root)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn set_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// UTC timestamp via the `date` binary (mirrors `lib/deploy.rhai`'s own `timestamp()`, which
/// shells out to `date -u` rather than pulling in a date/time dependency). Falls back to a raw
/// epoch-seconds marker if `date` is unavailable.
fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("epoch:{secs}")
        })
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn current_host() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_all_round_trips_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let e1 = AuditEntry::new("run", "Energize.rhai", Some("deploy"), &["v42".to_string()], "success".to_string());
        append(tmp.path(), &e1);
        let e2 = AuditEntry::new("exec", "Energize.rhai", None, &[], "failed: boom".to_string());
        append(tmp.path(), &e2);

        let entries = read_all(tmp.path());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "run");
        assert_eq!(entries[0].target.as_deref(), Some("deploy"));
        assert_eq!(entries[0].args, vec!["v42".to_string()]);
        assert_eq!(entries[1].command, "exec");
        assert_eq!(entries[1].outcome, "failed: boom");
    }

    #[test]
    fn read_all_skips_unparseable_lines_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        fs::write(
            tmp.path().join(".energize/audit.log"),
            "not json\n{\"ts\":\"x\",\"user\":\"u\",\"host\":\"h\",\"cwd\":\"c\",\"command\":\"exec\",\"file\":\"f\",\"target\":null,\"args\":[],\"outcome\":\"success\"}\n",
        )
        .unwrap();
        let entries = read_all(tmp.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "exec");
    }

    #[test]
    fn read_all_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_all(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn audit_log_is_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        append(
            tmp.path(),
            &AuditEntry::new("exec", "Energize.rhai", None, &[], "success".to_string()),
        );
        let mode = fs::metadata(tmp.path().join(".energize/audit.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "audit.log must be owner-only");
    }
}
