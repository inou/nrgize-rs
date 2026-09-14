//! Explicit declarations; shell text is never inspected to guess its effects.
use crate::engine::{
    context::SharedCtx,
    diagnostics,
    secret::{assert_no_secret_leak, posix_quote},
};
use rhai::{Array, Engine, EvalAltResult};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_directory")]
    pub directory: String,
    #[serde(default = "default_rsync")]
    pub tool: String,
}
fn default_directory() -> String {
    ".".into()
}
fn default_rsync() -> String {
    "rsync".into()
}

/// Only our known temporary-write checks get automatic cleanup. No arbitrary shell
/// operation can claim this guarantee by labelling itself temporary.
pub fn temporary_command(check: &Check) -> String {
    let prefix = format!("set -eu; umask 077; base={}; d=$(mktemp -d \"$base/.nrg-check.XXXXXXXXXX\"); trap 'rm -rf \"$d\"' 0; trap 'exit 1' HUP INT TERM; ", posix_quote(&check.directory));
    let stat = "mode=$(stat -c %a \"$d/dest\" 2>/dev/null || stat -f %Lp \"$d/dest\"); [ \"$mode\" = 600 ]; cmp \"$d/source\" \"$d/dest\"; ";
    let copy = if check.kind == "rsync-permissions" {
        format!(
            "{} -a --ignore-times --chmod=Fu+rw,Fu-x,Fgo-rwx -- \"$d/source\" \"$d/dest\"; ",
            posix_quote(&check.tool)
        )
    } else {
        let cmd = super::transfer::atomic_write_command("dest", 0o600, 21);
        format!("(cd \"$d\"; {}) < \"$d/source\"; ", cmd)
    };
    format!("{prefix}printf 'nrg permission probe\\n' > \"$d/source\"; chmod 777 \"$d/source\"; {copy}{stat}chmod 666 \"$d/dest\"; {copy}{stat}")
}

pub fn run(ctx: &SharedCtx, checks: &[Check], allow_temporary: bool) -> Result<(), String> {
    // Validate the entire declaration before executing any probes.
    for c in checks {
        if !matches!(
            c.kind.as_str(),
            "read-only" | "syntax" | "file-permissions" | "rsync-permissions"
        ) {
            return Err(format!("unknown preflight kind: {}", c.kind));
        }
        if matches!(c.kind.as_str(), "read-only" | "syntax") && c.command.is_empty() {
            return Err(format!("{} requires a command", c.name));
        }
        if c.kind.ends_with("permissions") && !allow_temporary && !ctx.is_dry_run() {
            return Err(format!(
                "{} is a temporary-write check; explicitly enable temporary checks",
                c.name
            ));
        }
        for value in [
            &c.name,
            &c.command,
            &c.directory,
            &c.tool,
            c.host.as_deref().unwrap_or(""),
        ] {
            assert_no_secret_leak(value).map_err(|e| e.to_string())?;
        }
    }
    for c in checks {
        let temporary = c.kind.ends_with("permissions");
        let class = if temporary {
            "temporary-write"
        } else {
            &c.kind
        };
        if temporary && ctx.is_dry_run() {
            ctx.record(
                "preflight",
                c.host.as_deref(),
                format!("{} [temporary-write skipped]", c.name),
            );
            continue;
        }
        let cmd = if temporary {
            temporary_command(c)
        } else {
            c.command.clone()
        };
        let started = diagnostics::begin(ctx, &c.name, c.host.as_deref().unwrap_or(""), class);
        let raw = if c.kind == "syntax" {
            match c.host.as_deref() {
                Some(h) => ctx.runner.run_ssh_stdin(h, "bash -n", &cmd),
                None => ctx.runner.run_local_stdin("bash -n", &cmd),
            }
        } else {
            let secrets: Vec<_> = ctx.secrets.lock().unwrap().iter().cloned().collect();
            ctx.runner
                .run_observed(c.host.as_deref(), &cmd, &secrets, false)
        };
        let result = super::exec::to_result(c.host.as_deref().unwrap_or(""), raw);
        let message = diagnostics::record_started(ctx, &c.name, class, &result, started);
        ctx.check_interrupt().map_err(|e| e.to_string())?;
        eprintln!(
            "[preflight:{class}] {}: {}",
            ctx.redacted(&c.name),
            if result.exit_code == 0 {
                "passed"
            } else {
                "FAILED"
            }
        );
        if result.exit_code != 0 {
            return Err(message);
        }
    }
    Ok(())
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    engine.register_fn(
        "preflight",
        move |checks: Array, allow_temporary: bool| -> Result<(), Box<EvalAltResult>> {
            let checks: Vec<Check> =
                rhai::serde::from_dynamic(&checks.into()).map_err(|e| e.to_string())?;
            run(&ctx, &checks, allow_temporary).map_err(Into::into)
        },
    );
}
