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

    /// Roll back to this image instead of the stdlib's snapshotted `<service>.prev`. Must not be
    /// empty/blank if given (an unset shell variable expanding to `""` is refused rather than
    /// silently falling back to `.prev` — Fable review, round 5).
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
    // Fable review, round 5: an unset shell variable (`--image "$TAG"` where $TAG is empty)
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
    // file; `nrg rollback` never reads this file's contents at all (only its directory, for
    // module resolution below) — so nothing else would ever catch a nonexistent path, silently
    // resolving `lib/deploy.rhai` against whatever happens to sit in the typo'd parent directory
    // instead (Fable review, round 5).
    if !std::path::Path::new(&path).is_file() {
        eprintln!("Error: {path:?} does not exist (or is not a file).");
        return 1;
    }

    // Fable review, round 5: the audit trail must record which hosts/image this rollback
    // actually targeted — a bare `"target":"app","args":[]` entry is indistinguishable from the
    // default (roll every recorded host back to `.prev`), losing exactly the facts an audit
    // trail exists for on the one command that's reached for during an incident.
    let mut audit_args: Vec<String> = args.host.iter().map(|h| format!("--host={h}")).collect();
    if let Some(img) = &args.image {
        audit_args.push(format!("--image={img}"));
    }
    let meta = AuditMeta { command: "rollback", target: Some(&args.service), args: &audit_args };
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
            // `Path::new("Energize.rhai").parent()` returns `Some("")`, not `None` — the empty
            // component must be treated the same as "no parent" (CWD), or a bare relative
            // `--file` renders a confusing `"" has no lib/deploy.rhai"` error instead of `"."`'s
            // (Fable review, round 5). Matches `build_for`'s identical fallback in `eval.rs`.
            let import_root = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| ".".into());
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
