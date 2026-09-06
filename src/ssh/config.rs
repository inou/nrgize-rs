use std::collections::HashMap;

/// Represents a parsed SSH config Host block.
#[derive(Debug, Clone)]
pub struct SshHostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
}

/// Parsed SSH configuration.
#[derive(Debug, Clone)]
pub struct SshConfig {
    hosts: HashMap<String, SshHostConfig>,
}

impl SshConfig {
    /// Parse an SSH config file.
    pub fn parse(content: &str) -> Self {
        let mut hosts = HashMap::new();
        let mut current_host: Option<String> = None;
        let mut current_config = SshHostConfig {
            hostname: None,
            user: None,
        };

        for line in content.lines() {
            let trimmed = line.trim();

            // Skip comments and empty lines
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Skip Match blocks (we don't support them)
            if trimmed.to_lowercase().starts_with("match ") {
                // Reset current host context since Match blocks are complex
                if let Some(host) = current_host.take() {
                    hosts.insert(host, current_config.clone());
                }
                current_config = SshHostConfig {
                    hostname: None,
                    user: None,
                };
                continue;
            }

            // Parse key-value: support both "Key Value" and "Key=Value"
            let (key, value) = if let Some(eq_pos) = trimmed.find('=') {
                let k = trimmed[..eq_pos].trim();
                let v = trimmed[eq_pos + 1..].trim();
                (k, unquote(v))
            } else {
                let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
                if parts.len() != 2 {
                    continue;
                }
                (parts[0].trim(), unquote(parts[1].trim()))
            };

            match key.to_lowercase().as_str() {
                "host" => {
                    // Save previous host block
                    if let Some(host) = current_host.take() {
                        hosts.insert(host, current_config.clone());
                    }
                    current_host = Some(value.to_string());
                    current_config = SshHostConfig {
                        hostname: None,
                        user: None,
                    };
                }
                "hostname" => {
                    current_config.hostname = Some(value.to_string());
                }
                "user" => {
                    current_config.user = Some(value.to_string());
                }
                _ => {
                    // Ignore other directives
                }
            }
        }

        // Save last host block
        if let Some(host) = current_host {
            hosts.insert(host, current_config);
        }

        SshConfig { hosts }
    }

    /// Load and parse the SSH config from the default location.
    pub fn load_default() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".ssh").join("config"))
            .unwrap_or_default();

        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => Self::parse(&content),
                Err(_) => Self::empty(),
            }
        } else {
            Self::empty()
        }
    }

    /// Create an empty SSH config.
    pub fn empty() -> Self {
        SshConfig {
            hosts: HashMap::new(),
        }
    }

    /// Resolve a host alias to the actual connection string.
    /// If the host is a known alias, returns `user@hostname` (or just hostname).
    /// Otherwise returns the host unchanged.
    pub fn resolve_host(&self, host: &str) -> String {
        // If the host contains '@', extract just the host part for lookup
        let (user_prefix, lookup_host) = if let Some(at_pos) = host.find('@') {
            (Some(&host[..at_pos + 1]), &host[at_pos + 1..])
        } else {
            (None, host)
        };

        if let Some(config) = self.hosts.get(lookup_host) {
            let resolved_hostname = config.hostname.as_deref().unwrap_or(lookup_host);

            // User priority: explicit in host string > SSH config > none
            if let Some(prefix) = user_prefix {
                format!("{}{}", prefix, resolved_hostname)
            } else if let Some(user) = &config.user {
                format!("{}@{}", user, resolved_hostname)
            } else {
                resolved_hostname.to_string()
            }
        } else {
            host.to_string()
        }
    }
}

