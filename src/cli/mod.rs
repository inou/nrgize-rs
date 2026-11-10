pub mod app;
pub mod audit;
pub mod doctor;
pub mod exec;
pub mod init;
pub mod lock;
pub mod logs;
pub mod rehearse;
pub mod remove;
pub mod rollback;
pub mod run;
pub mod secrets;
pub mod setup;
pub mod ssh;
pub mod status;
pub mod tasks;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nrg",
    about = "Energize — a Rhai-scripted deployment toolkit: SSH orchestration, directory releases and optional health-gated container rollouts",
    version
)]
pub struct Cli {
    /// Deployment destination: namespaces state and secrets (e.g. staging, production; letters,
    /// digits, - and _). Accepted before or after the subcommand. Defaults to the unnamespaced
    /// destination. Not accepted by rehearse. For `rollback` it must match the `--dest` the original
    /// deploy used, or hosts and `.prev` resolve against an empty namespace.
    #[arg(long, global = true)]
    pub dest: Option<String>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Validate recipe contracts, or execute them in a fresh local temporary workspace
    Rehearse(rehearse::RehearseArgs),
    /// Call a function defined in the Rhai orchestration file
    Run(run::RunArgs),
    /// List the functions defined in the Rhai orchestration file
    Tasks(tasks::TasksArgs),
    /// Open an interactive SSH session to a host
    Ssh(ssh::SshArgs),
    /// Scaffold a new Energize.rhai orchestration file
    Init(init::InitArgs),
    /// Validate the orchestration file compiles and declared capabilities are met (tool checks need --legacy-tools)
    Doctor(doctor::DoctorArgs),
    /// Manage encrypted secrets (age encryption)
    Secrets(secrets::SecretsArgs),
    /// Evaluate a file's top level; useful for one-off scripts
    Exec(exec::ExecArgs),
    /// Show the deployed version, image, and per-host container state for a service
    Status(status::StatusArgs),
    /// Show the audit trail of past `nrg exec`/`nrg run` invocations
    Audit(audit::AuditArgs),
    /// Tail a service's container logs across its deployed hosts
    Logs(logs::LogsArgs),
    /// Operate on a service's live container (exec, console)
    App(app::AppArgs),
    /// Stop and remove a service's container from its deployed hosts
    Remove(remove::RemoveArgs),
    /// Roll a service back to a previous image, using the stdlib directly (no script wiring needed)
    Rollback(rollback::RollbackArgs),
    /// Manually inspect/acquire/release a service's cross-machine deploy lock
    Lock(lock::LockArgs),
    /// Bootstrap a fresh host: install Docker if absent, create the network, boot the proxy
    Setup(setup::SetupArgs),
}

/// clap resolves a repeated global `--dest` to the last occurrence without complaint, so check
/// the raw argv: repeating the flag is fine only if every occurrence agrees.
pub fn check_dest_conflict(args: &[String]) -> Result<(), String> {
    let mut seen: Option<&str> = None;
    let mut it = args.iter().skip(1).map(String::as_str);
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        let value = match a {
            "--dest" => it.next(),
            _ => a.strip_prefix("--dest="),
        };
        if let Some(v) = value {
            match seen {
                Some(prev) if prev != v => {
                    return Err(format!(
                        "conflicting --dest values {prev:?} and {v:?}; give --dest once"
                    ));
                }
                _ => seen = Some(v),
            }
        }
    }
    Ok(())
}

static DESTINATION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
pub fn set_destination(dest: Option<&str>) {
    let _ = DESTINATION.set(dest.map(str::to_string));
}
pub fn destination() -> Option<String> {
    DESTINATION.get().cloned().flatten()
}
