mod cli;
mod engine;
mod secrets;
mod ssh;

use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Commands::Run(args) => cli::run::execute(&args),
        Commands::Tasks(args) => cli::tasks::execute(&args),
        Commands::Ssh(args) => cli::ssh::execute(&args),
        Commands::Init(args) => cli::init::execute(&args),
        Commands::Doctor(args) => cli::doctor::execute(&args).await,
        Commands::Secrets(args) => cli::secrets::execute(&args),
        Commands::Exec(args) => cli::exec::execute(&args),
    };

    std::process::exit(exit_code);
}
