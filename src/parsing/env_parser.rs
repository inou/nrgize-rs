use std::collections::HashMap;
use std::path::Path;

/// Parse a .env file into a HashMap of key-value pairs.
///
/// Supports:
/// - `KEY=VALUE`
/// - `KEY="VALUE"` (double-quoted, with \n escape)
/// - `KEY='VALUE'` (single-quoted, literal)
/// - `export KEY=VALUE` (export prefix stripped)
/// - `#` comments and blank lines
pub fn parse_env_file(path: &Path) -> Result<HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read env file '{}': {}", path.display(), e))?;
    Ok(parse_env_content(&content))
}

/// Parse .env content string into key-value pairs.
pub fn parse_env_content(content: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip comments and blank lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Strip optional `export ` prefix
        let trimmed = if let Some(rest) = trimmed.strip_prefix("export ") {
            rest.trim()
        } else {
            trimmed
        };

        // Split on first `=`
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim().to_string();
            let value = parse_env_value(value.trim());
            if !key.is_empty() {
                vars.insert(key, value);
            }
        }
    }

    vars
}

/// Parse the value part of a KEY=VALUE line, handling quoting.
fn parse_env_value(s: &str) -> String {
    if s.len() >= 2 {
        // Double-quoted: process escape sequences
        if s.starts_with('"') && s.ends_with('"') {
            return s[1..s.len() - 1]
                .replace("\\n", "\n")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
        }
        // Single-quoted: literal (no escapes)
        if s.starts_with('\'') && s.ends_with('\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    // Unquoted: take as-is
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_vars() {
        let content = "FOO=bar\nBAZ=qux\n";
        let vars = parse_env_content(content);
        assert_eq!(vars.get("FOO").unwrap(), "bar");
        assert_eq!(vars.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_double_quoted() {
        let content = r#"DATABASE_URL="postgres://user:pass@host/db""#;
        let vars = parse_env_content(content);
        assert_eq!(
            vars.get("DATABASE_URL").unwrap(),
            "postgres://user:pass@host/db"
        );
    }

    #[test]
    fn parse_single_quoted() {
        let content = "SECRET='don\\'t escape \\n here'";
        let vars = parse_env_content(content);
        // Single-quoted: no escaping
        assert_eq!(
            vars.get("SECRET").unwrap(),
            "don\\'t escape \\n here"
        );
    }

    #[test]
    fn parse_double_quoted_escapes() {
        let content = r#"MSG="hello\nworld""#;
        let vars = parse_env_content(content);
        assert_eq!(vars.get("MSG").unwrap(), "hello\nworld");
    }

    #[test]
    fn parse_export_prefix() {
        let content = "export APP_ENV=production\nexport PORT=3000\n";
        let vars = parse_env_content(content);
        assert_eq!(vars.get("APP_ENV").unwrap(), "production");
        assert_eq!(vars.get("PORT").unwrap(), "3000");
    }

    #[test]
    fn parse_comments_and_blanks() {
        let content = "# This is a comment\n\nFOO=bar\n# Another comment\nBAZ=qux\n";
        let vars = parse_env_content(content);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn parse_value_with_equals() {
        let content = "DATABASE_URL=postgres://user:pass@host/db?sslmode=require";
        let vars = parse_env_content(content);
        assert_eq!(
            vars.get("DATABASE_URL").unwrap(),
            "postgres://user:pass@host/db?sslmode=require"
        );
    }

    #[test]
    fn parse_empty_value() {
        let content = "EMPTY=\nALSO_EMPTY=\"\"\n";
        let vars = parse_env_content(content);
        assert_eq!(vars.get("EMPTY").unwrap(), "");
        assert_eq!(vars.get("ALSO_EMPTY").unwrap(), "");
    }

    #[test]
    fn missing_file_returns_error() {
        let result = parse_env_file(Path::new("/nonexistent/.env"));
        assert!(result.is_err());
    }
}
