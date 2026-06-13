use clap::{Args, Subcommand};
use crossterm::style::Stylize;
use crate::secrets;

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

    /// Encrypt a single value, output an ENC[...] token
    Encrypt {
        /// The plaintext value to encrypt
        value: String,
    },

    /// Decrypt a single ENC[...] token
    Decrypt {
        /// The encrypted token (ENC[...])
        token: String,
    },

    /// Encrypt an entire .env file → .env.enc
    Seal {
        /// Path to the .env file
        file: String,
    },

    /// Decrypt a .env.enc file → .env (for editing)
    Unseal {
        /// Path to the .env.enc file
        file: String,
    },
}

pub fn execute(args: &SecretsArgs) -> i32 {
    match &args.command {
        SecretsCommand::Init => cmd_init(),
        SecretsCommand::Encrypt { value } => cmd_encrypt(value),
        SecretsCommand::Decrypt { token } => cmd_decrypt(token),
        SecretsCommand::Seal { file } => cmd_seal(file),
        SecretsCommand::Unseal { file } => cmd_unseal(file),
    }
}

fn cmd_init() -> i32 {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    match secrets::generate_key_pair(&dir) {
        Ok((key_path, pubkey_path)) => {
            println!(
                "  {} Generated key pair:",
                "✓".green()
            );
            println!("    Private key: {}", key_path.display());
            println!("    Public key:  {}", pubkey_path.display());
            println!();
            // If we're inside a git work tree and `.nrg-key` isn't already ignored, warn loudly:
            // committing the unpassphrased identity would expose every sealed secret (#14).
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

/// Whether a `.gitignore` in `dir` already lists `.nrg-key` (exact line match, comments/blank
/// lines ignored). Best-effort: only checks `dir`'s own `.gitignore`.
fn gitignore_covers_key(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join(".gitignore"))
        .map(|c| {
            c.lines()
                .map(|l| l.trim())
                .any(|l| l == ".nrg-key" || l == "/.nrg-key" || l == "*.nrg-key")
        })
        .unwrap_or(false)
}

/// Resolve the public key file or print the standard error and return `Err(1)`. Shared by the
/// encrypt/seal commands (issue #24).
fn require_pubkey() -> Result<std::path::PathBuf, i32> {
    secrets::find_pubkey_file().ok_or_else(|| {
        render_error("No public key found (.nrg-key.pub). Run 'nrg secrets init' first.");
        1
    })
}

/// Resolve the private key file or print the standard error and return `Err(1)`. Shared by the
/// decrypt/unseal commands.
fn require_key(action: &str) -> Result<std::path::PathBuf, i32> {
    secrets::find_key_file().ok_or_else(|| {
        render_error(&format!("No private key found (.nrg-key). Cannot {action}."));
        1
    })
}

fn cmd_encrypt(value: &str) -> i32 {
    let pubkey = match require_pubkey() {
        Ok(p) => p,
        Err(code) => return code,
    };

    match secrets::encrypt_value(value, &pubkey) {
        Ok(token) => {
            println!("{}", token);
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}

fn cmd_decrypt(token: &str) -> i32 {
    let key = match require_key("decrypt") {
        Ok(p) => p,
        Err(code) => return code,
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

fn cmd_seal(file: &str) -> i32 {
    let pubkey = match require_pubkey() {
        Ok(p) => p,
        Err(code) => return code,
    };

    let path = std::path::Path::new(file);
    match secrets::seal_file(path, &pubkey) {
        Ok(out_path) => {
            println!(
                "  {} Sealed {} → {}",
                "✓".green(),
                file,
                out_path.display()
            );
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}

fn cmd_unseal(file: &str) -> i32 {
    let key = match require_key("unseal") {
        Ok(p) => p,
        Err(code) => return code,
    };

    let path = std::path::Path::new(file);
    match secrets::unseal_file(path, &key) {
        Ok(out_path) => {
            println!(
                "  {} Unsealed {} → {}",
                "✓".green(),
                file,
                out_path.display()
            );
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}
