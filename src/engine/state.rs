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

/// Find the project root by walking up from CWD looking for a marker, never searching above
/// `$HOME`. If no marker is found, default to CWD (safe first-run behavior — we never plant
/// `.energize` above where the user invoked us).
pub fn find_project_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read CWD: {e}"))?;
    Ok(find_root_from(&cwd, dirs::home_dir().as_deref()))
}

/// Core upward-search loop, parameterized on start dir + home boundary so it is testable
/// without touching process CWD / real `$HOME`.
fn find_root_from(start: &Path, home: Option<&Path>) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        for m in ROOT_MARKERS {
            if dir.join(m).exists() {
                return dir;
            }
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
    /// An in-memory store that never touches disk.
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
                    format!(
                        "CORRUPT state file {} ({e}). Refusing to run to avoid losing deploy \
                         history — inspect or restore it (a backup may exist at {}). Once \
                         fixed, re-run.",
                        path.display(),
                        backup_path(root).display()
                    )
                })?;
                Ok(StateStore {
                    root: Some(root.to_path_buf()),
                    data: file.data,
                })
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.data.get(key).cloned()
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.data.clone()
    }

    /// Set a key and atomically persist. No-op persistence for an ephemeral store.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.data.insert(key.to_string(), value.to_string());
        self.flush()
    }

    /// Delete a key and atomically persist.
    pub fn del(&mut self, key: &str) -> Result<(), String> {
        self.data.remove(key);
        self.flush()
    }

    /// Atomically write the current map: backup current → write `.tmp` → fsync → rename.
    /// `rename` is atomic on POSIX, so a crash mid-write never publishes a torn file
    /// (which is also why no separate checksum is needed — a partial write stays in `.tmp`).
    fn flush(&self) -> Result<(), String> {
        use std::io::Write;
        let Some(root) = &self.root else {
            return Ok(()); // ephemeral: nothing to persist
        };
        let dir = root.join(".energize");
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = state_path(root);
        let tmp = dir.join("state.json.tmp");
        let bak = backup_path(root);

        let file = StateFile {
            version: STATE_VERSION,
            data: self.data.clone(),
        };
        let json =
            serde_json::to_string_pretty(&file).map_err(|e| format!("cannot serialize state: {e}"))?;

        if path.exists() {
            let _ = fs::copy(&path, &bak); // best-effort backup
        }
        {
            let mut f =
                fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            f.write_all(json.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("cannot fsync {}: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, &path)
            .map_err(|e| format!("cannot rename {} -> {}: {e}", tmp.display(), path.display()))?;
        Ok(())
    }
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
        assert!(!tmp.path().join(".energize/state.json.tmp").exists());
    }

    #[test]
    fn ephemeral_set_does_not_touch_disk() {
        let mut s = StateStore::ephemeral();
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k"), Some("v".to_string()));
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
