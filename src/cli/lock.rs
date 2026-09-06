//! `nrg lock status|acquire|release <service> [--host h]` — manual control of the R15
//! cross-machine deploy lock (`lib/deploy.rhai`'s `acquire_deploy_lock`/`release_deploy_lock`),
//! for operators who need to inspect or forcibly clear a held lock without SSHing to the lock
//! host by hand (roadmap 2.1's "still open" item — the Kamal model).
//!
//! The lock is a plain directory: an atomic `mkdir /tmp/nrg-deploy-lock-<service>` on ONE host
//! IS the lock (no separate compare-and-swap needed — `mkdir` either creates it or fails with
//! "File exists"), holding a best-effort `holder` file naming who/when. This command mirrors
//! that exact convention directly in Rust — no Rhai engine involved, the same native-Rust
//! pattern `nrg status`/`nrg logs`/`nrg app exec`/`nrg remove` already use for host management —
//! so a lock taken by a real `deploy()`/`rollback()` call and one taken by `nrg lock acquire`
//! are indistinguishable to each other.
//!
//! CAVEAT: the lock host is the alphabetically-first host of the SPECIFIC `hosts` ARRAY the
//! deploy/rollback call that's holding it was given (`lock_host_for`, `lib/deploy.rhai` — Fable
//! review, full-project pass; originally `hosts[0]`, Opus review round 6 already flagged that as
//! order-dependent) — a transient, in-flight choice never persisted to state.
//! `StateStore::hosts_for(service)` returns every host EVER recorded for the service, sorted
//! alphabetically — NOT scoped to a single deploy's fleet, so it can be a DIFFERENT list than the
//! one the holding call actually used (a fleet that's grown or shrunk since). Auto-picking from
//! that unscoped list would silently target the WRONG host whenever it differs from the holding
//! call's own array — acquiring/releasing a lock that doesn't correspond to any real in-flight
//! deploy, or reporting a false "not locked" while a real lock sits on a different host entirely.
//! So this command only auto-picks when EXACTLY one host is recorded (no ambiguity possible);
//! with more than one, it refuses and lists them, requiring `--host` explicitly.

use crate::engine::runner::{CommandRunner, RawOutput, RealRunner};
use crate::engine::secret::posix_quote;
use crate::engine::state::{self, StateStore};
use clap::{Args, Subcommand};

/// Matches `lib/deploy.rhai`'s `lock_dir = "/tmp/nrg-deploy-lock-" + service` exactly.
fn lock_dir(service: &str) -> String {
    format!("/tmp/nrg-deploy-lock-{service}")
}

#[derive(Args)]
pub struct LockArgs {
    #[command(subcommand)]
    pub command: LockCommand,
}

#[derive(Subcommand)]
pub enum LockCommand {
    /// Show whether a service's deploy lock is currently held, and by whom
    Status(LockTargetArgs),
    /// Manually take a service's deploy lock — e.g. to block automated deploys/rollbacks during
    /// a maintenance window. Does NOT run a deploy.
    Acquire(LockTargetArgs),
    /// Force-release a service's deploy lock
    Release(LockReleaseArgs),
}

#[derive(Args)]
pub struct LockTargetArgs {
    /// Service name (the `service` argument passed to `deploy()`/`rollback()`).
    pub service: String,

    /// Target this host instead of the first host recorded in state for this service.
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Args)]
pub struct LockReleaseArgs {
    /// Service name (the `service` argument passed to `deploy()`/`rollback()`).
    pub service: String,

    /// Target this host instead of the first host recorded in state for this service.
    #[arg(long)]
    pub host: Option<String>,

    /// Actually release the lock. Without this flag, prints what WOULD be released.
    #[arg(long)]
    pub yes: bool,
}

pub fn execute(args: &LockArgs) -> i32 {
    match &args.command {
        LockCommand::Status(t) => cmd_status(t),
        LockCommand::Acquire(t) => cmd_acquire(t),
        LockCommand::Release(r) => cmd_release(r),
    }
}

/// Resolve the target host: `--host` wins outright; otherwise the ONE host recorded for
/// `service` in `.energize/state.json` — refuses to guess when there's more than one, since
/// `hosts_for` returns every host ever recorded, sorted alphabetically, not in deploy order (see
/// the module doc's CAVEAT — Opus review, round 6).
fn resolve_host(service: &str, host: &Option<String>) -> Result<String, String> {
    if let Some(h) = host {
        return Ok(h.clone());
    }
    let root = state::find_project_root()?;
    let store = StateStore::load(&root).map(|s| s.with_dest(crate::cli::destination()))?;
    let mut hosts = store.hosts_for(service);
    match hosts.len() {
        0 => Err(format!(
            "no hosts recorded for {service:?} (has it been deployed?); pass --host to target \
             one directly."
        )),
        1 => Ok(hosts.remove(0)),
        _ => Err(format!(
            "{service:?} has {} hosts recorded ({}) — the real lock host can't be guessed from \
             this list (`deploy()`/`rollback()` anchor the lock on the alphabetically-first host \
             of the SPECIFIC hosts array THAT call was given, which can differ from every host \
             ever recorded for this service — e.g. a fleet that's grown or shrunk since). Pass \
             --host to target one directly.",
            hosts.len(),
            hosts.join(", ")
        )),
    }
}

