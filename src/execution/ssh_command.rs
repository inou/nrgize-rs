use std::collections::HashMap;
use crate::ssh::config::SshConfig;

const LOCAL_HOSTS: &[&str] = &["127.0.0.1", "localhost", "local"];

/// Check if a host string refers to the local machine.
pub fn is_local_host(host: &str) -> bool {
    let host_part = host.split('@').last().unwrap_or(host);
    LOCAL_HOSTS.contains(&host_part)
}

/// Build the shell script that will be executed on the remote (or locally).
/// Includes variable exports, set -e, debug trap, and the task script.
pub fn build_script(
    script: &str,
    env: &HashMap<String, String>,
    host: &str,
) -> String {
    let mut parts = Vec::new();

    // Export environment variables
    for (key, value) in env {
        parts.push(format!("export {}=\"{}\"", key, escape_value(value)));
    }

    // Auto-set NRG_HOST
    parts.push(format!("export NRG_HOST=\"{}\"", escape_value(host)));

    // Strict error handling
    parts.push("set -e".to_string());

    // Debug trap for command tracing
    parts.push(
        r#"trap 'echo "NRG_TRACE:$BASH_COMMAND" >&2' DEBUG"#.to_string(),
    );

    // The actual script
    parts.push(script.to_string());

    parts.join("\n")
}

/// Build the full SSH command string for remote execution.
pub fn build_ssh_command(
    host: &str,
    script: &str,
    env: &HashMap<String, String>,
    ssh_config: &SshConfig,
) -> String {
    let full_script = build_script(script, env, host);

    if is_local_host(host) {
        // Local execution: just the script wrapped in bash
        format!("bash -se << \\EOF-NRG\n{}\nEOF-NRG", full_script)
    } else {
        let resolved = ssh_config.resolve_host(host);
        format!(
            "ssh {} 'bash -se' << \\EOF-NRG\n{}\nEOF-NRG",
            resolved, full_script
        )
    }
}

/// Build a tokio Command for execution.
pub fn build_process(
    host: &str,
    script: &str,
    env: &HashMap<String, String>,
    ssh_config: &SshConfig,
) -> tokio::process::Command {
    let _full_script = build_script(script, env, host);

    if is_local_host(host) {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-se");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // We'll write the script to stdin
        cmd
    } else {
        let resolved = ssh_config.resolve_host(host);
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg(&resolved);
        cmd.arg("bash -se");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd
    }
}

/// Escape a value for safe inclusion in a double-quoted shell string.
fn escape_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_local_host_checks() {
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("localhost"));
        assert!(is_local_host("local"));
        assert!(is_local_host("user@127.0.0.1"));
        assert!(!is_local_host("remote.example.com"));
        assert!(!is_local_host("user@remote.example.com"));
    }

    #[test]
    fn build_script_includes_env_vars() {
        let mut env = HashMap::new();
        env.insert("APP_ENV".into(), "production".into());
        env.insert("BRANCH".into(), "main".into());

        let script = build_script("echo hello", &env, "prod.example.com");

        assert!(script.contains("export APP_ENV=\"production\""));
        assert!(script.contains("export BRANCH=\"main\""));
        assert!(script.contains("export NRG_HOST=\"prod.example.com\""));
        assert!(script.contains("set -e"));
        assert!(script.contains("NRG_TRACE"));
        assert!(script.contains("echo hello"));
    }

    #[test]
    fn build_ssh_command_remote() {
        let env = HashMap::new();
        let ssh_config = SshConfig::empty();

        let cmd = build_ssh_command(
            "user@prod.example.com",
            "echo deploy",
            &env,
            &ssh_config,
        );

        assert!(cmd.starts_with("ssh user@prod.example.com"));
        assert!(cmd.contains("EOF-NRG"));
        assert!(cmd.contains("echo deploy"));
    }

    #[test]
    fn build_ssh_command_local() {
        let env = HashMap::new();
        let ssh_config = SshConfig::empty();

        let cmd = build_ssh_command("127.0.0.1", "echo local", &env, &ssh_config);

        assert!(cmd.starts_with("bash -se"));
        assert!(!cmd.contains("ssh"));
        assert!(cmd.contains("echo local"));
    }

    #[test]
    fn build_ssh_command_resolves_alias() {
        let ssh_config = SshConfig::parse(
            "Host myserver\n    HostName 192.168.1.100\n    User deploy\n",
        );
        let env = HashMap::new();

        let cmd = build_ssh_command("myserver", "echo hi", &env, &ssh_config);
        assert!(cmd.contains("ssh deploy@192.168.1.100"));
    }

    #[test]
    fn escape_value_handles_special_chars() {
        assert_eq!(escape_value(r#"hello "world""#), r#"hello \"world\""#);
        assert_eq!(escape_value("$HOME"), "\\$HOME");
        assert_eq!(escape_value("`cmd`"), "\\`cmd\\`");
    }
}
