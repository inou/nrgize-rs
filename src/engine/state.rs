//! Project-anchored, lock-serialized, atomically-written deployment state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Markers that identify a project root, searched upward from CWD. `.git` is intentionally
/// NOT a marker (spec §8): we never want to plant state at an unrelated VCS root.
const ROOT_MARKERS: &[&str] = &[".energize", "energize.toml", ".nrg-key"];

/// Current on-disk schema version.
const STATE_VERSION: u32 = 1;

/// Env var holding the canonical path of the state lock the current process tree owns. A
/// nested `nrg` invocation (e.g. from a pre-deploy hook) reads this to AVOID self-deadlock.
pub const LOCK_ENV: &str = "NRG_STATE_LOCK";

/// True if `dir` directly contains a project-root marker.
fn has_marker(dir: &Path) -> bool {
    ROOT_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Find the project root by walking up from CWD looking for a marker, never searching above
/// `$HOME`. If no marker is found, default to CWD (safe first-run behavior — we never plant
/// `.energize` above where the user invoked us). As a guard, we REFUSE to use `$HOME` itself
/// as a markerless root (so a throwaway script never scaffolds `$HOME/.energize`).
pub fn find_project_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read CWD: {e}"))?;
    let home = dirs::home_dir();
    let root = find_root_from(&cwd, home.as_deref());
    if let Some(h) = &home {
        if &root == h && !has_marker(&root) {
            return Err(format!(
                "refusing to use your home directory ({}) as a project root. cd into a project \
                 directory, or create an `energize.toml` file / `.energize/` directory there to \
                 mark it as an Energize project.",
                h.display()
            ));
        }
    }
    Ok(root)
}

/// Core upward-search loop, parameterized on start dir + home boundary so it is testable
/// without touching process CWD / real `$HOME`.
fn find_root_from(start: &Path, home: Option<&Path>) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        if has_marker(&dir) {
            return dir;
        }
        if let Some(h) = home {
            if dir == h {
                break; // do not search above $HOME
            }
        }
        if !dir.pop() {
            break; // reached filesystem root
        }
    }
    start.to_path_buf()
}

/// On-disk representation: a versioned wrapper around the key/value map.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    data: BTreeMap<String, String>,
}

/// In-memory deployment state. `root == None` is an ephemeral store (no disk I/O), used by
/// unit tests and any non-state command path.
#[derive(Debug)]
pub struct StateStore {
    root: Option<PathBuf>,
    data: BTreeMap<String, String>,
}

impl StateStore {
    /// An in-memory store that never touches disk. Used by tests and the dry-run (P3) path.
    #[allow(dead_code)] // wired to the dry-run path in P3
    pub fn ephemeral() -> Self {
        StateStore {
            root: None,
            data: BTreeMap::new(),
        }
    }

