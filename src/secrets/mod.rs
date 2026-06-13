use std::path::{Path, PathBuf};
use std::process::Command;

const KEY_FILENAME: &str = ".nrg-key";
const PUBKEY_FILENAME: &str = ".nrg-key.pub";

/// Restrict a file to owner read/write (0600) on unix. No-op elsewhere.
fn set_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

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

/// Walk up from `start` looking for `filename`, stopping AT `$HOME` (never searching above it).
/// Bounding the search at `$HOME` matches the state-root search and stops a stray `.nrg-key` in
/// an unrelated parent directory above your home from being silently used for decryption (#14).
fn find_upward(filename: &str, start: PathBuf, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
        if let Some(h) = home {
            if dir == h {
                break; // do not search above $HOME
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Find the private key file by walking up from CWD (stopping at `$HOME`), then `~/.config/nrg/key`.
pub fn find_key_file() -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = find_upward(KEY_FILENAME, cwd, dirs::home_dir().as_deref()) {
            return Some(p);
        }
    }
    if let Some(config_dir) = dirs::config_dir() {
        let candidate = config_dir.join("nrg").join("key");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Find the public key file (same bounded search as the private key).
pub fn find_pubkey_file() -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = find_upward(PUBKEY_FILENAME, cwd, dirs::home_dir().as_deref()) {
            return Some(p);
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

    // Enforce 0600 on the private identity ourselves rather than trusting age-keygen's umask —
    // this is an UNPASSPHRASED X25519 key, so owner-only at rest is the floor (#14).
    set_owner_only(&key_path);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_upward_stops_at_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/user");
        let sub = home.join("proj/sub");
        std::fs::create_dir_all(&sub).unwrap();
        // A key ABOVE $HOME must NOT be found from a dir inside $HOME.
        std::fs::write(tmp.path().join("home/.nrg-key"), "k").unwrap();
        assert_eq!(find_upward(KEY_FILENAME, sub.clone(), Some(&home)), None);
        // A key inside the project (below $HOME) IS found.
        std::fs::write(home.join("proj/.nrg-key"), "k").unwrap();
        assert_eq!(
            find_upward(KEY_FILENAME, sub, Some(&home)),
            Some(home.join("proj/.nrg-key"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn set_owner_only_makes_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("k");
        std::fs::write(&p, "secret").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        set_owner_only(&p);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
