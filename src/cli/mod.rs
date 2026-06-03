pub mod doctor;
pub mod exec;
pub mod init;
pub mod run;
pub mod secrets;
pub mod ssh;
pub mod tasks;
pub mod ui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nrg", about = "Energize — A beautiful SSH task runner", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Execute a task or macro
    Run(run::RunArgs),
    /// List available tasks and macros
    Tasks(tasks::TasksArgs),
    /// Open an SSH session to a defined server
    Ssh(ssh::SshArgs),
    /// Scaffold a new task file
    Init(init::InitArgs),
    /// Validate configuration and connectivity
    Doctor(doctor::DoctorArgs),
    /// Manage encrypted secrets (age encryption)
    Secrets(secrets::SecretsArgs),
    /// Evaluate a Rhai file in orchestration mode with runtime primitives
    Exec(exec::ExecArgs),
}
