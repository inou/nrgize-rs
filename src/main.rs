mod cli;
mod engine;
mod execution;
mod parsing;
mod runtime;
mod secrets;
mod ssh;

use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    // Register panic hook to restore terminal state
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show
        );
        default_hook(info);
    }));

    let cli = Cli::parse();

    let exit_code = match cli.command {
        Commands::Run(args) => cli::run::execute(&args).await,
        Commands::Tasks(args) => cli::tasks::execute(&args),
        Commands::Ssh(args) => cli::ssh::execute(&args),
        Commands::Init(args) => cli::init::execute(&args),
        Commands::Doctor(args) => cli::doctor::execute(&args).await,
        Commands::Secrets(args) => cli::secrets::execute(&args),
        Commands::Exec(args) => cli::exec::execute(&args),
    };

    std::process::exit(exit_code);
}
