use crate::secrets;
use clap::{Args, Subcommand};
use crossterm::style::Stylize;

/// Print an error to stderr with a red `Error:` prefix.
fn render_error(message: &str) {
    eprintln!("{} {}", "Error:".red().bold(), message);
}

#[derive(Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub command: SecretsCommand,
}

#[derive(Subcommand)]
pub enum SecretsCommand {
    /// Generate a new age key pair (.nrg-key and .nrg-key.pub)
    Init,

    /// Encrypt a value from stdin and store it in .energize/secrets[.DEST]
    Encrypt {
        /// The secret's name, as passed to `secret("KEY")`.
        key: String,

        /// Store it in .energize/secrets.DEST (used only with `--dest DEST`) instead of the shared
        /// .energize/secrets.
        #[arg(long)]
        dest: Option<String>,

        /// Print the ENC[...] token on stdout and write no file.
        #[arg(long)]
        print: bool,
    },

    /// Decrypt a single ENC[...] token
    Decrypt {
        /// The encrypted token (ENC[...]). Omit to read it from stdin instead (recommended —
        /// keeps it off argv/`ps`/shell history).
        token: Option<String>,
    },
}

pub fn execute(args: &SecretsArgs) -> i32 {
    match &args.command {
        SecretsCommand::Init => cmd_init(),
        SecretsCommand::Encrypt { key, dest, print } => cmd_encrypt(key, dest.as_deref(), *print),
        SecretsCommand::Decrypt { token } => cmd_decrypt(token.as_deref()),
    }
}

/// Read all of stdin and strip exactly one trailing line ending (`\n` or `\r\n`) — the shape a
/// pipe or heredoc naturally produces — without touching any OTHER whitespace the caller's value
/// might genuinely contain (this keeps a secret value off argv/`ps`/shell
/// history by letting it come from stdin instead).
fn read_stdin_value(what: &str) -> Result<String, i32> {
    use std::io::Read;
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        render_error(&format!("Failed to read the {what} from stdin: {e}"));
        return Err(1);
    }
    if let Some(s) = buf.strip_suffix('\n') {
        buf = s.strip_suffix('\r').unwrap_or(s).to_string();
    }
    if buf.is_empty() {
        render_error(&format!("No {what} given."));
        return Err(1);
    }
    Ok(buf)
}