    /// Load state from `<root>/.energize/state.json`. A MISSING file is an empty store
    /// (legitimate first run). A PRESENT but unparseable file is FATAL — we refuse to run
    /// rather than silently resetting deploy history (the old `unwrap_or_default()` bug).
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = state_path(root);
        match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(StateStore {
                root: Some(root.to_path_buf()),
                data: BTreeMap::new(),
            }),
            Err(e) => Err(format!("cannot read state file {}: {e}", path.display())),
            Ok(content) => {
                let file: StateFile = serde_json::from_str(&content).map_err(|e| {
                    // Only point at the backup if it actually exists — the `.bak` write is
                    // best-effort, so advertising a file that may never have been written
                    // (and is at most "one flush ago", not a pre-run snapshot) misleads (#28).
                    let bak = backup_path(root);
                    let hint = if bak.exists() {
                        format!(" A backup from the previous write may exist at {}.", bak.display())
                    } else {
                        String::new()
                    };
                    format!(
                        "CORRUPT state file {} ({e}). Refusing to run to avoid losing deploy \
                         history — inspect or restore it.{hint} Once fixed, re-run.",
                        path.display()
                    )
                })?;
                if file.version > STATE_VERSION {
                    return Err(format!(
                        "state file {} is version {}, but this nrg understands up to version {}. \
                         Upgrade nrg to read it (refusing to downgrade-rewrite it).",
                        path.display(),
                        file.version,
                        STATE_VERSION
                    ));
                }
                Ok(StateStore {
                    root: Some(root.to_path_buf()),
                    data: file.data,
                })
            }
        }
    }

    /// Load the on-disk data into an in-memory OVERLAY (root = None ⇒ flush is a no-op). Used
    /// by dry-run so `state_set`/`state_del` stay consistent for subsequent `state_get`s
    /// without ever touching disk.
    pub fn load_overlay(root: &Path) -> Result<Self, String> {
        let loaded = Self::load(root)?;
        Ok(StateStore {
            root: None,
            data: loaded.data,
        })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.data.get(key).cloned()
    }

    /// The project root this store is anchored at (`None` for an ephemeral store). Used to resolve
    /// project-relative paths (secrets, `.env`) against the SAME root as state, so running `nrg`
    /// from a subdirectory doesn't read a different `.env` than it writes state to (issue #19).
    pub fn root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.data.clone()
    }

    /// Every service with a `<svc>.version` key (set by `lib/deploy.rhai`'s `deploy()`), sorted
    /// for stable output. Lets `nrg status`/`nrg logs`/`nrg app exec` discover what's deployed
    /// without the caller needing to know service names up front.
    pub fn services(&self) -> Vec<String> {
        self.data
            .keys()
            .filter_map(|k| k.strip_suffix(".version").map(str::to_string))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Hosts with a persisted proxy target for `service`: `<service>.target.<host>` -> host.
    /// Matched by PREFIX (not split on '.') because a host commonly contains dots itself
    /// (`deploy@web1.example.com`), so a naive split would fragment it.
    pub fn hosts_for(&self, service: &str) -> Vec<String> {
        let prefix = format!("{service}.target.");
        let mut hosts: Vec<String> = self
            .data
            .keys()
            .filter_map(|k| k.strip_prefix(prefix.as_str()).map(str::to_string))
            .collect();
        hosts.sort();
        hosts
    }

    /// Set a key and atomically persist. No-op persistence for an ephemeral store.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.reload_from_disk()?;
        self.data.insert(key.to_string(), value.to_string());
        self.flush()
    }

    /// Delete a key and atomically persist.
    pub fn del(&mut self, key: &str) -> Result<(), String> {
        self.reload_from_disk()?;
        self.data.remove(key);
        self.flush()
    }

    /// Re-read the on-disk map before a mutation so a stale in-memory copy (e.g. after a
    /// nested `nrg` invocation wrote concurrently between our load and this write) does not
    /// clobber another writer's keys when we flush the whole map. No-op for an ephemeral
    /// store. Missing file = empty; corrupt = error (fail loud, same as `load`).
    fn reload_from_disk(&mut self) -> Result<(), String> {
        if let Some(root) = self.root.clone() {
            self.data = StateStore::load(&root)?.data;
        }
        Ok(())
    }

    /// Atomically write the current map: backup current → write a UNIQUE temp file in the same
    /// dir → fsync → atomic rename. `rename` is atomic on POSIX, so a crash mid-write never
    /// publishes a torn file (no separate checksum needed — a partial write stays in the temp).
    ///
    /// The temp file is a `NamedTempFile` with a UNIQUE name (not a fixed `state.json.tmp`), so two
    /// interleaved writers can't truncate each other's temp and rename a half-written file into
    /// place (issue #28). It is created 0600 by `tempfile`, and the final `state.json` inherits
    /// those perms via the rename — so state (which may hold `state_set`'d secrets) is never
    /// world-readable (issue #12). The `.bak` is `chmod 0600` after the copy for the same reason.
    fn flush(&self) -> Result<(), String> {
        let Some(root) = &self.root else {
            return Ok(()); // ephemeral: nothing to persist
        };
        let dir = root.join(".energize");
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = state_path(root);
        let bak = backup_path(root);

        let file = StateFile {
            version: STATE_VERSION,
            data: self.data.clone(),
        };
        let json =
            serde_json::to_string_pretty(&file).map_err(|e| format!("cannot serialize state: {e}"))?;

        if path.exists() {
            let _ = fs::copy(&path, &bak); // best-effort backup
            set_owner_only(&bak); // the backup carries the same (possibly secret) data
        }

        // A uniquely-named temp file IN THE SAME DIR (so the rename stays on one filesystem and is
        // atomic). tempfile creates it 0600.
        let mut tmp = tempfile::Builder::new()
            .prefix("state.json.")
            .suffix(".tmp")
            .tempfile_in(&dir)
            .map_err(|e| format!("cannot create temp state file in {}: {e}", dir.display()))?;
        {
            use std::io::Write;
            tmp.write_all(json.as_bytes())
                .map_err(|e| format!("cannot write temp state file: {e}"))?;
            tmp.as_file()
                .sync_all()
                .map_err(|e| format!("cannot fsync temp state file: {e}"))?;
        }
        // persist() does the atomic rename onto `path`, preserving the 0600 perms.
        tmp.persist(&path)
            .map_err(|e| format!("cannot publish state file {}: {e}", path.display()))?;
        // fsync the directory so the rename itself is durable across a hard crash. Unix only;
        // best-effort.
        #[cfg(unix)]
        if let Ok(dirf) = fs::File::open(&dir) {
            let _ = dirf.sync_all();
        }
        Ok(())
    }
}

