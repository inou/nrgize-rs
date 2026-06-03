# Rhai Migration — Phase 1: State Locking + Atomicity — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Replace the Starlark-era state primitives with a project-anchored, lock-serialized, atomically-written, corruption-fatal `StateStore`, exposed to Rhai as `state_get/set/del/all`, with the exclusive lock held for the whole mutating `nrg exec` run and re-entrant for nested invocations.

**Architecture:** New `src/engine/state.rs` owns project-root discovery, `StateStore` (in-memory map + atomic flush + corruption-hard-fail load), and `open_lock` (an `fd_lock::RwLock<File>` on `.energize/state.lock`). `RunCtx` gains an `Arc<Mutex<StateStore>>`; the state builtins snapshot it out of the `RunCtx` lock before touching disk (same pattern as the runner). `cli/exec.rs` discovers the root, takes the exclusive flock (skipped if an ancestor `nrg` already holds it), loads state (aborting on corruption), and runs. P0 tests keep working because `context::shared()` defaults to an **ephemeral** (no-disk) store.

**Tech Stack:** `fd-lock 4` (advisory flock), `serde` + `serde_json` (state file), `dirs` (home boundary), `tempfile` (tests).

---

## Why these files

| File | Responsibility |
|---|---|
| `src/engine/state.rs` | `find_project_root`, `StateStore` (load/get/set/del/all/flush, ephemeral + on-disk), `StateFile` schema, `open_lock`, `lock_is_reentrant`. |
| `src/engine/builtins/state.rs` | Register `state_get/set/del/all` over the `RunCtx` store. |
| `src/engine/context.rs` | Add `state: Arc<Mutex<StateStore>>`; `shared` (ephemeral) + `shared_with_state`. |
| `src/engine/builtins/mod.rs` | Register the state builtins. |
| `src/cli/exec.rs` | Discover root → take exclusive lock (re-entrant) → load store → run. |
| `Cargo.toml` | Enable `serde` `derive` feature for `StateFile`. |

**Deliberate deferral (documented, not silent):** programmatic **NFS detection/refusal** (spec §8) needs a platform `statfs`/`libc` dependency for a rare edge case; Phase 1 ships a doc-level warning ("keep `.energize` on a local filesystem") instead. Tracked for a later hardening pass.

---

## Task 1: Enable serde derive

**Files:** Modify `Cargo.toml`

- [ ] **Step 1: Add the derive feature**

Change the `serde` line in `[dependencies]`:

```toml
serde = { version = "1", features = ["derive"] }
```

- [ ] **Step 2: Verify build**

Run: `cargo build`
Expected: compiles (serde already present; this only adds the derive macro).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: enable serde derive feature for state schema"
```

---

## Task 2: Project-root discovery

**Files:** Create `src/engine/state.rs`; Modify `src/engine/mod.rs` (add `pub mod state;`)

- [ ] **Step 1: Declare the module**

Add to `src/engine/mod.rs` (with the other `pub mod` lines):

```rust
pub mod state;
```

- [ ] **Step 2: Write the failing test + implementation**

Create `src/engine/state.rs`:

```rust
//! Project-anchored, lock-serialized, atomically-written deployment state.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Markers that identify a project root, searched upward from CWD. `.git` is intentionally
/// NOT a marker (spec §8): we never want to plant state at an unrelated VCS root.
const ROOT_MARKERS: &[&str] = &[".energize", "energize.toml", ".nrg-key"];

/// Find the project root by walking up from CWD looking for a marker, never searching above
/// `$HOME`. If no marker is found, default to CWD (safe first-run behavior — we never plant
/// `.energize` above where the user invoked us).
pub fn find_project_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read CWD: {e}"))?;
    let home = dirs::home_dir();
    let mut dir = cwd.clone();
    loop {
        for m in ROOT_MARKERS {
            if dir.join(m).exists() {
                return Ok(dir);
            }
        }
        if let Some(h) = &home {
            if &dir == h {
                break; // do not search above $HOME
            }
        }
        if !dir.pop() {
            break; // reached filesystem root
        }
    }
    Ok(cwd)
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

        // Search from `sub` should locate `root` (the dir holding `.energize`).
        // find_project_root reads CWD, so we test the core loop via a helper:
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

    /// Test seam mirroring `find_project_root` but with explicit start + home (so tests don't
    /// depend on process CWD / real $HOME).
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
                    break;
                }
            }
            if !dir.pop() {
                break;
            }
        }
        start.to_path_buf()
    }
}
```

> The test uses a `find_root_from` seam so it doesn't mutate process CWD. `find_project_root` itself is exercised by the integration test in Task 8.

- [ ] **Step 3: Run tests**

Run: `cargo test --bin nrg engine::state`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/mod.rs src/engine/state.rs
git commit -m "feat(state): project-root discovery (upward marker search, \$HOME-bounded)"
```

