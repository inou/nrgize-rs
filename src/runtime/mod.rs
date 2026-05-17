//! Runtime primitives for Starlark orchestration mode.
//!
//! This module provides built-in functions that make Starlark a deployment
//! orchestration runtime rather than just a configuration language. Functions
//! have side effects (SSH execution, HTTP requests, file I/O) and return
//! results that Starlark code can branch on.
//!
//! # Built-in functions
//!
//! ## Execution
//! - `ssh_exec(host, cmd)` → ExecResult — run a command on a remote host
//! - `local_exec(cmd)` → ExecResult — run a command locally
//! - `ssh_exec_all(hosts, cmd)` → [ExecResult] — run a command on multiple hosts in parallel
//!
//! ## HTTP
//! - `http_get(url)` → HttpResponse — GET request (for health checks)
//! - `http_post(url, body)` → HttpResponse — POST request (for webhooks)
//!
//! ## File Transfer
//! - `upload(host, local_path, remote_path)` — SCP a local file to remote host
//! - `write_remote(host, content, remote_path)` — write a string to a remote file
//!
//! ## State
//! - `state_get(key)` → string|None — read persistent state
//! - `state_set(key, value)` — write persistent state
//! - `state_del(key)` — delete a state key
//! - `state_all()` → dict — read all state
//!
//! ## Utilities
//! - `sleep(seconds)` — blocking delay
//! - `nrg_env(name)` → string — get env var (fails if unset)
//! - `env_or(name, default)` → string — get env var with default
//! - `secret(name)` → string — get secret from env/file

pub mod exec;
pub mod http;
pub mod loader;
pub mod state;
pub mod transfer;
pub mod types;
pub mod util;

use starlark::environment::GlobalsBuilder;

/// Register all runtime built-in functions into a GlobalsBuilder.
///
/// Call this when setting up the Starlark environment for "exec" (script) mode.
///
/// ```ignore
/// let globals = GlobalsBuilder::standard()
///     .with(runtime::register_all)
///     .build();
/// ```
pub fn register_all(builder: &mut GlobalsBuilder) {
    exec::exec_builtins(builder);
    http::http_builtins(builder);
    transfer::transfer_builtins(builder);
    state::state_builtins(builder);
    util::util_builtins(builder);
}
