//! Starlark built-in functions for cross-run state persistence.
//!
//! State is stored as a JSON object in `.energize/state.json` relative to the
//! current working directory. This allows Starlark scripts to track deployment
//! versions, timestamps, and other metadata across invocations.

use starlark::environment::GlobalsBuilder;
use starlark::values::none::NoneType;
use starlark::values::Heap;
use starlark::values::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Get the path to the state file.
fn state_file_path() -> PathBuf {
    let dir = PathBuf::from(".energize");
    dir.join("state.json")
}

/// Load state from disk. Returns empty map if file doesn't exist.
fn load_state() -> HashMap<String, String> {
    let path = state_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Save state to disk. Creates .energize/ directory if needed.
fn save_state(state: &HashMap<String, String>) -> anyhow::Result<()> {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create .energize directory: {}", e))?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|e| anyhow::anyhow!("Failed to serialize state: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("Failed to write state file: {}", e))?;
    Ok(())
}

/// Register state built-in functions into the Starlark global environment.
#[starlark::starlark_module]
pub fn state_builtins(builder: &mut GlobalsBuilder) {
    /// Get a value from persistent state by key.
    ///
    /// Returns the string value, or None if the key doesn't exist.
    ///
    /// Example:
    ///   current = state_get("current_version")
    ///   if current:
    ///       print("Currently deployed: " + current)
    fn state_get<'v>(
        key: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let state = load_state();
        match state.get(key) {
            Some(val) => Ok(heap.alloc(val.as_str())),
            None => Ok(heap.alloc(NoneType)),
        }
    }

    /// Set a value in persistent state.
    ///
    /// The value is immediately written to `.energize/state.json`.
    ///
    /// Example:
    ///   state_set("previous_version", state_get("current_version") or "none")
    ///   state_set("current_version", TAG)
    fn state_set(
        key: &str,
        value: &str,
    ) -> anyhow::Result<NoneType> {
        let mut state = load_state();
        state.insert(key.to_string(), value.to_string());
        save_state(&state)?;
        Ok(NoneType)
    }

    /// Delete a key from persistent state.
    ///
    /// Example:
    ///   state_del("current_version")
    fn state_del(
        key: &str,
    ) -> anyhow::Result<NoneType> {
        let mut state = load_state();
        state.remove(key);
        save_state(&state)?;
        Ok(NoneType)
    }

    /// Return all state as a dict.
    ///
    /// Example:
    ///   all = state_all()
    ///   for k in all:
    ///       print(k + " = " + all[k])
    fn state_all<'v>(
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        let state = load_state();
        let pairs: Vec<(Value<'v>, Value<'v>)> = state
            .iter()
            .map(|(k, v)| (heap.alloc(k.as_str()), heap.alloc(v.as_str())))
            .collect();
        Ok(heap.alloc(starlark::values::dict::AllocDict(pairs)))
    }
}
