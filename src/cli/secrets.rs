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
            println!(
                "  {} Add {} to your .gitignore!",
                "⚠".yellow(),
                ".nrg-key".bold()
            );
            println!("    The public key (.nrg-key.pub) is safe to commit.");
            0
        }
        Err(e) => {
            render_error(&e);
            1
        }
    }
}

fn cmd_encrypt(value: &str) -> i32 {
    let pubkey = match secrets::find_pubkey_file() {
        Some(p) => p,
        None => {
            render_error(
                "No public key found (.nrg-key.pub). Run 'nrg secrets init' first.",
            );
            return 1;
        }
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
    let key = match secrets::find_key_file() {
        Some(p) => p,
        None => {
            render_error("No private key found (.nrg-key). Cannot decrypt.");
            return 1;
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

fn cmd_seal(file: &str) -> i32 {
    let pubkey = match secrets::find_pubkey_file() {
        Some(p) => p,
        None => {
            render_error(
                "No public key found (.nrg-key.pub). Run 'nrg secrets init' first.",
            );
            return 1;
        }
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
    let key = match secrets::find_key_file() {
        Some(p) => p,
        None => {
            render_error("No private key found (.nrg-key). Cannot unseal.");
            return 1;
        }
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
