use std::path::{Path, PathBuf};
use std::process::Command;

const KEY_FILENAME: &str = ".nrg-key";
const PUBKEY_FILENAME: &str = ".nrg-key.pub";

/// Check if the `age` binary is available.
pub fn age_available() -> bool {
    Command::new("age")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if the `age-keygen` binary is available.
pub fn age_keygen_available() -> bool {
    Command::new("age-keygen")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Find the private key file by walking up from CWD, then checking ~/.config/nrg/key.
pub fn find_key_file() -> Option<PathBuf> {
    // Walk up from CWD
    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let candidate = dir.join(KEY_FILENAME);
            if candidate.exists() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
    }

    // Check ~/.config/nrg/key
    if let Some(config_dir) = dirs::config_dir() {
        let candidate = config_dir.join("nrg").join("key");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

/// Find the public key file (same search as private key).
pub fn find_pubkey_file() -> Option<PathBuf> {
    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let candidate = dir.join(PUBKEY_FILENAME);
            if candidate.exists() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
    }

    if let Some(config_dir) = dirs::config_dir() {
        let candidate = config_dir.join("nrg").join("key.pub");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

/// Generate a new age key pair. Returns (private_key_path, public_key_path).
pub fn generate_key_pair(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    if !age_keygen_available() {
        return Err(
            "age-keygen not found. Install age: https://github.com/FiloSottile/age\n\
             On macOS: brew install age\n\
             On Linux: apt install age (or download from GitHub releases)"
                .to_string(),
        );
    }

    let key_path = dir.join(KEY_FILENAME);
    let output = Command::new("age-keygen")
        .arg("-o")
        .arg(&key_path)
        .output()
        .map_err(|e| format!("Failed to run age-keygen: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "age-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Extract public key from stderr (age-keygen prints it there)
    let stderr = String::from_utf8_lossy(&output.stderr);
    let pubkey = stderr
        .lines()
        .find(|l| l.starts_with("Public key:"))
        .and_then(|l| l.strip_prefix("Public key: "))
        .unwrap_or("")
        .trim()
        .to_string();

    // Write public key file
    let pubkey_path = dir.join(PUBKEY_FILENAME);
    std::fs::write(&pubkey_path, &pubkey)
        .map_err(|e| format!("Failed to write public key: {}", e))?;

    Ok((key_path, pubkey_path))
}

/// Encrypt a single value using the public key.
pub fn encrypt_value(plaintext: &str, pubkey_path: &Path) -> Result<String, String> {
    if !age_available() {
        return Err(
            "age not found. Install age: https://github.com/FiloSottile/age\n\
             On macOS: brew install age\n\
             On Linux: apt install age (or download from GitHub releases)"
                .to_string(),
        );
    }

    let pubkey = std::fs::read_to_string(pubkey_path)
        .map_err(|e| format!("Cannot read public key '{}': {}", pubkey_path.display(), e))?;
    let pubkey = pubkey.trim();

    let mut child = Command::new("age")
        .args(["-r", pubkey, "-a"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run age: {}", e))?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(plaintext.as_bytes())
            .map_err(|e| format!("Failed to write to age stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("age failed: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "age encrypt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Base64 armored output → wrap as ENC[...]
    let ciphertext = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!("ENC[{}]", ciphertext))
}

/// Decrypt a single ENC[...] token.
pub fn decrypt_value(token: &str, key_path: &Path) -> Result<String, String> {
    if !age_available() {
        return Err(
            "age not found. Install age: https://github.com/FiloSottile/age\n\
             On macOS: brew install age\n\
             On Linux: apt install age (or download from GitHub releases)"
                .to_string(),
        );
    }

    // Strip ENC[...] wrapper
    let ciphertext = token
        .strip_prefix("ENC[")
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("Invalid encrypted token format: {}", token))?;

    let mut child = Command::new("age")
        .args(["-d", "-i", &key_path.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run age: {}", e))?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(ciphertext.as_bytes())
            .map_err(|e| format!("Failed to write to age stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("age failed: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "age decrypt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Encrypt an entire .env file (as a single blob) using the public key.
pub fn seal_file(env_path: &Path, pubkey_path: &Path) -> Result<PathBuf, String> {
    if !age_available() {
        return Err(
            "age not found. Install age: https://github.com/FiloSottile/age\n\
             On macOS: brew install age\n\
             On Linux: apt install age (or download from GitHub releases)"
                .to_string(),
        );
    }

    let pubkey = std::fs::read_to_string(pubkey_path)
        .map_err(|e| format!("Cannot read public key: {}", e))?;
    let pubkey = pubkey.trim();

    let out_path = PathBuf::from(format!("{}.enc", env_path.display()));

    let output = Command::new("age")
        .args(["-r", pubkey, "-o"])
        .arg(&out_path)
        .arg(env_path)
        .output()
        .map_err(|e| format!("Failed to run age: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "age seal failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(out_path)
}

/// Decrypt a sealed .env file.
pub fn unseal_file(enc_path: &Path, key_path: &Path) -> Result<PathBuf, String> {
    if !age_available() {
        return Err(
            "age not found. Install age: https://github.com/FiloSottile/age\n\
             On macOS: brew install age\n\
             On Linux: apt install age (or download from GitHub releases)"
                .to_string(),
        );
    }

    // Output path: strip .enc extension
    let out_path = if let Some(stem) = enc_path.to_string_lossy().strip_suffix(".enc") {
        PathBuf::from(stem)
    } else {
        PathBuf::from(format!("{}.decrypted", enc_path.display()))
    };

    let output = Command::new("age")
        .args(["-d", "-i"])
        .arg(key_path)
        .arg("-o")
        .arg(&out_path)
        .arg(enc_path)
        .output()
        .map_err(|e| format!("Failed to run age: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "age unseal failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(out_path)
}