---

## Task 3: StateStore — load (missing→empty, corrupt→fatal) + ephemeral

**Files:** Modify `src/engine/state.rs`

- [ ] **Step 1: Add the schema + StateStore::load/ephemeral + getters**

Append to `src/engine/state.rs` (after the `find_project_root` fn, before `#[cfg(test)]`):

```rust
use serde::{Deserialize, Serialize};

/// Current on-disk schema version.
const STATE_VERSION: u32 = 1;

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
                         history — inspect or restore it (a backup may exist at \
                         {}). Once fixed, re-run.",
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
}

fn state_path(root: &Path) -> PathBuf {
    root.join(".energize").join("state.json")
}
fn backup_path(root: &Path) -> PathBuf {
    root.join(".energize").join("state.json.bak")
}
```

- [ ] **Step 2: Add tests** (inside the existing `#[cfg(test)] mod tests`, after the root tests):

```rust
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
```

- [ ] **Step 3: Run tests**

Run: `cargo test --bin nrg engine::state`
Expected: PASS (4 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/state.rs
git commit -m "feat(state): StateStore load — missing=empty, corrupt=fatal, versioned schema"
```

---

## Task 4: StateStore — atomic flush (set/del)

**Files:** Modify `src/engine/state.rs`

- [ ] **Step 1: Add set/del/flush** (inside `impl StateStore`, after `all`):

```rust
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
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("cannot serialize state: {e}"))?;

        if path.exists() {
            let _ = fs::copy(&path, &bak); // best-effort backup
        }
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            f.write_all(json.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("cannot fsync {}: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, &path)
            .map_err(|e| format!("cannot rename {} -> {}: {e}", tmp.display(), path.display()))?;
        Ok(())
    }
```

- [ ] **Step 2: Add tests:**

```rust
    #[test]
    fn set_persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = StateStore::load(tmp.path()).unwrap();
            s.set("app.version", "v42").unwrap();
            s.set("app.image", "ghcr.io/x:v42").unwrap();
            s.del("app.image").unwrap();
        }
        // Reload from disk: only app.version survives, no stray .tmp.
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
```

- [ ] **Step 3: Run tests**

Run: `cargo test --bin nrg engine::state`
Expected: PASS (6 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/state.rs
git commit -m "feat(state): atomic flush (backup + tmp + fsync + rename) for set/del"
```

---

## Task 5: Lock open + acquire + re-entrancy helper

**Files:** Modify `src/engine/state.rs`

- [ ] **Step 1: Add `open_lock` + `lock_is_reentrant`** (top-level fns, after `find_project_root`):

```rust
/// Env var holding the canonical path of the state lock the current process tree owns. A
/// nested `nrg` invocation (e.g. from a pre-deploy hook) reads this to AVOID self-deadlock.
pub const LOCK_ENV: &str = "NRG_STATE_LOCK";

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
```

- [ ] **Step 2: Add tests:**

```rust
    #[test]
    fn exclusive_lock_blocks_second_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        let mut l1 = open_lock(tmp.path()).unwrap();
        let _g1 = l1.write().unwrap(); // hold exclusive
        let mut l2 = open_lock(tmp.path()).unwrap();
        assert!(l2.try_write().is_err(), "second acquire must not succeed while held");
    }

    #[test]
    fn reentrancy_detected_by_matching_env() {
        let tmp = tempfile::tempdir().unwrap();
        let key = lock_key(tmp.path());
        assert!(lock_is_reentrant(&key, Some(&key)));
        assert!(!lock_is_reentrant(&key, None));
        assert!(!lock_is_reentrant(&key, Some("/some/other/root")));
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --bin nrg engine::state`
Expected: PASS (8 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/state.rs
git commit -m "feat(state): advisory flock (open_lock, canonical lock_key, reentrancy)"
```

---

## Task 6: RunCtx gains a StateStore

**Files:** Modify `src/engine/context.rs`

- [ ] **Step 1: Add the field + constructors**

Replace the body of `src/engine/context.rs` (keep the `EffectMode` enum as-is) so `RunCtx` carries the store:

```rust
//! Per-run shared context captured by every side-effecting builtin.

use crate::engine::runner::CommandRunner;
use crate::engine::state::StateStore;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectMode {
    Live,
    DryRun,
}

pub struct RunCtx {
    pub mode: EffectMode,
    pub runner: Arc<dyn CommandRunner>,
    /// In its own `Arc<Mutex>` so a builtin can snapshot it out of the `RunCtx` lock before
    /// touching disk (mirrors the runner pattern).
    pub state: Arc<Mutex<StateStore>>,
    pub trace: bool,
}

impl RunCtx {
    fn build(runner: Arc<dyn CommandRunner>, state: StateStore) -> Self {
        RunCtx {
            mode: EffectMode::Live,
            runner,
            state: Arc::new(Mutex::new(state)),
            trace: std::env::var("NRG_TRACE").is_ok(),
        }
    }
}

pub type SharedCtx = Arc<Mutex<RunCtx>>;

/// Shared context with an EPHEMERAL (no-disk) store — used by unit tests and any path that
/// doesn't load real state.
pub fn shared(runner: Arc<dyn CommandRunner>) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::build(runner, StateStore::ephemeral())))
}

