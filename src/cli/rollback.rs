//! `nrg rollback <service> [--host h]... [--image tag] [--dry-run] [--lock-timeout secs]` —
//! roll a service back via the stdlib's `deploy::rollback(hosts, service, cfg)`, without
//! requiring the project's own `Energize.rhai` to define a wrapper function around it (roadmap
//! 3.3 — `nrg run rollback` already worked, but only for projects that had hand-written one).
//!
//! Hosts default to every host `.energize/state.json` records for the service (the same
//! `StateStore::hosts_for` lookup `nrg remove` uses) — `--host` (repeatable) overrides that to a
//! specific subset. `--image` overrides the stdlib's own snapshotted `<service>.prev`; omit it
//! to use the persisted rollback target (or the stdlib's own error if none exists — e.g. a
//! mutable `:latest` `.prev`, robustness review R10).
//!
//! This reuses `nrg exec`/`nrg run`'s shared `execute_with`/`wire_run` wiring (state lock, R7
//! SIGINT/SIGTERM interrupt handling, `--dry-run` overlay support, audit-trail logging) rather
//! than reimplementing any of it — `deploy()` (which `rollback()` calls internally) is a real,
//! side-effecting, interruptible operation, unlike `nrg remove`'s single idempotent `docker rm`.

use crate::cli::exec::{execute_with, resolve_file, AuditMeta};
use clap::Args;

#[derive(Args)]
pub struct RollbackArgs {
    /// Service name (the `service` argument passed to `deploy()`).
    pub service: String,

    /// Roll back only this host, instead of every host recorded in state. Repeatable.
    #[arg(long)]
    pub host: Vec<String>,

    /// Roll back to this image instead of the stdlib's snapshotted `<service>.prev`.
    #[arg(long)]
    pub image: Option<String>,

    /// Path to the `.rhai` file whose directory anchors `import "lib/deploy"` resolution — its
    /// CONTENTS are never read or run, only its directory. Defaults to the project's
    /// Energize.rhai (or energize.rhai), whose directory is normally the project root, where
    /// `lib/` lives; pointing `--file` at a file in a DIFFERENT directory genuinely changes
    /// which `lib/` copy is used (matching `nrg exec --file`'s own module-resolution rules), not
    /// just which path is recorded in the audit trail.
    #[arg(long)]
    pub file: Option<String>,

    /// Show the plan of side effects without executing (no lock, no state writes).
    #[arg(long)]
    pub dry_run: bool,

    /// Give up waiting for the state lock after this many seconds (another `nrg` run holding it
    /// is reported as an error instead of blocking forever). Default: wait indefinitely.
    #[arg(long)]
    pub lock_timeout: Option<u64>,
}

pub fn execute(args: &RollbackArgs) -> i32 {
    let path = match resolve_file(
        &args.file,
        "Create one or pass a file:\n  nrg rollback <service> --file deploy.rhai",
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let meta = AuditMeta { command: "rollback", target: Some(&args.service), args: &[] };
    execute_with(
        &path,
        args.dry_run,
        args.lock_timeout.map(std::time::Duration::from_secs),
        meta,
        |path, ctx| {
            // Deliberately DIFFERENT from `nrg remove`'s "no hosts recorded" handling (which
            // prints to stdout and exits 0 — a no-op success, since "nothing to remove" already
            // matches the goal state): here it's a real error. Rolling back is an action that
            // needs a target; a service with no known deployment has nothing to roll back to.
            let hosts = if args.host.is_empty() {
                ctx.state.lock().unwrap().hosts_for(&args.service)
            } else {
                args.host.clone()
            };
            if hosts.is_empty() {
                return Err(format!(
                    "no hosts recorded for {:?} (has it been deployed?); pass --host to target \
                     one directly.",
                    args.service
                ));
            }
            if let Some(bad) = hosts.iter().find(|h| h.trim().is_empty()) {
                return Err(format!(
                    "a --host value cannot be empty or blank (got {bad:?}); pass a real host \
                     alias or address."
                ));
            }
            // Module resolution (`import "lib/deploy"`) is anchored at THIS file's own
            // directory — the same directory `Energize.rhai` itself resolves its `lib/*.rhai`
            // imports relative to (matching `nrg exec`/`nrg run`'s `build_for`), and honoring
            // `--file` the same way those commands do: pointing `--file` elsewhere genuinely
            // changes which `lib/` is used, not just which path is recorded in the audit trail.
            let import_root = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| ".".into());
            crate::engine::eval::run_rollback(
                &import_root,
                &hosts,
                &args.service,
                args.image.as_deref(),
                ctx,
            )
        },
    )
}
