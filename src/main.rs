mod audit;
mod cli;
mod engine;
mod secrets;
mod ssh;
#[cfg(test)]
mod test_support;
mod trust;

use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();
    cli::set_destination(cli.dest.as_deref());
    if let Some(dest) = &cli.dest {
        eprintln!("[nrg] destination: {}", cli::audit::display_safe(dest));
    }

    let audit_command = match &cli.command {
        Commands::Remove(_) => Some("remove"),
        Commands::Lock(_) => Some("lock"),
        Commands::App(_) => Some("app"),
        Commands::Secrets(_) => Some("secrets"),
        Commands::Setup(args) if !args.dry_run => Some("setup"),
        _ => None,
    };
    let audit_root = audit_command.and_then(|_| engine::state::find_project_root().ok());
    let audit_args = cli::destination()
        .map(|d| vec![format!("--dest={d}")])
        .unwrap_or_default();
    if let (Some(command), Some(root)) = (audit_command, &audit_root) {
        audit::append(
            root,
            &audit::AuditEntry::new(command, "", None, &audit_args, "begin".into()),
        );
    }
    let exit_code = match cli.command {
        Commands::Run(args) => cli::run::execute(&args),
        Commands::Tasks(args) => cli::tasks::execute(&args),
        Commands::Ssh(args) => cli::ssh::execute(&args),
        Commands::Init(args) => cli::init::execute(&args),
        Commands::Doctor(args) => cli::doctor::execute(&args),
        Commands::Secrets(args) => cli::secrets::execute(&args),
        Commands::Exec(args) => cli::exec::execute(&args),
        Commands::Status(args) => cli::status::execute(&args),
        Commands::Audit(args) => cli::audit::execute(&args),
        Commands::Logs(args) => cli::logs::execute(&args),
        Commands::App(args) => cli::app::execute(&args),
        Commands::Remove(args) => cli::remove::execute(&args),
        Commands::Rollback(args) => cli::rollback::execute(&args),
        Commands::Lock(args) => cli::lock::execute(&args),
        Commands::Vendor(args) => cli::vendor::execute(&args),
        Commands::Setup(args) => cli::setup::execute(&args),
    };

    if let (Some(command), Some(root)) = (audit_command, &audit_root) {
        audit::append(
            root,
            &audit::AuditEntry::new(
                command,
                "",
                None,
                &audit_args,
                if exit_code == 0 {
                    "success".into()
                } else {
                    format!("failed (exit {exit_code})")
                },
            ),
        );
    }
    std::process::exit(exit_code);
}