fn cmd_init() -> i32 {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    match secrets::generate_key_pair(&dir) {
        Ok((key_path, pubkey_path)) => {
            println!("  {} Generated key pair:", "✓".green());
            println!("    Private key: {}", key_path.display());
            println!("    Public key:  {}", pubkey_path.display());
            println!();
            // If we're inside a git work tree and `.nrg-key` isn't already ignored, warn loudly:
            // committing the unpassphrased identity would expose every encrypted secret (#14).
            if in_git_worktree(&dir) && !gitignore_covers_key(&dir) {
                println!(
                    "  {} {} is NOT in .gitignore and you're in a git repo — committing it would \
                     leak your private key. Add this line to .gitignore now:",
                    "⚠".yellow().bold(),
                    ".nrg-key".bold()
                );
                println!("      .nrg-key");
            } else {
                println!(
                    "  {} Make sure {} is in your .gitignore!",
                    "⚠".yellow(),
                    ".nrg-key".bold()
                );
            }
            println!("    The public key (.nrg-key.pub) is safe to commit.");
            if secrets_file_ignored(&dir) {
                println!(
                    "  {} .energize/secrets is git-ignored; it holds only ciphertext and is meant \
                     to be committed. Use `.energize/*` with `!.energize/secrets` and \
                     `!.energize/secrets.*` in .gitignore.",
                    "⚠".yellow().bold()
                );
            }
            println!("  Add a secret: printf %s VALUE | nrg secrets encrypt KEY [--dest D]");
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}

/// Whether `dir` (or an ancestor) contains a `.git` — i.e. we're inside a git work tree where a
/// stray commit could leak the key.
fn in_git_worktree(dir: &std::path::Path) -> bool {
    let mut d = dir.to_path_buf();
    loop {
        if d.join(".git").exists() {
            return true;
        }
        if !d.pop() {
            return false;
        }
    }
}

/// Whether `.nrg-key` in `dir` is already git-ignored. Asks git first (`git check-ignore`, which
/// honours parent `.gitignore` files, `.git/info/exclude` and the global excludes file); when git
/// is unavailable or `dir` is not a work tree, falls back to an exact-line match in `dir`'s own
/// `.gitignore` (comments/blank lines ignored).
fn gitignore_covers_key(dir: &std::path::Path) -> bool {
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", ".nrg-key"])
        .output()
    {
        match out.status.code() {
            Some(0) => return true,
            Some(1) => return false,
            _ => {} // 128 etc.: not a work tree / git error — use the file-based fallback
        }
    }
    std::fs::read_to_string(dir.join(".gitignore"))
        .map(|c| {
            c.lines()
                .map(|l| l.trim())
                .any(|l| l == ".nrg-key" || l == "/.nrg-key" || l == "*.nrg-key")
        })
        .unwrap_or(false)
}

/// Whether git ignores `.energize/secrets` in `dir`. Asks git only: outside a work tree (or
/// without git) there is nothing to warn about.
fn secrets_file_ignored(dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", ".energize/secrets"])
        .output()
        .is_ok_and(|o| o.status.code() == Some(0))
}

/// Resolve the public key file or print the standard error and return `Err(1)` (issue #24).
fn require_pubkey() -> Result<std::path::PathBuf, i32> {
    let pubkey = match secrets::find_pubkey_file() {
        Ok(Some(p)) => p,
        Ok(None) => {
            render_error("No public key found (.nrg-key.pub). Run 'nrg secrets init' first.");
            return Err(1);
        }
        // A key file was found but refused as untrusted (not ours / writable by others) — say so
        // instead of pretending none exists, and never fall back to a different key.
        Err(e) => {
            render_error(&e);
            return Err(1);
        }
    };
    // Name the recipient this encrypt is actually encrypting to, so a substituted
    // `.nrg-key.pub` is visible rather than silent. On stderr, not stdout: `nrg secrets encrypt`
    // prints the ENC[...] token on stdout for the user to copy or pipe.
    eprintln!("  Encrypting to public key: {}", pubkey.display());
    Ok(pubkey)
}

/// Resolve the private key file or print the standard error and return `Err(1)`.
fn require_key(action: &str) -> Result<std::path::PathBuf, i32> {
    match secrets::find_key_file_checked() {
        Ok(Some(p)) => Ok(p),
        Ok(None) => {
            render_error(&format!(
                "No private key found (.nrg-key). Cannot {action}."
            ));
            Err(1)
        }
        Err(e) => {
            render_error(&e);
            Err(1)
        }
    }
}

fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Set `KEY=line_value` in `content`: replace the first `KEY=` line in place, else append.
/// Comments and order are kept, and the result always ends with a newline.
fn upsert_kv_line(content: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key}={value}");
    let mut replaced = false;
    let mut out: Vec<&str> = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        let is_key = !replaced
            && !t.starts_with('#')
            && t.split_once('=').is_some_and(|(k, _)| k.trim() == key);
        if is_key {
            replaced = true;
            out.push(&new_line);
        } else {
            out.push(line);
        }
    }
    if !replaced {
        out.push(&new_line);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| format!("cannot create a temporary file in {}: {e}", dir.display()))?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    secrets::set_mode(tmp.path(), 0o644)?;
    tmp.persist(path)
        .map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(())
}

fn cmd_encrypt(key: &str, dest: Option<&str>, print: bool) -> i32 {
    if !valid_secret_name(key) {
        render_error("invalid secret name: use letters, digits and '_' (the name is not echoed)");
        return 1;
    }
    if let Some(d) = dest {
        if !crate::engine::state::is_valid_dest_name(d) {
            render_error(&format!(
                "invalid --dest {d:?}: must be non-empty and contain only letters, digits, '-', \
                 or '_'"
            ));
            return 1;
        }
    }
    let pubkey = match require_pubkey() {
        Ok(p) => p,
        Err(code) => return code,
    };
    {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            eprint!("Value for {key} (end with Ctrl-D): ");
        }
    }
    let value = match read_stdin_value("value to encrypt") {
        Ok(v) => v,
        Err(code) => return code,
    };
    let token = match secrets::encrypt_value(&value, &pubkey) {
        Ok(t) => t,
        Err(e) => {
            render_error(&e);
            return 1;
        }
    };
    if print {
        println!("{token}");
        return 0;
    }

    let root = crate::engine::state::find_project_root()
        .or_else(|_| std::env::current_dir().map_err(|e| e.to_string()));
    let root = match root {
        Ok(r) => r,
        Err(e) => {
            render_error(&e);
            return 1;
        }
    };
    let rel = match dest {
        Some(d) => format!(".energize/secrets.{d}"),
        None => ".energize/secrets".to_string(),
    };
    let path = root.join(&rel);
    let existing = if path.exists() {
        if let Some(reason) = crate::trust::untrusted_reason(&path) {
            render_error(&format!(
                "refusing to modify {}: {reason}. A secrets file must be owned by the user \
                 running nrg and must not be writable by other users.",
                path.display()
            ));
            return 1;
        }
        match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                render_error(&format!("cannot read {}: {e}", path.display()));
                return 1;
            }
        }
    } else {
        String::new()
    };
    if let Err(e) = write_atomic(&path, &upsert_kv_line(&existing, key, &token)) {
        render_error(&e);
        return 1;
    }
    eprintln!(
        "{} Set {key} in {rel} (ciphertext — safe to commit)",
        "✓".green()
    );
    0
}

