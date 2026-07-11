//! Test-only support shared across `#[cfg(test)]` unit tests in this binary.
//!
//! Robustness review: "Flaky patterns" — several unit tests (`runner.rs`'s host-key test,
//! `secret.rs`'s secret-env tests, `exec.rs`'s `NRG_SECRET_LEAK` test) mutate PROCESS-GLOBAL
//! environment variables (`std::env::set_var`/`remove_var`) while cargo runs test threads in
//! parallel by default. `set_var` racing with another thread's `set_var` (or even a plain
//! `std::env::var` read) is not just a logic race — glibc's `getenv`/`setenv` aren't documented
//! as thread-safe against each other, which is exactly why later Rust editions mark
//! `set_var`/`remove_var` `unsafe`. `ENV_MUTEX` serializes every env-mutating test in THIS binary
//! against every OTHER one, closing the highest-probability case (two of our own tests racing).

#[cfg(test)]
pub static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared env-test mutex, recovering from a poisoned lock (an earlier env-mutating
/// test panicking mid-mutation must not permanently block every later one from ever running).
#[cfg(test)]
pub fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}
