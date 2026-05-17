use clap::Args;
use crossterm::style::Stylize;

use crate::cli::ui;
use crate::parsing;

#[derive(Args)]
pub struct TasksArgs {
    /// Explicit file path
    #[arg(long)]
    pub path: Option<String>,

    /// Filename in current directory
    #[arg(long)]
    pub conf: Option<String>,
}

pub fn execute(args: &TasksArgs) -> i32 {
    let file_path = match parsing::resolve_file(args.path.as_deref(), args.conf.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    let config = match parsing::parse_file(&file_path, &std::collections::HashMap::new()) {
        Ok(c) => c,
        Err(e) => {
            ui::render_error(&e.to_string());
            return 1;
        }
    };

    // Display macros
    if !config.macros.is_empty() {
        println!("\n{}", "Macros:".bold());
        for (name, macro_def) in &config.macros {
            let tasks_str = macro_def.tasks.join(" → ");
            println!("  {} {}", name.as_str().green(), tasks_str.dark_grey());
        }
    }

    // Display tasks
    if !config.tasks.is_empty() {
        println!("\n{}", "Tasks:".bold());
        for (_name, task) in &config.tasks {
            let display = task.display_name_with_emoji();
            let parallel = if task.parallel { " [parallel]" } else { "" };

            let target = if task.local {
                "local".to_string()
            } else if task.upload.is_some() {
                format!("upload → {}", task.servers.join(", "))
            } else {
                task.servers.join(", ")
            };

            println!(
                "  {} → {}{}",
                display,
                target.dark_grey(),
                parallel.cyan()
            );
        }
    }

    if config.tasks.is_empty() && config.macros.is_empty() {
        println!("No tasks or macros defined.");
    }

    println!();
    0
}
