use crossterm::style::{Color, Stylize};
use crate::parsing::models::TaskResult;
use indexmap::IndexMap;

/// Braille spinner frames.
#[allow(dead_code)]
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Color rotation for server names.
#[allow(dead_code)]
pub const SERVER_COLORS: &[Color] = &[
    Color::Yellow,
    Color::Cyan,
    Color::Magenta,
    Color::Blue,
    Color::Green,
];

/// Get a color for a server by index.
#[allow(dead_code)]
pub fn server_color(index: usize) -> Color {
    SERVER_COLORS[index % SERVER_COLORS.len()]
}

/// Render a task header line.
#[allow(dead_code)]
pub fn render_task_header(task_name: &str, emoji: Option<&str>, index: usize, total: usize) {
    let prefix = match emoji {
        Some(e) => format!("{} ", e),
        None => String::new(),
    };
    println!(
        "\n{}[{}/{}] {}",
        prefix,
        index + 1,
        total,
        task_name.bold()
    );
}

/// Render an output line with server name prefix.
#[allow(dead_code)]
pub fn render_output_line(server_name: &str, line: &str, color: Color) {
    println!("  {} {}", format!("[{}]", server_name).with(color), line);
}

/// Render the result summary table.
pub fn render_result_table(results: &IndexMap<String, TaskResult>) {
    println!("\n{}", "Results:".bold());
    println!("{}", "─".repeat(60));

    for (name, result) in results {
        let status = if result.succeeded() {
            "✓".green().to_string()
        } else {
            "✗".red().to_string()
        };

        let duration = format!("{:.2}s", result.duration.as_secs_f64());

        let host_info = match &result.failed_host {
            Some(host) => format!(" (failed: {})", host).red().to_string(),
            None => String::new(),
        };

        println!("  {} {} {} {}", status, name, duration.dark_grey(), host_info);
    }

    println!("{}", "─".repeat(60));
}

/// Render the application banner.
#[allow(dead_code)]
pub fn render_banner() {
    println!(
        "{}",
        "nrg — Energize SSH task runner".bold()
    );
}

/// Render an error message.
pub fn render_error(message: &str) {
    eprintln!("{} {}", "Error:".red().bold(), message);
}

/// Render a success message.
pub fn render_success(message: &str) {
    println!("{} {}", "✓".green(), message);
}

/// Render a warning message.
pub fn render_warning(message: &str) {
    println!("{} {}", "⚠".yellow(), message);
}