/// `mkdir ... 2>&1` and `rm -rf ... 2>&1` both redirect the remote command's stderr onto its OWN
/// stdout, so the real failure reason lands in `stdout` — `stderr` only ever carries an
/// ssh-level transport error (already handled separately via `exit_code == 255`). Combining both
/// here (rather than reading `stderr` alone) is what makes `first_reason` actually find it
/// (Fable's final review, round 6 — `cmd_release`'s `--yes` path used to read `stderr` alone and
/// silently fall back to a generic message on a real, distinctive `rm` failure).
fn combined_output(out: &RawOutput) -> String {
    format!("{}{}", out.stdout, out.stderr)
}

/// The first non-blank line of `stderr`, or `fallback` — the same "surface the real reason, not
/// a blank line" idiom `nrg remove`'s `remove_container` uses.
fn first_reason(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn cmd_status(args: &LockTargetArgs) -> i32 {
    let host = match resolve_host(&args.service, &args.host) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let runner = RealRunner;
    let dir = lock_dir(&args.service);
    // Read-only probe — `test -d` never creates or removes anything, unlike `mkdir`.
    let out = runner.run_ssh(&host, &format!("test -d {}", posix_quote(&dir)));
    if out.exit_code == 255 {
        eprintln!(
            "Error: {host}: unreachable — {}",
            first_reason(&out.stderr, "ssh failed")
        );
        return 1;
    }
    // Fable's final review (round 6): only `test -d`'s OWN "absent" exit code (1) means "not
    // locked" — any other nonzero code (a spawn/injection-guard failure reported as `-1`, a
    // signal-killed ssh reported as `128+signal`, ...) is a real failure to even CHECK, not a
    // negative answer, and must not be silently reported as "not locked".
    if out.exit_code == 1 {
        println!("{}: not locked (checked {host})", args.service);
        return 0;
    }
    if out.exit_code != 0 {
        eprintln!(
            "Error: {host}: could not check whether {:?} is locked: {}",
            args.service,
            first_reason(&out.stderr, "test -d failed")
        );
        return 1;
    }
    let holder = read_holder(&runner, &host, &dir);
    println!("{}: LOCKED on {host} by {holder}", args.service);
    println!("  {dir}");
    0
}

fn cmd_acquire(args: &LockTargetArgs) -> i32 {
    let host = match resolve_host(&args.service, &args.host) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let runner = RealRunner;
    let dir = lock_dir(&args.service);
    let out = runner.run_ssh(&host, &format!("mkdir {} 2>&1", posix_quote(&dir)));
    if out.exit_code == 255 {
        eprintln!(
            "Error: {host}: unreachable — {}",
            first_reason(&out.stderr, "ssh failed")
        );
        return 1;
    }
    if out.exit_code != 0 {
        let combined = combined_output(&out);
        if combined.contains("File exists") {
            let holder = read_holder(&runner, &host, &dir);
            eprintln!(
                "Error: {:?} is already locked on {host} by {holder} — a deploy/rollback (or a \
                 previous `nrg lock acquire`) already holds it.",
                args.service
            );
        } else {
            eprintln!(
                "Error: could not acquire the lock on {host}: {}",
                first_reason(&combined, "mkdir failed")
            );
        }
        return 1;
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let ts = now_utc();
    let holder_line = format!("{user} at {ts} (via nrg lock acquire)");
    runner.run_ssh(
        &host,
        &format!(
            "echo {} > {} 2>/dev/null",
            posix_quote(&holder_line),
            posix_quote(&format!("{dir}/holder"))
        ),
    );
    println!("{}: locked on {host} by {holder_line}", args.service);
    println!("Automated deploys/rollbacks of this service will refuse to run until this lock is released.");
    0
}

fn cmd_release(args: &LockReleaseArgs) -> i32 {
    let host = match resolve_host(&args.service, &args.host) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let runner = RealRunner;
    let dir = lock_dir(&args.service);
    if !args.yes {
        let out = runner.run_ssh(&host, &format!("test -d {}", posix_quote(&dir)));
        // Opus review, round 6: an unreachable host (exit 255) is NOT "not locked" — treating it
        // as such would let an operator run the safe preview first, see "nothing to release",
        // and wrongly conclude the lock is already clear when it was never actually checked.
        if out.exit_code == 255 {
            eprintln!(
                "Error: {host}: unreachable — {}",
                first_reason(&out.stderr, "ssh failed")
            );
            return 1;
        }
        // Fable's final review (round 6): same reasoning as cmd_status — only exit code 1
        // (`test -d`'s own "absent" result) means "not locked"; any OTHER nonzero code (spawn
        // failure, the option-injection guard, a signal-killed ssh) is a real failure to check,
        // not a negative answer.
        if out.exit_code != 0 && out.exit_code != 1 {
            eprintln!(
                "Error: {host}: could not check whether {:?} is locked: {}",
                args.service,
                first_reason(&out.stderr, "test -d failed")
            );
            return 1;
        }
        if out.exit_code == 0 {
            let holder = read_holder(&runner, &host, &dir);
            println!(
                "Would release {:?}'s lock on {host} (held by {holder}).",
                args.service
            );
        } else {
            println!(
                "{}: not locked on {host} — nothing to release.",
                args.service
            );
        }
        println!("Re-run with --yes to actually release it. Only do this if you're certain no deploy/rollback of this service is genuinely still running.");
        return 0;
    }
    let out = runner.run_ssh(&host, &format!("rm -rf {} 2>&1", posix_quote(&dir)));
    if out.exit_code == 255 {
        eprintln!(
            "Error: {host}: unreachable — {}",
            first_reason(&out.stderr, "ssh failed")
        );
        return 1;
    }
    if out.exit_code != 0 {
        // Fable's final review (round 6): `rm -rf ... 2>&1` redirects the remote command's
        // stderr onto ITS OWN stdout — the real failure reason lands in `out.stdout`, not
        // `out.stderr` (which only ever carries an ssh-level transport error, already handled
        // above). Matches `cmd_acquire`'s identical combined-output handling below.
        let combined = combined_output(&out);
        eprintln!(
            "Error: could not release the lock on {host}: {}",
            first_reason(&combined, "rm -rf failed")
        );
        return 1;
    }
    println!("{}: released on {host}.", args.service);
    0
}

fn read_holder(runner: &dyn CommandRunner, host: &str, dir: &str) -> String {
    let out = runner.run_ssh(
        host,
        &format!("cat {} 2>/dev/null", posix_quote(&format!("{dir}/holder"))),
    );
    if out.exit_code == 0 && !out.stdout.trim().is_empty() {
        out.stdout.trim().to_string()
    } else {
        "unknown (holder marker missing or unreadable)".to_string()
    }
}

/// UTC timestamp via the `date` binary (mirrors `lib/deploy.rhai`'s own `timestamp()`, which
/// shells out to `date -u` rather than pulling in a date/time dependency), so a holder line
/// written by `nrg lock acquire` reads identically to one `deploy()`/`rollback()` itself writes.
fn now_utc() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S UTC"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown time".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn combined_output_concatenates_stdout_then_stderr() {
        let out = RawOutput {
            stdout: "rm: cannot remove ".to_string(),
            stderr: "'/tmp/x': Permission denied\n".to_string(),
            exit_code: 1,
        };
        assert_eq!(
            combined_output(&out),
            "rm: cannot remove '/tmp/x': Permission denied\n"
        );
    }

    #[test]
    fn read_holder_returns_the_files_contents_when_readable() {
        let runner = FakeRunner::new();
        runner.respond_cmd("web1", "cat", "alice at 2026-07-11 12:00:00 UTC");
        let holder = read_holder(&runner, "web1", "/tmp/nrg-deploy-lock-app");
        assert_eq!(holder, "alice at 2026-07-11 12:00:00 UTC");
    }

    #[test]
    fn read_holder_falls_back_when_the_file_is_missing_or_unreadable() {
        let runner = FakeRunner::new();
        runner.fail_cmd("web1", "cat", 1, "");
        let holder = read_holder(&runner, "web1", "/tmp/nrg-deploy-lock-app");
        assert_eq!(holder, "unknown (holder marker missing or unreadable)");
    }

    #[test]
    fn first_reason_picks_the_first_nonblank_line() {
        assert_eq!(
            first_reason("\n  \nreal reason\nmore\n", "fallback"),
            "real reason"
        );
    }

    #[test]
    fn first_reason_falls_back_on_entirely_blank_stderr() {
        assert_eq!(first_reason("\n  \n", "fallback"), "fallback");
    }

    #[test]
    fn lock_dir_matches_the_stdlibs_own_convention() {
        assert_eq!(lock_dir("app"), "/tmp/nrg-deploy-lock-app");
    }
}
