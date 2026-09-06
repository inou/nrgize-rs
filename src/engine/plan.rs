//! The dry-run plan log: a record of the side effects a run WOULD perform.

/// One side-effecting action that dry-run recorded instead of executing.
#[derive(Debug, Clone)]
pub struct PlannedAction {
    /// Short kind tag: "local", "ssh", "ssh-all", "ssh-stdin", "local-stdin", "write",
    /// "state", "check", "rollback".
    pub kind: String,
    /// Host(s) the action targets, if any.
    pub host: Option<String>,
    /// Human-readable detail (command / key=value). Redacted centrally by `RunCtx::record`.
    pub detail: String,
}

/// Render the plan as a human-readable block (caller has already redacted details).
pub fn render_plan(actions: &[PlannedAction]) -> String {
    use std::collections::BTreeSet;
    let mut out = String::from("\nPLAN (dry run — no changes made):\n");
    if actions.is_empty() {
        out.push_str("  (no side effects)\n");
    }
    for a in actions {
        let host = a.host.as_deref().unwrap_or("-");
        out.push_str(&format!("  {:<7} {:<22} {}\n", a.kind, host, a.detail));
    }
    let hosts: BTreeSet<&str> = actions.iter().filter_map(|a| a.host.as_deref()).collect();
    out.push_str(&format!(
        "{} action(s), {} host(s). 0 executed.\n",
        actions.len(),
        hosts.len()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_summarizes_actions_and_hosts() {
        let actions = vec![
            PlannedAction {
                kind: "local".into(),
                host: None,
                detail: "docker build".into(),
            },
            PlannedAction {
                kind: "ssh".into(),
                host: Some("a".into()),
                detail: "docker pull".into(),
            },
            PlannedAction {
                kind: "ssh".into(),
                host: Some("b".into()),
                detail: "docker pull".into(),
            },
        ];
        let r = render_plan(&actions);
        assert!(r.contains("docker build"));
        assert!(r.contains("3 action(s), 2 host(s). 0 executed."));
    }

    #[test]
    fn render_empty_plan() {
        assert!(render_plan(&[]).contains("0 action(s), 0 host(s). 0 executed."));
    }
}