/// Restrict a file to owner read/write (0600) on unix. No-op (and harmless) elsewhere.
fn set_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn state_path(root: &Path) -> PathBuf {
    root.join(".energize").join("state.json")
}
fn backup_path(root: &Path) -> PathBuf {
    root.join(".energize").join("state.json.bak")
}

/// Open (creating if needed) the advisory lock file for a root. The returned `RwLock<File>`
/// must be kept alive by the caller; calling `.write()` on it takes the exclusive flock,
/// released when the guard (and the `RwLock`) drop at the end of the run.
pub fn open_lock(root: &Path) -> io::Result<fd_lock::RwLock<fs::File>> {
    let dir = root.join(".energize");
    fs::create_dir_all(&dir)?;
    let f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("state.lock"))?;
    Ok(fd_lock::RwLock::new(f))
}

/// The canonical lock key for a root (resolves symlinks so aliases share one lock).
pub fn lock_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// True if this process tree already holds the lock for `key` (set in `LOCK_ENV` by the
/// ancestor that acquired it) — so we reuse it instead of deadlocking.
pub fn lock_is_reentrant(key: &str, env_val: Option<&str>) -> bool {
    env_val == Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_marker_in_ancestor_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let sub = root.join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(root.join(".energize")).unwrap(); // the marker
        let found = find_root_from(&sub, Some(tmp.path()));
        assert_eq!(found, root);
    }

    #[test]
    fn defaults_to_start_dir_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let start = tmp.path().join("x/y");
        fs::create_dir_all(&start).unwrap();
        let found = find_root_from(&start, Some(tmp.path()));
        assert_eq!(found, start);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let s = StateStore::load(tmp.path()).unwrap();
        assert!(s.all().is_empty());
    }

    #[test]
    fn load_corrupt_file_is_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        fs::write(tmp.path().join(".energize/state.json"), "{ this is not json").unwrap();
        let err = StateStore::load(tmp.path()).unwrap_err();
        assert!(err.contains("CORRUPT"), "got: {err}");
    }

    #[test]
    fn set_persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = StateStore::load(tmp.path()).unwrap();
            s.set("app.version", "v42").unwrap();
            s.set("app.image", "ghcr.io/x:v42").unwrap();
            s.del("app.image").unwrap();
        }
        let s2 = StateStore::load(tmp.path()).unwrap();
        assert_eq!(s2.get("app.version"), Some("v42".to_string()));
        assert_eq!(s2.get("app.image"), None);
        // No leftover temp files (unique-named temps are renamed away on success).
        let leftover: Vec<_> = fs::read_dir(tmp.path().join(".energize"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "no temp state files should remain: {leftover:?}");
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut s = StateStore::load(tmp.path()).unwrap();
        s.set("db_pw", "supersecretvalue").unwrap();
        let mode = fs::metadata(tmp.path().join(".energize/state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "state.json must be owner-only (secrets may be stored)");
        // A second write (which makes a .bak) must also keep the backup owner-only.
        s.set("db_pw2", "anothersecretvalue").unwrap();
        let bmode = fs::metadata(tmp.path().join(".energize/state.json.bak"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(bmode, 0o600, "state.json.bak must be owner-only too");
    }

    #[test]
    fn ephemeral_set_does_not_touch_disk() {
        let mut s = StateStore::ephemeral();
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k"), Some("v".to_string()));
    }

    #[test]
    fn services_discovered_from_version_keys() {
        let mut s = StateStore::ephemeral();
        s.set("app.version", "v1").unwrap();
        s.set("app.image", "ghcr.io/x:v1").unwrap();
        s.set("worker.version", "v2").unwrap();
        assert_eq!(s.services(), vec!["app".to_string(), "worker".to_string()]);
    }

    #[test]
    fn hosts_for_handles_dotted_and_at_sign_host_names() {
        let mut s = StateStore::ephemeral();
        s.set("app.target.deploy@web1.example.com", "localhost:13000").unwrap();
        s.set("app.target.deploy@web2.example.com", "localhost:13010").unwrap();
        assert_eq!(
            s.hosts_for("app"),
            vec!["deploy@web1.example.com".to_string(), "deploy@web2.example.com".to_string()]
        );
    }

    #[test]
    fn hosts_for_does_not_cross_services() {
        let mut s = StateStore::ephemeral();
        s.set("app.target.web1", "localhost:1").unwrap();
        s.set("worker.target.web1", "localhost:2").unwrap();
        assert_eq!(s.hosts_for("app"), vec!["web1".to_string()]);
    }

    #[test]
    fn set_merges_concurrent_external_writes() {
        // Regression for the re-entrant nested-nrg lost-update bug: a stale in-memory store
        // must reload before flushing so it doesn't clobber another writer's keys.
        let tmp = tempfile::tempdir().unwrap();
        let mut parent = StateStore::load(tmp.path()).unwrap();
        parent.set("a", "1").unwrap(); // disk {a}
        {
            // Simulate a nested `nrg` writing `b` independently between the parent's writes.
            let mut child = StateStore::load(tmp.path()).unwrap();
            child.set("b", "2").unwrap(); // disk {a,b}
        }
        // Parent's in-memory map is still {a}; setting `c` must reload {a,b} first.
        parent.set("c", "3").unwrap();
        let final_state = StateStore::load(tmp.path()).unwrap();
        assert_eq!(final_state.get("a"), Some("1".into()));
        assert_eq!(final_state.get("b"), Some("2".into()), "nested write must not be lost");
        assert_eq!(final_state.get("c"), Some("3".into()));
    }

    #[test]
    fn overlay_seeds_from_disk_but_never_writes() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = StateStore::load(tmp.path()).unwrap();
            s.set("seeded", "yes").unwrap();
        }
        let mut overlay = StateStore::load_overlay(tmp.path()).unwrap();
        assert_eq!(overlay.get("seeded"), Some("yes".into())); // seeded from disk
        overlay.set("ghost", "1").unwrap(); // mutates memory only
        assert_eq!(overlay.get("ghost"), Some("1".into()));
        // Disk is untouched: a fresh load doesn't see `ghost`.
        let disk = StateStore::load(tmp.path()).unwrap();
        assert_eq!(disk.get("ghost"), None);
    }

    #[test]
    fn load_rejects_future_version() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".energize")).unwrap();
        fs::write(
            tmp.path().join(".energize/state.json"),
            r#"{"version": 999, "data": {}}"#,
        )
        .unwrap();
        let err = StateStore::load(tmp.path()).unwrap_err();
        assert!(err.contains("version 999"), "got: {err}");
    }

    #[test]
    fn exclusive_lock_blocks_second_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        let mut l1 = open_lock(tmp.path()).unwrap();
        let _g1 = l1.write().unwrap(); // hold exclusive
        let mut l2 = open_lock(tmp.path()).unwrap();
        assert!(
            l2.try_write().is_err(),
            "second acquire must not succeed while held"
        );
    }

    #[test]
    fn reentrancy_detected_by_matching_env() {
        let tmp = tempfile::tempdir().unwrap();
        let key = lock_key(tmp.path());
        assert!(lock_is_reentrant(&key, Some(&key)));
        assert!(!lock_is_reentrant(&key, None));
        assert!(!lock_is_reentrant(&key, Some("/some/other/root")));
    }
}
