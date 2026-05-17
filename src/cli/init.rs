use clap::Args;
use std::path::Path;

use crate::cli::ui;

#[derive(Args)]
pub struct InitArgs {}

const BASH_TEMPLATE: &str = r#"# @servers local=127.0.0.1 production=user@example.com

APP_ENV="production"

# @before
notify_start() {
    echo "Starting deployment..."
}

# @task on:production
deploy() {
    cd /var/www/app
    git pull origin main
    echo "Deployed!"
}

# @after
notify_end() {
    echo "Deployment finished."
}

# @macro full-deploy deploy
"#;

const STARLARK_TEMPLATE: &str = r#"servers(
    local = "127.0.0.1",
    production = "user@example.com",
)

APP_ENV = "production"

before(script = 'echo "Starting deployment..."')

task(
    name = "deploy",
    on = ["production"],
    script = """
        cd /var/www/app
        git pull origin main
        echo "Deployed!"
    """,
)

after(script = 'echo "Deployment finished."')

define_macro(
    name = "full-deploy",
    tasks = ["deploy"],
)
"#;

pub fn execute(_args: &InitArgs) -> i32 {
    // Prompt for format
    let formats = vec!["Starlark (.star)", "Bash (.sh)"];
    let format_choice = dialoguer::Select::new()
        .with_prompt("Select task file format")
        .items(&formats)
        .default(0)
        .interact()
        .unwrap_or(0);

    let (filename, template) = match format_choice {
        0 => ("Energize.star", STARLARK_TEMPLATE),
        1 => ("Energize.sh", BASH_TEMPLATE),
        _ => unreachable!(),
    };

    // Check if file already exists
    if Path::new(filename).exists() {
        ui::render_error(&format!("{} already exists.", filename));
        return 1;
    }

    // Write the template
    match std::fs::write(filename, template) {
        Ok(_) => {
            ui::render_success(&format!("Created {}", filename));
            0
        }
        Err(e) => {
            ui::render_error(&format!("Failed to write {}: {}", filename, e));
            1
        }
    }
}