fn cmd_decrypt(token: Option<&str>) -> i32 {
    let key = match require_key("decrypt") {
        Ok(p) => p,
        Err(code) => return code,
    };
    let owned;
    let token = match token {
        Some(t) => t,
        None => {
            owned = match read_stdin_value("token to decrypt") {
                Ok(v) => v,
                Err(code) => return code,
            };
            &owned
        }
    };

    match secrets::decrypt_value(token, &key) {
        Ok(plaintext) => {
            println!("{}", plaintext);
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}

#[cfg(test)]
mod gitignore_warning_tests {
    use super::*;

    #[test]
    fn gitignore_covers_key_honours_a_parent_gitignore_via_git() {
        let git_ok = |args: &[&str], dir: &std::path::Path| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let tmp = tempfile::tempdir().unwrap();
        if !git_ok(&["init", "-q"], tmp.path()) {
            return; // no git on this box: the fallback tests cover the file-based path
        }
        let sub = tmp.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        assert!(!gitignore_covers_key(&sub));
        std::fs::write(tmp.path().join(".gitignore"), ".nrg-key\n").unwrap();
        assert!(gitignore_covers_key(&sub), "parent .gitignore must count");
    }

    #[test]
    fn gitignore_covers_key_recognizes_a_bare_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "node_modules\n.nrg-key\n*.log\n",
        )
        .unwrap();
        assert!(gitignore_covers_key(tmp.path()));
    }

    #[test]
    fn gitignore_covers_key_recognizes_a_rooted_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "node_modules\n/.nrg-key\n*.log\n",
        )
        .unwrap();
        assert!(gitignore_covers_key(tmp.path()));
    }

    #[test]
    fn gitignore_covers_key_recognizes_a_globbed_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "node_modules\n*.nrg-key\n*.log\n",
        )
        .unwrap();
        assert!(gitignore_covers_key(tmp.path()));
    }

    #[test]
    fn gitignore_covers_key_false_when_gitignore_is_missing_or_silent_on_it() {
        let tmp = tempfile::tempdir().unwrap();
        // No .gitignore at all.
        assert!(!gitignore_covers_key(tmp.path()));
        // A .gitignore that exists but never mentions the key.
        std::fs::write(tmp.path().join(".gitignore"), "node_modules\n*.log\n").unwrap();
        assert!(!gitignore_covers_key(tmp.path()));
    }

    #[test]
    fn gitignore_covers_key_ignores_comments_and_blank_lines() {
        let tmp = tempfile::tempdir().unwrap();
        // A COMMENTED-OUT mention must not count as real coverage.
        std::fs::write(tmp.path().join(".gitignore"), "# .nrg-key\n\n*.log\n").unwrap();
        assert!(!gitignore_covers_key(tmp.path()));
    }

    #[test]
    fn in_git_worktree_true_inside_a_repo_and_from_a_nested_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(
            in_git_worktree(&sub),
            "must find .git in an ancestor, not just the exact dir"
        );
    }

    #[test]
    fn in_git_worktree_false_outside_any_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!in_git_worktree(tmp.path()));
    }
}

#[cfg(test)]
mod encrypt_tests {
    use super::*;

    #[test]
    fn upsert_replaces_the_first_matching_line_in_place() {
        let out = upsert_kv_line("# keys\nA=ENC[1]\nB=ENC[2]\nA=ENC[3]\n", "A", "ENC[new]");
        assert_eq!(out, "# keys\nA=ENC[new]\nB=ENC[2]\nA=ENC[3]\n");
    }

    #[test]
    fn upsert_appends_a_missing_key_and_keeps_comments() {
        let out = upsert_kv_line("# A=ENC[commented]\nB=ENC[2]\n", "A", "ENC[new]");
        assert_eq!(out, "# A=ENC[commented]\nB=ENC[2]\nA=ENC[new]\n");
    }

    #[test]
    fn upsert_adds_the_trailing_newline_and_handles_an_empty_file() {
        assert_eq!(
            upsert_kv_line("B=ENC[2]", "A", "ENC[n]"),
            "B=ENC[2]\nA=ENC[n]\n"
        );
        assert_eq!(upsert_kv_line("", "A", "ENC[n]"), "A=ENC[n]\n");
    }

    #[test]
    fn secret_names_are_identifiers() {
        for ok in ["K", "_k", "DB_URL2", "a1"] {
            assert!(valid_secret_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A=B", "A B", "a\nb", "é"] {
            assert!(!valid_secret_name(bad), "{bad:?}");
        }
    }
}
