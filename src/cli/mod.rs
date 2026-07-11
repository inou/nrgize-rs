pub mod app;
pub mod audit;
pub mod doctor;
pub mod exec;
pub mod init;
pub mod lock;
pub mod logs;
pub mod remove;
pub mod rollback;
pub mod run;
pub mod secrets;
pub mod ssh;
pub mod status;
pub mod tasks;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nrg", about = "Energize — A Rhai-powered SSH orchestration runner", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Call a function defined in the Rhai orchestration file
    Run(run::RunArgs),
    /// List the functions defined in the Rhai orchestration file
    Tasks(tasks::TasksArgs),
    /// Open an interactive SSH session to a host
    Ssh(ssh::SshArgs),
    /// Scaffold a new Energize.rhai orchestration file
    Init(init::InitArgs),
    /// Validate the orchestration file compiles and required tools are installed
    Doctor(doctor::DoctorArgs),
    /// Manage encrypted secrets (age encryption)
    Secrets(secrets::SecretsArgs),
    /// Evaluate a Rhai orchestration file top-to-bottom with runtime primitives
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
    /// Manually inspect/acquire/release a service's cross-machine deploy lock (robustness review R15)
    Lock(lock::LockArgs),
}
