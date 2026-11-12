//! `nrg rollback <service> [--host h]... [--image tag] [--undo-pre-deploy] [--dry-run] [--lock-timeout secs]` —
//! roll a service back via the stdlib's `deploy::rollback(hosts, service, cfg)`, without
//! requiring the project's own `Energize.rhai` to define a wrapper function around it (`nrg run rollback` already worked, but only for projects that had hand-written one).
//!
//! Hosts default to every host `.energize/state.json` records for the service (the same
//! `StateStore::hosts_for` lookup `nrg remove` uses) — `--host` (repeatable) overrides that to a
//! specific subset. `--image` overrides the stdlib's own snapshotted `<service>.prev`; omit it
//! to use the persisted rollback target (or the stdlib's own error if none exists — e.g. a
//! mutable `:latest` `.prev`).
//!
//! This reuses `nrg exec`/`nrg run`'s shared `execute_with`/`wire_run` wiring (state lock,
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

    /// Roll back to this image instead of the stdlib's snapshotted `<service>.prev`. Must not be
    /// empty/blank if given (an unset shell variable expanding to `""` is refused rather than
    /// silently falling back to `.prev`).
    #[arg(long)]
    pub image: Option<String>,

    /// After the containers are back on the previous image, also undo the migration of the release
    /// being rolled back, using the `pre_deploy_undo` that release was deployed with. Refused
    /// (before anything is touched) when that release has none saved.
    #[arg(long)]
    pub undo_pre_deploy: bool,

    /// Path to the project's `.rhai` file — its CONTENTS are never read or run; it anchors the
    /// project state and audit trail. Defaults to the project's Energize.rhai (or energize.rhai).
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
    // An unset shell variable (`--image "$TAG"` where $TAG is empty)
    // must NOT silently fall back to the stdlib's automatic `.prev` lookup — that's a different
    // rollback target than the caller almost certainly intended, and the failure is silent
    // (exit 0) since an empty override is otherwise indistinguishable from "no override given"
    // (see `run_rollback`'s `if __nrg_image != ""` check).
    if let Some(img) = &args.image {
        if img.trim().is_empty() {
            eprintln!(
                "Error: --image cannot be empty or blank; omit the flag entirely to use the \
                 stdlib's snapshotted <service>.prev instead."
            );
            return 1;
        }
    }

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
    // `resolve_file` accepts an explicit `--file` path exactly as given, without checking it
    // actually exists. `nrg exec`/`nrg run` catch a typo'd path naturally, since they COMPILE the
    // file; `nrg rollback` never reads this file's contents, so nothing else would catch a
    // nonexistent path.
    if !std::path::Path::new(&path).is_file() {
        eprintln!("Error: {path:?} does not exist (or is not a file).");
        return 1;
    }

    // The audit trail must record which hosts/image this rollback
    // actually targeted — a bare `"target":"app","args":[]` entry is indistinguishable from the
    // default (roll every recorded host back to `.prev`), losing exactly the facts an audit
    // trail exists for on the one command that's reached for during an incident.
    let mut audit_args: Vec<String> = args.host.iter().map(|h| format!("--host={h}")).collect();
    if let Some(img) = &args.image {
        audit_args.push(format!("--image={img}"));
    }
    if args.undo_pre_deploy {
        audit_args.push("--undo-pre-deploy".to_string());
    }
    let meta = AuditMeta {
        command: "rollback",
        target: Some(&args.service),
        args: &audit_args,
    };
    execute_with(
        &path,
        args.dry_run,
        args.lock_timeout.map(std::time::Duration::from_secs),
        crate::cli::destination(),
        meta,
        |_, ctx| {
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
                // Name the destination actually checked — otherwise a user
                // who forgot `--dest <name>` (the service was really deployed under a NAMED
                // destination) sees a generic "no hosts recorded" with no hint that it only ever
                // checked the DEFAULT namespace, which is confusing on the one command reached
                // for during an incident.
                let dest = ctx
                    .state
                    .lock()
                    .unwrap()
                    .dest()
                    .unwrap_or_else(|| "default".to_string());
                return Err(format!(
                    "no hosts recorded for {:?} in destination {dest:?} (has it been deployed — \
                     under this destination? pass --dest if it was deployed under a different \
                     one); pass --host to target one directly.",
                    args.service
                ));
            }
            if let Some(bad) = hosts.iter().find(|h| h.trim().is_empty()) {
                return Err(format!(
                    "a --host value cannot be empty or blank (got {bad:?}); pass a real host \
                     alias or address."
                ));
            }
            crate::engine::eval::run_rollback(
                &hosts,
                &args.service,
                args.image.as_deref(),
                args.undo_pre_deploy,
                ctx,
            )
        },
    )
}
