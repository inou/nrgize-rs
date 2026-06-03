pub mod doctor;
pub mod exec;
pub mod init;
pub mod run;
pub mod secrets;
pub mod ssh;
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
}
