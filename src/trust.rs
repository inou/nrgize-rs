//! The one rule `nrg` applies before it believes a path it found by walking UP the directory
//! tree: **the invoking user must control it** — we own it, and no other user can write it.
//!
//! Two upward searches rely on this, and they must not drift apart:
//!
//! * the age-key search (`crate::secrets::find_upward`) — a planted `.nrg-key.pub` becomes the
//!   recipient every secret is encrypted to;
//! * project-root discovery (`crate::engine::state::find_root_from`) — a planted `.energize` /
//!   `energize.toml` / `.nrg-key` marker makes that directory the root, and the root supplies the
//!   `Energize.rhai` we execute, the `.energize/secrets` and `.env` we read (whose `CMD[...]`
//!   values are run through `sh -c`), and the state/audit files we write.
//!
//! Deliberate calls, identical for both:
//!
//! * **Group-writable is TRUSTED** while we own the path. `0775` directories and `0664` files are
//!   the umask-002 default on RHEL/Fedora and on setgid team checkouts; refusing them would break
//!   ordinary installs, not attacks.
//! * **The sticky bit is not an exemption.** `/tmp` is `1777`: sticky stops other users deleting
//!   *your* entries, it does not stop them creating their own — and a marker or key file they
//!   created is exactly the problem.
//! * **euid 0 is not exempt from the ownership rule.** A root shell that wanders into another
//!   user's directory must not adopt that user's project or key.
//! * A path we cannot even inspect is untrusted, not assumed fine.

use std::path::Path;

/// The effective uid of this process — the identity that decides who could have WRITTEN the thing
/// we are about to trust. Declared directly rather than taking on a `libc` dependency: `geteuid`
/// is always linked in by std on unix, and `uid_t` is `u32` on every unix target Rust supports.
#[cfg(unix)]
pub fn effective_uid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

/// Why `path` is NOT under the invoking user's control, phrased to be pasted into an error
/// message — `None` when it IS (which is when, and only when, it may be trusted).
#[cfg(unix)]
pub fn untrusted_reason(path: &Path) -> Option<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let me = effective_uid();
    // `metadata` (not `symlink_metadata`): a symlink is only as trustworthy as what it resolves
    // to, and that is the file/directory we would actually read.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Some(format!("it cannot be inspected ({e})")),
    };
    if meta.uid() != me {
        return Some(format!(
            "it is owned by uid {} but nrg is running as uid {me}",
            meta.uid()
        ));
    }
    if meta.permissions().mode() & 0o002 != 0
        || (meta.permissions().mode() & 0o020 != 0
            && std::env::var("NRG_STRICT_TRUST").as_deref() == Ok("1"))
    {
        return Some(format!(
            "it is writable by other users (mode {:04o})",
            meta.permissions().mode() & 0o7777
        ));
    }
    None
}

/// Non-unix has no uid/mode model to check against; nothing is refused on this ground.
#[cfg(not(unix))]
pub fn untrusted_reason(_path: &Path) -> Option<String> {
    None
}

/// True if the invoking user controls `path` — the boolean form of [`untrusted_reason`], for the
/// callers that only need to stop walking rather than to explain themselves.
pub fn is_user_controlled(path: &Path) -> bool {
    untrusted_reason(path).is_none()
}

/// Every test here needs uids and modes to mean something, so the whole module is unix-only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_directory_we_own_and_others_cannot_write_is_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o755);
        assert_eq!(untrusted_reason(&dir), None);
        assert!(is_user_controlled(&dir));
    }

    #[test]
    fn group_writable_is_trusted_but_world_writable_is_not() {
        // 0775 / 0664 are the umask-002 default on RHEL/Fedora and setgid team checkouts —
        // refusing those would break ordinary installs. Other-writable is the actual exposure.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("group");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o775);
        assert_eq!(untrusted_reason(&dir), None, "0775 must stay trusted");

        let file = tmp.path().join("secrets");
        std::fs::write(&file, "K=v\n").unwrap();
        chmod(&file, 0o664);
        assert_eq!(untrusted_reason(&file), None, "0664 must stay trusted");

        chmod(&dir, 0o777);
        assert!(
            untrusted_reason(&dir)
                .unwrap()
                .contains("writable by other users"),
            "0777 must be refused"
        );
        chmod(&file, 0o666);
        assert!(untrusted_reason(&file)
            .unwrap()
            .contains("writable by other users"));
    }

    #[test]
    fn the_sticky_bit_is_not_an_exemption() {
        // /tmp is 1777: sticky stops other users DELETING your entries, not creating their own.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tmpish");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o1777);
        assert!(
            untrusted_reason(&dir)
                .unwrap()
                .contains("writable by other users"),
            "a 1777 directory must be refused just like 0777"
        );
    }

    #[test]
    fn a_path_that_cannot_be_inspected_is_untrusted() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(untrusted_reason(&missing).is_some());
        assert!(!is_user_controlled(&missing));
    }

    #[test]
    fn a_foreign_owned_path_is_refused_even_when_running_as_root() {
        // euid 0 gets no ownership exemption: a root shell in another user's directory must not
        // adopt that user's project/key. Only runnable as root (nothing else can produce a path
        // owned by a different uid), so it self-skips otherwise, matching this repo's pattern for
        // environment-dependent tests (see tests/secrets_age.rs).
        if effective_uid() != 0 {
            eprintln!("skipping: needs root to create a path owned by another uid");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("someone-elses");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o700);
        let ok = std::process::Command::new("chown")
            .arg("65534")
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: chown unavailable");
            return;
        }
        assert!(
            untrusted_reason(&dir)
                .unwrap()
                .contains("owned by uid 65534"),
            "root must NOT be exempt from the ownership rule"
        );
    }
}
