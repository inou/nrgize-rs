//! `nrg tasks` — list the functions defined in the `Energize.rhai` orchestration file.
//! Each function is a callable entry point for `nrg run <fn>`.

use crate::cli::exec::resolve_file;
use crate::engine::eval;
use clap::Args;
use crossterm::style::Stylize;

#[derive(Args)]
pub struct TasksArgs {
    /// Path to the `.rhai` file. Defaults to Energize.rhai.
    #[arg(long)]
    pub file: Option<String>,
}

pub fn execute(args: &TasksArgs) -> i32 {
    let path = match resolve_file(&args.file, "Create one or pass a file with --file.") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let fns = match eval::list_functions(std::path::Path::new(&path)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    if fns.is_empty() {
        println!("No functions defined in {}.", path);
        return 0;
    }

    println!("\n{}", "Functions:".bold());
    for f in &fns {
        let args_label = match f.params {
            0 => String::new(),
            n => format!("({} arg{})", n, if n == 1 { "" } else { "s" }),
        };
        println!("  {} {}", f.name.as_str().green(), args_label.dark_grey());
    }
    println!();
    0
}
