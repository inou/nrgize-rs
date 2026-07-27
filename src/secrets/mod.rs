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

/// The effective uid of this process — the identity that decides who could have WRITTEN the file
/// we are about to trust as a cryptographic key. Defined once in `crate::trust`, which project-root
/// discovery (`crate::engine::state`) shares, so the two upward searches apply the SAME rule.
#[cfg(unix)]
use crate::trust::effective_uid;

/// True if `dir` is inside the region the invoking user controls: we own it and it is not
/// writable by other users (group-writable is still trusted — `0775` is the umask-002 default).
/// This is the boundary the upward key search refuses to climb out of; see `crate::trust`.
use crate::trust::is_user_controlled as dir_is_user_controlled;

/// Refuse a key file that somebody OTHER than the invoking user could have planted, or could
/// still replace: one we don't own, one that is world-writable, or one sitting in a directory we
/// don't own / that is world-writable (anyone who can write that directory can substitute the
/// key). Checked with `symlink_metadata` first, so a symlink is judged by its OWN ownership
/// rather than its target's, and then with `metadata`, so a link pointing at somebody else's
/// file is refused too.
///
/// Group ownership and group-writable modes are intentionally accepted: `0664` files and `0775`
/// directories are ordinary umask-002 defaults, not evidence of tampering.
#[cfg(unix)]
fn ensure_key_file_is_trusted(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let me = effective_uid();
    let refuse = |reason: String| -> Result<(), String> {
        Err(format!(
            "Refusing to use the key file '{}': {reason}. A key file must be owned by the user \
             running nrg and must not be writable by other users — otherwise anyone who can write \
             it (or its directory) can substitute the key your secrets are encrypted to.",
            path.display()
        ))
    };
    let inspect = |what: &str, e: std::io::Error| {
        format!("Cannot inspect {what} for key file '{}': {e}", path.display())
    };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let dir_meta = std::fs::metadata(dir).map_err(|e| inspect("the containing directory", e))?;
    if dir_meta.uid() != me {
        return refuse(format!(
            "its directory '{}' is owned by uid {} but nrg is running as uid {}",
            dir.display(),
            dir_meta.uid(),
            me
        ));
    }
    if dir_meta.permissions().mode() & 0o002 != 0 {
        return refuse(format!(
            "its directory '{}' is writable by other users (mode {:04o})",
            dir.display(),
            dir_meta.permissions().mode() & 0o7777
        ));
    }

    let link_meta = std::fs::symlink_metadata(path).map_err(|e| inspect("the file itself", e))?;
    if link_meta.uid() != me {
        return refuse(format!(
            "it is owned by uid {} but nrg is running as uid {}",
            link_meta.uid(),
            me
        ));
    }

    let meta = std::fs::metadata(path).map_err(|e| inspect("the file it resolves to", e))?;
    if meta.uid() != me {
        return refuse(format!(
            "it resolves to a file owned by uid {} but nrg is running as uid {}",
            meta.uid(),
            me
        ));
    }
    if meta.permissions().mode() & 0o002 != 0 {
        return refuse(format!(
            "it is writable by other users (mode {:04o})",
            meta.permissions().mode() & 0o7777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_key_file_is_trusted(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Walk up from `start` looking for `filename`, never leaving the region the invoking user
/// controls. Two boundaries, not one:
///
/// * `$HOME` — as before, the search never climbs above your home directory (#14). With no home
///   directory at all (`dirs::home_dir()` is `None`) there is no such boundary to stop at, so
///   only `start` itself is searched.
/// * ownership — the walk stops as soon as the directory it just searched is not one this user
///   controls, and whatever it does find must pass `ensure_key_file_is_trusted`. The `$HOME`
///   boundary only fires when `$HOME` really is an ancestor: run from `/tmp/...`, `/srv`, `/opt`,
///   a CI workspace or a container `WORKDIR`, the old loop popped all the way to `/` and adopted
///   the first `.nrg-key`/`.nrg-key.pub` it met — a file any other local user can plant in a
///   world-writable ancestor, which then becomes the recipient every `ENC[...]` token and `.enc`
///   file is encrypted to.
///
/// A candidate that exists but fails the ownership/mode check is a hard error, never a silent
/// fall-through to some other key: silently encrypting to a substituted recipient is the whole
/// vulnerability.
fn find_upward(
    filename: &str,
    start: PathBuf,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let mut dir = start;
    loop {
        let candidate = dir.join(filename);
        if candidate.exists() {
            ensure_key_file_is_trusted(&candidate)?;
            return Ok(Some(candidate));
        }
        match home {
            Some(h) if dir == h => break, // do not search above $HOME
            None => break,                // no $HOME boundary: never climb above `start`
            _ => {}
        }
        if !dir_is_user_controlled(&dir) {
            break; // do not climb out of the region this user controls
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(None)
}

/// Find the private key file by walking up from CWD (bounded by `$HOME` and by ownership, see
/// `find_upward`), then `~/.config/nrg/key`. `Err` means a key file WAS found and refused as
/// untrusted; the caller must surface that instead of carrying on with a different key.
pub fn find_key_file_checked() -> Result<Option<PathBuf>, String> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = find_upward(KEY_FILENAME, cwd, dirs::home_dir().as_deref())? {
            return Ok(Some(p));
        }
    }
    if let Some(config_dir) = dirs::config_dir() {
        let candidate = config_dir.join("nrg").join("key");
        if candidate.exists() {
            ensure_key_file_is_trusted(&candidate)?;
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// `Option`-shaped view of [`find_key_file_checked`] for callers that only have that shape (the
/// engine's `ENC[...]` resolution). A refusal is reported on stderr — it must never be silent —
/// and yields `None`, so the caller fails closed with its own "no key" error rather than ever
/// using a key we refused.
pub fn find_key_file() -> Option<PathBuf> {
    match find_key_file_checked() {
        Ok(found) => found,
        Err(e) => {
            eprintln!("[nrg] {e}");
            None
        }
    }
}

/// Find the public key file (same bounded, ownership-checked search as the private key).
pub fn find_pubkey_file() -> Result<Option<PathBuf>, String> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = find_upward(PUBKEY_FILENAME, cwd, dirs::home_dir().as_deref())? {
            return Ok(Some(p));
        }
    }
    if let Some(config_dir) = dirs::config_dir() {
        let candidate = config_dir.join("nrg").join("key.pub");
        if candidate.exists() {
            ensure_key_file_is_trusted(&candidate)?;
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Extract age-keygen's public key from its stderr output and validate it looks like a real
/// X25519 age public key (always bech32-encoded, starting with "age1") before the caller writes
/// it anywhere. Without this, a drift in age-keygen's stderr format silently fell back to an
/// EMPTY string via `unwrap_or("")`, which used to be written straight to `.nrg-key.pub` — every
/// later `encrypt` would then fail with a cryptic "age: no recipients" instead of pointing at the
/// real cause. Pulled out as its own pure function so the validation itself is unit-testable
/// without needing to fake `age-keygen`'s real stderr.
fn parse_and_validate_pubkey(stderr: &str) -> Result<String, String> {
    let pubkey = stderr
        .lines()
        .find(|l| l.starts_with("Public key:"))
        .and_then(|l| l.strip_prefix("Public key: "))
        .unwrap_or("")
        .trim()
        .to_string();

    if !pubkey.starts_with("age1") {
        return Err(format!(
            "age-keygen did not print a recognizable public key (expected a line starting with \
             'Public key:' whose value starts with 'age1'); got: {:?}.",
            stderr.trim()
        ));
    }

    Ok(pubkey)
}

/// Read the recipient out of a `.nrg-key.pub` file and validate it really is an age X25519
/// recipient before it is spliced into `age -r`. Generation already enforces this shape
/// (`parse_and_validate_pubkey`); re-checking it on READ means a substituted or corrupted
/// recipient file fails loudly here instead of quietly encrypting to whatever it happened to
/// contain.
fn read_recipient(pubkey_path: &Path) -> Result<String, String> {
    let contents = std::fs::read_to_string(pubkey_path)
        .map_err(|e| format!("Cannot read public key '{}': {}", pubkey_path.display(), e))?;
    let recipient = contents.trim().to_string();
    if !recipient.starts_with("age1") {
        return Err(format!(
            "Public key file '{}' does not hold an age recipient (expected a value starting with \
             'age1'); got: {:?}. Refusing to encrypt to it — re-run 'nrg secrets init' or restore \
             the real .nrg-key.pub.",
            pubkey_path.display(),
            recipient.chars().take(48).collect::<String>()
        ));
    }
    Ok(recipient)
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

    let stderr = String::from_utf8_lossy(&output.stderr);
    let pubkey = parse_and_validate_pubkey(&stderr).map_err(|e| {
        format!(
            "{e} The private key was still written to {} — delete it and retry, or extract the \
             public key manually with 'age-keygen -y {}'.",
            key_path.display(),
            key_path.display()
        )
    })?;

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

    let pubkey = read_recipient(pubkey_path)?;

    let mut child = Command::new("age")
        .args(["-r", &pubkey, "-a"])
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

    // age -a's PEM-style armor is multi-line ("-----BEGIN AGE ENCRYPTED FILE-----\n...\n-----END
    // AGE ENCRYPTED FILE-----"), which is NOT safe to paste as a single `KEY=VALUE` line in
    // .env / .energize/secrets (the documented workflow for an ENC[...] token) — a line-based
    // parser would only see the first line. Join with `|` (never present in base64 or the PEM
    // header/footer, so this is unambiguous and reversible by decrypt_value) to make the whole
    // token single-line-safe.
    let ciphertext = String::from_utf8_lossy(&output.stdout);
    let single_line = ciphertext.trim().lines().collect::<Vec<_>>().join("|");
    Ok(format!("ENC[{}]", single_line))
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

    // Strip ENC[...] wrapper, then reverse encrypt_value's `|`-join back into real newlines so
    // age sees its own PEM-style armor unchanged. Safe for an OLDER token that already has real
    // newlines (pre-dating the `|`-join): it contains no `|`, so the replace is a no-op.
    let ciphertext = token
        .strip_prefix("ENC[")
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("Invalid encrypted token format: {}", token))?;
    let ciphertext = ciphertext.replace('|', "\n");

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

    let pubkey = read_recipient(pubkey_path)?;

    let out_path = PathBuf::from(format!("{}.enc", env_path.display()));

    let output = Command::new("age")
        .args(["-r", &pubkey, "-o"])
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

/// Decrypt a sealed .env file. Refuses to overwrite an existing output file unless `overwrite` is
/// true (a locally-edited `.env` used to be silently clobbered the moment someone ran `unseal`
/// again). The decrypted output is always forced to owner-only (0600) afterward: `age -o` writes
/// under the process umask, which on a shared/misconfigured umask can leave decrypted secrets
/// group- or world-readable at rest.
pub fn unseal_file(enc_path: &Path, key_path: &Path, overwrite: bool) -> Result<PathBuf, String> {
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

    if !overwrite && out_path.exists() {
        return Err(format!(
            "{} already exists — refusing to overwrite a possibly locally-edited file. Pass \
             --force to overwrite it anyway.",
            out_path.display()
        ));
    }

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

    // The decrypted output is plaintext secrets at rest — owner-only regardless of umask, the
    // same floor generate_key_pair already enforces on the private identity itself.
    set_owner_only(&out_path);

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
        assert_eq!(find_upward(KEY_FILENAME, sub.clone(), Some(&home)), Ok(None));
        // A key inside the project (below $HOME) IS found.
        std::fs::write(home.join("proj/.nrg-key"), "k").unwrap();
        assert_eq!(
            find_upward(KEY_FILENAME, sub, Some(&home)),
            Ok(Some(home.join("proj/.nrg-key")))
        );
    }

    #[test]
    fn find_upward_finds_a_project_key_from_a_subdir_outside_home() {
        // The ordinary case the ownership bound must NOT break: a project the invoking user owns
        // that does not live under $HOME at all (/srv, /opt, a CI checkout under /tmp), invoked
        // from a subdirectory. $HOME is never an ancestor here, so this walk is bounded purely by
        // ownership — and every directory in it is ours.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("srv/app");
        let sub = proj.join("deploy/config");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(proj.join(".nrg-key.pub"), "age1example").unwrap();
        let elsewhere = tmp.path().join("home/someone-else");
        assert_eq!(
            find_upward(PUBKEY_FILENAME, sub, Some(&elsewhere)),
            Ok(Some(proj.join(".nrg-key.pub")))
        );
    }

    #[test]
    fn find_upward_without_a_home_boundary_never_climbs_above_the_start_dir() {
        // `dirs::home_dir()` returning None used to leave the walk with no boundary at all, so it
        // popped to `/` and adopted whatever it met on the way.
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("proj/sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.path().join("proj/.nrg-key"), "k").unwrap();
        assert_eq!(find_upward(KEY_FILENAME, sub.clone(), None), Ok(None));
        // The start directory itself is still searched.
        std::fs::write(sub.join(".nrg-key"), "k").unwrap();
        assert_eq!(
            find_upward(KEY_FILENAME, sub.clone(), None),
            Ok(Some(sub.join(".nrg-key")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_upward_refuses_a_key_in_a_world_writable_ancestor() {
        // The reported attack: CWD is not under $HOME, so the old walk popped past every
        // boundary and silently adopted a `.nrg-key.pub` any local user could have planted in a
        // world-writable ancestor — making it the recipient of every secret encrypted afterwards.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let wide = tmp.path().join("wide");
        let sub = wide.join("proj/sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(wide.join(PUBKEY_FILENAME), "age1attacker").unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o777)).unwrap();

        let err = find_upward(PUBKEY_FILENAME, sub, Some(tmp.path()))
            .expect_err("a key in a world-writable directory must be refused, not used");
        assert!(err.contains(".nrg-key.pub"), "the error must name the file: {err}");
        assert!(
            err.contains("writable by other users"),
            "the error must give the reason: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_upward_refuses_a_world_writable_key_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join(KEY_FILENAME);
        std::fs::write(&key, "k").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o666)).unwrap();

        let err = find_upward(KEY_FILENAME, tmp.path().to_path_buf(), Some(tmp.path()))
            .expect_err("a world-writable key file must be refused");
        assert!(err.contains(".nrg-key"), "the error must name the file: {err}");
        assert!(
            err.contains("writable by other users"),
            "the error must give the reason: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_upward_does_not_climb_out_of_the_user_controlled_region() {
        // Nothing above a directory this user does not control is searched at all — the walk
        // stops there rather than continuing to `/`.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let wide = tmp.path().join("wide");
        let sub = wide.join("proj");
        std::fs::create_dir_all(&sub).unwrap();
        // A perfectly good key of our own, but ABOVE the world-writable directory.
        std::fs::write(tmp.path().join(KEY_FILENAME), "k").unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o777)).unwrap();

        // `$HOME` is somewhere else entirely, so only the ownership bound can stop this walk.
        let elsewhere = tmp.path().join("home/someone-else");
        assert_eq!(find_upward(KEY_FILENAME, sub, Some(&elsewhere)), Ok(None));
    }

    #[cfg(unix)]
    #[test]
    fn group_writable_key_files_and_directories_are_still_accepted() {
        // umask 002 (the RHEL/Fedora default, and any setgid team checkout) produces 0664 files
        // in 0775 directories. Those are ordinary, not tampered with — refusing them would break
        // perfectly normal installations.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let key = proj.join(PUBKEY_FILENAME);
        std::fs::write(&key, "age1example").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o664)).unwrap();
        std::fs::set_permissions(&proj, std::fs::Permissions::from_mode(0o775)).unwrap();

        assert_eq!(
            find_upward(PUBKEY_FILENAME, proj.clone(), Some(tmp.path())),
            Ok(Some(key))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_key_symlinked_into_a_world_writable_directory_is_refused() {
        // The link itself is ours and sits in a directory we control, but it resolves to a file
        // anyone can rewrite — so the recipient/identity is still attacker-controlled.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let wide = tmp.path().join("wide");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&wide).unwrap();
        let target = wide.join("planted");
        std::fs::write(&target, "age1attacker").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::os::unix::fs::symlink(&target, proj.join(PUBKEY_FILENAME)).unwrap();

        let err = find_upward(PUBKEY_FILENAME, proj, Some(tmp.path()))
            .expect_err("a key resolving to a world-writable file must be refused");
        assert!(
            err.contains("writable by other users"),
            "the error must give the reason: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_key_the_user_owns_end_to_end_is_still_accepted() {
        // The mirror image of the test above: symlinking `.nrg-key` at a key you keep elsewhere
        // is a legitimate setup and must keep working.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let target = store.join("key");
        std::fs::write(&target, "k").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = proj.join(KEY_FILENAME);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(
            find_upward(KEY_FILENAME, proj, Some(tmp.path())),
            Ok(Some(link))
        );
    }

    #[test]
    fn read_recipient_accepts_a_real_pubkey_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(PUBKEY_FILENAME);
        let key = "age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqqqqqqq";
        std::fs::write(&p, format!("{key}\n")).unwrap();
        assert_eq!(read_recipient(&p).unwrap(), key);
    }

    #[test]
    fn read_recipient_refuses_a_file_that_is_not_an_age_recipient() {
        // The recipient is validated at generation time; validating it again on READ is what
        // makes a substituted or corrupted `.nrg-key.pub` fail loudly instead of silently
        // becoming the key every secret is encrypted to.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(PUBKEY_FILENAME);
        std::fs::write(&p, "ssh-rsa AAAAnot-an-age-recipient\n").unwrap();
        let err = read_recipient(&p).unwrap_err();
        assert!(err.contains("does not hold an age recipient"), "got: {err}");
        assert!(err.contains("ssh-rsa"), "the error should quote what it saw: {err}");
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

    #[test]
    fn parse_and_validate_pubkey_accepts_real_age_keygen_stderr() {
        let stderr = "Public key: age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqqqqqqq\n";
        assert_eq!(
            parse_and_validate_pubkey(stderr).unwrap(),
            "age1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqqqqqqq"
        );
    }

    #[test]
    fn parse_and_validate_pubkey_rejects_a_missing_public_key_line() {
        // A stderr format drift (or a completely different message) with no "Public key:" line
        // at all used to silently fall back to an empty string via `unwrap_or("")`.
        let err = parse_and_validate_pubkey("some unrelated warning\n").unwrap_err();
        assert!(err.contains("did not print a recognizable public key"), "got: {err}");
    }

    #[test]
    fn parse_and_validate_pubkey_rejects_a_value_not_starting_with_age1() {
        // Even if a "Public key:" line IS present, a value that doesn't look like a real X25519
        // age public key (e.g. a truncated/garbled line from a different age-keygen version)
        // must be refused rather than written to .nrg-key.pub as-is.
        let err = parse_and_validate_pubkey("Public key: not-a-real-key\n").unwrap_err();
        assert!(err.contains("did not print a recognizable public key"), "got: {err}");
        assert!(err.contains("not-a-real-key"), "error should quote what it actually saw: {err}");
    }
}