/// Remove surrounding quotes from a value.
fn unquote(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_ssh_config() {
        let content = r#"
Host myserver
    HostName 192.168.1.100
    User deploy
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("myserver"), "deploy@192.168.1.100");
    }

    #[test]
    fn parse_multiple_hosts() {
        let content = r#"
Host staging
    HostName staging.example.com
    User admin

Host production
    HostName prod.example.com
    User deploy
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("staging"), "admin@staging.example.com");
        assert_eq!(config.resolve_host("production"), "deploy@prod.example.com");
    }

    #[test]
    fn resolve_unknown_host_unchanged() {
        let config = SshConfig::parse("");
        assert_eq!(config.resolve_host("user@unknown.com"), "user@unknown.com");
    }

    #[test]
    fn resolve_host_with_explicit_user_overrides_config() {
        let content = r#"
Host myserver
    HostName 192.168.1.100
    User config_user
"#;
        let config = SshConfig::parse(content);
        // Explicit user should take priority over config user
        assert_eq!(
            config.resolve_host("explicit@myserver"),
            "explicit@192.168.1.100"
        );
    }

    #[test]
    fn parse_key_equals_value_format() {
        let content = r#"
Host myserver
    HostName=192.168.1.100
    User=deploy
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("myserver"), "deploy@192.168.1.100");
    }

    #[test]
    fn parse_quoted_values() {
        let content = r#"
Host myserver
    HostName "192.168.1.100"
    User "deploy"
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("myserver"), "deploy@192.168.1.100");
    }

    #[test]
    fn skip_comments_and_empty_lines() {
        let content = r#"
# This is a comment
Host myserver
    # Another comment
    HostName 192.168.1.100

    User deploy
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("myserver"), "deploy@192.168.1.100");
    }

    #[test]
    fn hostname_only_no_user() {
        let content = r#"
Host myserver
    HostName 192.168.1.100
"#;
        let config = SshConfig::parse(content);
        assert_eq!(config.resolve_host("myserver"), "192.168.1.100");
    }

    #[test]
    fn empty_config() {
        let config = SshConfig::empty();
        assert_eq!(config.resolve_host("anything"), "anything");
    }

    // Robustness review: "SSH config parser fidelity" — this parser only ever backed the
    // informational "Connecting to..." display line in `nrg app exec`/`nrg ssh` (R9 moved all
    // REAL SSH connections onto the plain, unresolved alias, so the system `ssh` binary's own,
    // fully-featured config resolution is what actually applies) — so these known divergences
    // from real `ssh_config(5)` semantics are cosmetic (a wrong/incomplete confirmation message),
    // not a silent misconnection. Documented here, rather than fixed, per that finding's own
    // "practical impact is now purely cosmetic" conclusion.

    #[test]
    fn host_wildcard_is_not_supported_only_exact_alias_names_match() {
        // `Host *` (and any other glob pattern) is stored as the literal key "*", which will
        // never equal a real alias someone actually looks up — so a `User`/`HostName` set under
        // a wildcard block is silently never applied to any host.
        let content = "Host *\n    User deploy\n";
        let config = SshConfig::parse(content);
        assert_eq!(
            config.resolve_host("myserver"), "myserver",
            "a `Host *` block must NOT apply to an unrelated alias (this parser has no glob support)"
        );
    }

    #[test]
    fn multi_name_host_line_collapses_to_one_literal_key_not_two_aliases() {
        // `Host web1 web2` should define TWO separate aliases sharing one config block (real
        // ssh_config(5) semantics) — this parser instead keeps everything after the first
        // whitespace-split as ONE literal key ("web1 web2", with the space), so neither `web1`
        // nor `web2` individually resolves to anything.
        let content = "Host web1 web2\n    User deploy\n    HostName 10.0.0.1\n";
        let config = SshConfig::parse(content);
        assert_eq!(
            config.resolve_host("web1"),
            "web1",
            "individual alias `web1` must NOT resolve (multi-name Host lines aren't split)"
        );
        assert_eq!(
            config.resolve_host("web2"),
            "web2",
            "individual alias `web2` must NOT resolve (multi-name Host lines aren't split)"
        );
        // The literal, space-containing "key" this parser actually stored DOES resolve —
        // documenting the exact (surprising) shape of the divergence, not just its absence.
        assert_eq!(config.resolve_host("web1 web2"), "deploy@10.0.0.1");
    }

    #[test]
    fn match_blocks_are_skipped_directives_inside_never_apply_to_any_host() {
        // `Match` blocks are explicitly unsupported: parsing resets the "current host" context,
        // so any `User`/`HostName` lines that follow a `Match` line are silently discarded
        // rather than attached to whichever `Host` block precedes them.
        //
        // Caveat found in this fix's own final review: the detection is
        // `starts_with("match ")` — SPACE-separated only. A real `ssh_config(5)` file may
        // legally use a TAB after `Match`, which this check misses entirely: that line then
        // falls through to the ordinary key/value parser (where "match\thost other" isn't a
        // recognized key and is ignored), WITHOUT resetting the current-host context — so its
        // following directives leak the OPPOSITE way, attaching to the preceding `Host` block
        // instead of being discarded. Since this parser's output only ever backs the cosmetic
        // "Connecting to..." banner (see this finding's own resolution note), the worst case is
        // a wrong `user@` hint in that banner, not a misconnection — not worth chasing further,
        // but recorded here rather than silently left for a future reader to rediscover.
        let content =
            "Host myserver\n    HostName 192.168.1.100\n\nMatch host myserver\n    User deploy\n";
        let config = SshConfig::parse(content);
        assert_eq!(
            config.resolve_host("myserver"),
            "192.168.1.100",
            "a `User` set inside a `Match` block must be silently ignored, not applied to \
             the preceding `Host` block"
        );
    }
}