/// Shared context with a real, loaded on-disk store (used by `nrg exec`).
pub fn shared_with_state(runner: Arc<dyn CommandRunner>, state: StateStore) -> SharedCtx {
    Arc::new(Mutex::new(RunCtx::build(runner, state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn ctx_defaults_to_live_with_ephemeral_state() {
        let ctx = shared(FakeRunner::shared());
        let g = ctx.lock().unwrap();
        assert_eq!(g.mode, EffectMode::Live);
        assert!(g.state.lock().unwrap().all().is_empty());
    }
}
```

- [ ] **Step 2: Run tests** (the whole engine, since this changes a shared type)

Run: `cargo test --bin nrg engine`
Expected: PASS — all existing engine tests still green (they call `shared`, which now bundles an ephemeral store).

- [ ] **Step 3: Commit**

```bash
git add src/engine/context.rs
git commit -m "feat(state): RunCtx carries Arc<Mutex<StateStore>> (ephemeral default)"
```

---

## Task 7: state builtins (`state_get/set/del/all`)

**Files:** Create `src/engine/builtins/state.rs`; Modify `src/engine/builtins/mod.rs`

- [ ] **Step 1: Register in `builtins/mod.rs`**

```rust
//! Registration of all side-effecting Rhai builtins.
pub mod exec;
pub mod http;
pub mod state;
pub mod util;

use crate::engine::context::SharedCtx;
use rhai::Engine;

pub fn register_builtins(engine: &mut Engine, ctx: SharedCtx) {
    exec::register(engine, ctx.clone());
    http::register(engine, ctx.clone());
    state::register(engine, ctx.clone());
    util::register(engine, ctx);
}
```

- [ ] **Step 2: Create `src/engine/builtins/state.rs`**

```rust
//! Persistent-state builtins, backed by the RunCtx's StateStore. Reads/writes snapshot the
//! store Arc out of the RunCtx lock before touching it (so disk I/O never holds RunCtx).

use crate::engine::context::SharedCtx;
use crate::engine::state::StateStore;
use rhai::{Dynamic, Engine, EvalAltResult, Map};
use std::sync::{Arc, Mutex};

fn store(ctx: &SharedCtx) -> Arc<Mutex<StateStore>> {
    ctx.lock().unwrap().state.clone()
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // state_get(key) -> String | ()   (() when absent, so scripts can use `if x { }`)
    {
        let ctx = ctx.clone();
        engine.register_fn("state_get", move |key: &str| -> Dynamic {
            match store(&ctx).lock().unwrap().get(key) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
    }
    // state_set(key, value) — persists atomically; throws on I/O failure.
    {
        let ctx = ctx.clone();
        engine.register_fn(
            "state_set",
            move |key: &str, value: &str| -> Result<(), Box<EvalAltResult>> {
                store(&ctx)
                    .lock()
                    .unwrap()
                    .set(key, value)
                    .map_err(|e| e.into())
            },
        );
    }
    // state_del(key) — persists atomically; throws on I/O failure.
    {
        let ctx = ctx.clone();
        engine.register_fn("state_del", move |key: &str| -> Result<(), Box<EvalAltResult>> {
            store(&ctx).lock().unwrap().del(key).map_err(|e| e.into())
        });
    }
    // state_all() -> Map
    {
        let ctx = ctx.clone();
        engine.register_fn("state_all", move || -> Map {
            store(&ctx)
                .lock()
                .unwrap()
                .all()
                .into_iter()
                .map(|(k, v)| (k.into(), Dynamic::from(v)))
                .collect()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared_with_state;
    use crate::engine::runner::FakeRunner;

    fn engine_with_disk(root: &std::path::Path) -> (Engine, SharedCtx) {
        let store = StateStore::load(root).unwrap();
        let ctx = shared_with_state(FakeRunner::shared(), store);
        let mut e = Engine::new();
        register(&mut e, ctx.clone());
        (e, ctx)
    }

    #[test]
    fn set_get_del_roundtrip_in_script() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        let out: String = e
            .eval(
                r#"
                state_set("app.version", "v9");
                let v = state_get("app.version");
                state_del("missing-is-fine");
                v
            "#,
            )
            .unwrap();
        assert_eq!(out, "v9");
        // Persisted to disk by the atomic flush.
        let reloaded = StateStore::load(tmp.path()).unwrap();
        assert_eq!(reloaded.get("app.version"), Some("v9".to_string()));
    }

    #[test]
    fn state_get_absent_is_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, _ctx) = engine_with_disk(tmp.path());
        // `if state_get(...)` is false-y on () — script-level absence handling.
        let present: bool = e.eval(r#"if state_get("nope") == () { false } else { true }"#).unwrap();
        assert!(!present);
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --bin nrg engine::builtins::state`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add src/engine/builtins/mod.rs src/engine/builtins/state.rs
git commit -m "feat(state): state_get/set/del/all builtins over the locked StateStore"
```

---

## Task 8: Wire `nrg exec` to lock + load state; integration test

**Files:** Modify `src/cli/exec.rs`; Create `tests/state_lock.rs`

- [ ] **Step 1: Wire lock + load into `execute`**

In `src/cli/exec.rs`, replace the body of `execute` from the `let ssh = …` line onward:

```rust
    use crate::engine::state;

    // Discover the project root and serialize concurrent mutating runs with an advisory
    // flock — UNLESS an ancestor `nrg` already holds it (re-entrancy), to avoid deadlock.
    let root = match state::find_project_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let key = state::lock_key(&root);
    let reentrant = state::lock_is_reentrant(&key, std::env::var(state::LOCK_ENV).ok().as_deref());

    // Keep both the RwLock and its guard alive for the whole run (the guard borrows the
    // RwLock, so they must share this stack frame).
    let mut lock_holder = if reentrant {
        None
    } else {
        match state::open_lock(&root) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("Error: cannot open state lock under {}: {e}", root.display());
                return 1;
            }
        }
    };
    let _guard = match lock_holder.as_mut() {
        Some(l) => match l.write() {
            Ok(g) => Some(g),
            Err(_) => {
                eprintln!(
                    "Error: another `nrg` run is in progress (state lock held under {}).",
                    root.display()
                );
                return 1;
            }
        },
        None => None,
    };
    if !reentrant {
        std::env::set_var(state::LOCK_ENV, &key);
    }

    let store = match state::StateStore::load(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let ssh = SshConfig::load_default();
    let ctx = crate::engine::context::shared_with_state(Arc::new(RealRunner { ssh }), store);

    match crate::engine::eval::run_file(std::path::Path::new(&path), ctx) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
```

Remove the now-unused `use crate::engine::context::shared;` import at the top (keep `RealRunner`, `SshConfig`, `Arc`).

> **Lifetime note for the engineer:** `lock_holder` (owns the `RwLock<File>`) and `_guard` (borrows it) are both locals in `execute`, so the exclusive flock lives until the function returns — covering the entire `run_file`. Do not try to move them into a struct (self-referential).

- [ ] **Step 2: Integration test — corruption aborts, lock serializes**

Create `tests/state_lock.rs`:

```rust
//! Integration: state corruption is fatal, and a normal run persists state atomically.

use assert_cmd::Command;
use std::fs;

#[test]
fn exec_aborts_on_corrupt_state() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(dir.path().join(".energize/state.json"), "{ broken json").unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("k", "v");"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure()
        .stderr(predicates::str::contains("CORRUPT"));
}

#[test]
fn exec_persists_state_atomically() {
    let dir = tempfile::tempdir().unwrap();
    // `.energize` marks the project root and makes this the state home.
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    fs::write(
        dir.path().join("Energize.rhai"),
        r#"state_set("deploy.version", "v123"); print(state_get("deploy.version"));"#,
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .success()
        .stderr(predicates::str::contains("v123"));

    // State landed on disk in the versioned schema; no stray temp file.
    let raw = fs::read_to_string(dir.path().join(".energize/state.json")).unwrap();
    assert!(raw.contains("\"version\""));
    assert!(raw.contains("deploy.version"));
    assert!(!dir.path().join(".energize/state.json.tmp").exists());
}
```

- [ ] **Step 3: Run the integration test**

Run: `cargo test --test state_lock`
Expected: PASS (2 tests).

- [ ] **Step 4: Full suite + engine clippy gate**

Run: `cargo test`
Expected: all green (P0 engine + state + integration; legacy untouched).
Run: `cargo clippy --all-targets 2>&1 | grep -E "src/engine|src/cli/exec"`
Expected: empty (new code clippy-clean; legacy lints remain, deleted in P6).

- [ ] **Step 5: Commit**

```bash
git add src/cli/exec.rs tests/state_lock.rs
git commit -m "feat(cli): nrg exec discovers root, takes re-entrant flock, loads state"
```

---

## Task 9: Acceptance smoke + adversarial review

- [ ] **Step 1: Hand-run nested + concurrent behavior**

```bash
mkdir -p /tmp/nrg-p1/sub && cd /tmp/nrg-p1 && mkdir -p .energize
cat > Energize.rhai <<'EOF'
state_set("count", env_or("N", "1"));
print("count=" + state_get("count"));
print("all=" + state_all().len().to_string());
EOF
# from a subdirectory: root discovery should find /tmp/nrg-p1 (the .energize marker), not sub/
( cd sub && N=7 cargo run -q --manifest-path /Users/inou/dev/nrgize-rs/Cargo.toml -- exec /tmp/nrg-p1/Energize.rhai )
cat /tmp/nrg-p1/.energize/state.json     # should show version:1 + count:7, in /tmp/nrg-p1 not /tmp/nrg-p1/sub
rm -rf /tmp/nrg-p1
```

Expected: `count=7`, a single `state.json` at `/tmp/nrg-p1/.energize/` with `"version": 1`.

- [ ] **Step 2: Adversarial review workflow** (see orchestration in the session) covering: lock re-entrancy correctness, contention behavior, atomic-write crash safety, corruption hard-fail messaging, project-root edge cases (symlinks, $HOME boundary, no-marker default), and the deferred NFS gap. Fold confirmed fix-now items in; defer the rest with notes.

---

## Phase 1 review outcome (adversarial workflow, 2026-06-03)

3-lens review (lock/re-entrancy, durability, edge-cases) + adversarial verification. Core
design held: exclusive flock acquired before `load` and held across the whole run (closing
the inter-process read-modify-write race), atomicity sound, symlink-aliased roots serialize
(flock is per-inode).

**Fixed in P1** (commit after T9):
- **HIGH — re-entrant nested-`nrg` lost update:** parent's stale in-memory map clobbered a
  nested child's writes on whole-map flush. `set`/`del` now reload-from-disk before flushing
  (regression test `set_merges_concurrent_external_writes`).
- Directory `fsync` after rename (rename wasn't crash-durable).
- Reject `version > STATE_VERSION` on load (no silent downgrade-rewrite).
- `has_state(key)` builtin + corrected `state_get` doc (Rhai needs `bool` conditions —
  `if state_get(x) {}` errors; use `!= ()` / `has_state`).
- Refuse `$HOME` as a markerless root (no `$HOME/.energize` scaffolding).
- Contention now prints "Waiting for the state lock…" instead of blocking silently.

**Deferred:** `canonicalize()`-failure could yield a non-canonical lock key that breaks
string re-entrancy detection for alias paths (low, narrow — requires canonicalize to fail on
an existing path); NFS detection (still doc-level). Carry both into a later hardening pass.

## Self-review (author)

- **Spec §8 coverage:** atomic temp+fsync+rename → T4; corruption fatal (missing=empty) → T3; project-root marker (not `.git`, no fallback above `$HOME`) → T2; advisory flock keyed on canonical path → T5; exclusive-for-mutating-run + re-entrant → T8; backup → T4; schema version → T3. **Deferred & documented:** NFS detection (needs platform dep), and "reads use a shared/lock-free snapshot" (no read-only command exists yet in P0/P1 — `StateStore::load` already reads without taking the exclusive lock, so the capability exists; a `nrg state`/status command is future work).
- **Placeholder scan:** none — all steps have concrete code/commands. The NFS deferral is explicit, not a silent gap.
- **Type consistency:** `StateStore::{ephemeral,load,get,set,del,all,flush}`, `find_project_root`, `open_lock`, `lock_key`, `lock_is_reentrant`, `LOCK_ENV`, `shared_with_state`, `RunCtx.state: Arc<Mutex<StateStore>>` — names used identically across T2–T8.
