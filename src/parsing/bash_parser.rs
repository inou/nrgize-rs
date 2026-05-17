use super::models::*;
use super::{ParseError, Parser};
use regex::Regex;
use std::path::Path;

pub struct BashParser;

impl Parser for BashParser {
    fn parse(&self, path: &Path, content: &str) -> Result<ParseResult, ParseError> {
        let mut result = ParseResult::new();

        parse_servers(content, &mut result, path)?;
        parse_variables(content, &mut result);
        parse_helper_functions(content, &mut result);
        parse_tasks(content, &mut result, path)?;
        parse_macros(content, &mut result, path)?;
        parse_hooks(content, &mut result, path)?;

        Ok(result)
    }
}

/// Parse server declarations: `# @servers local=127.0.0.1 staging=user@host ...`
fn parse_servers(
    content: &str,
    result: &mut ParseResult,
    _path: &Path,
) -> Result<(), ParseError> {
    let re = Regex::new(r"(?m)^#\s*@servers\s+(.+)$").unwrap();

    for cap in re.captures_iter(content) {
        let servers_str = cap.get(1).unwrap().as_str().trim();
        // Parse key=value pairs, handling comma-separated hosts
        let pair_re = Regex::new(r"(\w[\w-]*)=([\S]+)").unwrap();

        for pair in pair_re.captures_iter(servers_str) {
            let name = pair.get(1).unwrap().as_str().to_string();
            let value = pair.get(2).unwrap().as_str();

            // Support comma-separated hosts: web=host1,host2
            let hosts: Vec<String> = value.split(',').map(|s| s.to_string()).collect();

            result.servers.insert(
                name.clone(),
                ServerDefinition::new(name, hosts),
            );
        }
    }

    Ok(())
}

/// Parse top-level UPPERCASE=value variable assignments (before first function).
fn parse_variables(content: &str, result: &mut ParseResult) {
    let re = Regex::new(r#"(?m)^([A-Z_][A-Z0-9_]*)=("(?:[^"\\]|\\.)*"|'[^']*'|\S+)\s*$"#).unwrap();

    let first_func = content.find("() {").or_else(|| content.find("(){\n"));

    let search_region = match first_func {
        Some(pos) => &content[..pos],
        None => content,
    };

    let mut preamble = String::new();
    for cap in re.captures_iter(search_region) {
        let line = cap.get(0).unwrap().as_str();
        // Skip lines that are inside comments
        if !line.trim_start().starts_with('#') {
            if !preamble.is_empty() {
                preamble.push('\n');
            }
            preamble.push_str(line.trim());
        }
    }

    result.variable_preamble = preamble;
}

/// Parse non-annotated functions as helper functions.
fn parse_helper_functions(content: &str, result: &mut ParseResult) {
    let func_re = Regex::new(r"(?m)^(\w+)\(\)\s*\{").unwrap();
    let lines: Vec<&str> = content.lines().collect();

    let mut helpers = String::new();

    for cap in func_re.captures_iter(content) {
        let func_match = cap.get(0).unwrap();
        let func_name = cap.get(1).unwrap().as_str();

        // Find which line this function starts on
        let start_offset = func_match.start();
        let line_num = content[..start_offset].lines().count();

        // Check if the previous non-empty line is an annotation
        let prev_line = find_previous_non_empty_line(&lines, line_num);
        if let Some(prev) = prev_line {
            let trimmed = prev.trim();
            if trimmed.starts_with("# @task")
                || trimmed.starts_with("# @before")
                || trimmed.starts_with("# @after")
                || trimmed.starts_with("# @error")
                || trimmed.starts_with("# @success")
                || trimmed.starts_with("# @finished")
            {
                continue; // This is an annotated function, skip
            }
        }

        // Extract the function body — find the opening brace
        let brace_pos = content[start_offset..].find('{').map(|p| start_offset + p);
        if let Some(brace_pos) = brace_pos {
            if let Some(body) = extract_function_body(content, brace_pos) {
                if !helpers.is_empty() {
                    helpers.push('\n');
                }
                helpers.push_str(&format!("{}() {{\n{}\n}}", func_name, body));
            }
        }
    }

    if !helpers.is_empty() {
        if !result.variable_preamble.is_empty() {
            result.variable_preamble.push('\n');
        }
        result.variable_preamble.push_str(&helpers);
    }
}

/// Parse task definitions: `# @task on:production parallel confirm="..." emoji:rocket`
fn parse_tasks(
    content: &str,
    result: &mut ParseResult,
    _path: &Path,
) -> Result<(), ParseError> {
    let annotation_re = Regex::new(r"(?m)^#\s*@task\s+(.*)$").unwrap();

    for cap in annotation_re.captures_iter(content) {
        let annotation_match = cap.get(0).unwrap();
        let opts_str = cap.get(1).unwrap().as_str();

        // Find the next function definition after this annotation
        let remaining = &content[annotation_match.end()..];
        let func_re = Regex::new(r"(?m)^\s*(\w+)\(\)\s*\{").unwrap();

        if let Some(func_cap) = func_re.captures(remaining) {
            let func_name = func_cap.get(1).unwrap().as_str().to_string();
            let func_match = func_cap.get(0).unwrap();
            let func_start_in_content = annotation_match.end() + func_match.start();

            // Find the opening brace position
            let brace_pos = content[func_start_in_content..]
                .find('{')
                .map(|p| func_start_in_content + p);

            if let Some(brace_pos) = brace_pos {
                if let Some(body) = extract_function_body(content, brace_pos) {
                    let body = dedent(&body);

                    // Parse annotation options
                    let servers = parse_task_option_on(opts_str);
                    let parallel = opts_str.contains("parallel");
                    let confirm = parse_task_option_confirm(opts_str);
                    let emoji = parse_task_option_emoji(opts_str);

                    let local = opts_str.contains("local");
                    let servers = if local { vec![] } else { servers };

                    result.tasks.insert(
                        func_name.clone(),
                        TaskDefinition {
                            name: func_name,
                            script: body,
                            servers,
                            parallel,
                            confirm,
                            emoji,
                            local,
                            env_file: None,
                            upload: None,
                            docker_deploy: None,
                        },
                    );
                }
            }
        }
    }

    Ok(())
}

/// Parse macro definitions (single-line and multi-line).
fn parse_macros(
    content: &str,
    result: &mut ParseResult,
    _path: &Path,
) -> Result<(), ParseError> {
    // Multi-line macros: # @macro name\n#   task1\n#   task2\n# @endmacro
    let multiline_re =
        Regex::new(r"(?m)^#\s*@macro\s+(\S+)\s*\n((?:#\s+\S+\s*\n)*)#\s*@endmacro").unwrap();

    for cap in multiline_re.captures_iter(content) {
        let name = cap.get(1).unwrap().as_str().to_string();
        let body = cap.get(2).unwrap().as_str();

        let tasks: Vec<String> = body
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim().trim_start_matches('#').trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect();

        result.macros.insert(
            name.clone(),
            MacroDefinition {
                name,
                tasks,
            },
        );
    }

    // Single-line macros: # @macro deploy pull install migrate cache
    // (but not ones that are followed by @endmacro — those are multi-line)
    let singleline_re = Regex::new(r"(?m)^#\s*@macro\s+(\S+)\s+(.+)$").unwrap();

    for cap in singleline_re.captures_iter(content) {
        let name = cap.get(1).unwrap().as_str().to_string();

        // Skip if this is actually a multi-line macro (already parsed)
        if result.macros.contains_key(&name) {
            continue;
        }

        let tasks_str = cap.get(2).unwrap().as_str();
        let tasks: Vec<String> = tasks_str.split_whitespace().map(|s| s.to_string()).collect();

        result.macros.insert(
            name.clone(),
            MacroDefinition { name, tasks },
        );
    }

    Ok(())
}

/// Parse lifecycle hooks.
fn parse_hooks(
    content: &str,
    result: &mut ParseResult,
    _path: &Path,
) -> Result<(), ParseError> {
    for hook_type in HookType::all() {
        let keyword = hook_type.keyword();
        let pattern = format!(r"(?m)^#\s*@{}\s*$", regex::escape(keyword));
        let re = Regex::new(&pattern).unwrap();

        for mat in re.find_iter(content) {
            let remaining = &content[mat.end()..];

            // Find the next function definition
            let func_re = Regex::new(r"(?m)^\s*(\w+)\(\)\s*\{").unwrap();

            if let Some(func_cap) = func_re.captures(remaining) {
                let func_match = func_cap.get(0).unwrap();
                let func_start_in_content = mat.end() + func_match.start();

                let brace_pos = content[func_start_in_content..]
                    .find('{')
                    .map(|p| func_start_in_content + p);

                if let Some(brace_pos) = brace_pos {
                    if let Some(body) = extract_function_body(content, brace_pos) {
                        let body = dedent(&body);
                        result.hooks.push(HookDefinition {
                            hook_type: *hook_type,
                            script: body,
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

// --- Helpers ---

/// Extract a function body starting from the opening brace position.
/// Uses brace-balanced extraction with quote-state tracking.
fn extract_function_body(content: &str, open_brace_pos: usize) -> Option<String> {
    let bytes = content.as_bytes();
    if bytes.get(open_brace_pos) != Some(&b'{') {
        return None;
    }

    let mut depth = 0;
    let mut i = open_brace_pos;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    while i < bytes.len() {
        let ch = bytes[i] as char;

        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }

        if ch == '\\' && in_double_quote {
            escape_next = true;
            i += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if !in_single_quote && !in_double_quote {
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    // Return content between braces
                    let body = &content[open_brace_pos + 1..i];
                    return Some(body.trim_matches('\n').to_string());
                }
            }
        }

        i += 1;
    }

    None
}

/// Remove common leading whitespace from all lines.
fn dedent(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();

    // Find minimum indentation of non-empty lines
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .map(|line| {
            if line.len() >= min_indent {
                &line[min_indent..]
            } else {
                line.trim()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the `on:` option from task annotations.
fn parse_task_option_on(opts: &str) -> Vec<String> {
    let re = Regex::new(r"on:(\S+)").unwrap();
    if let Some(cap) = re.captures(opts) {
        cap.get(1)
            .unwrap()
            .as_str()
            .split(',')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![]
    }
}

/// Parse the `confirm="..."` option from task annotations.
fn parse_task_option_confirm(opts: &str) -> Option<String> {
    let re = Regex::new(r#"confirm="([^"]*)""#).unwrap();
    re.captures(opts)
        .map(|cap| cap.get(1).unwrap().as_str().to_string())
}

/// Parse the `emoji:` option from task annotations.
fn parse_task_option_emoji(opts: &str) -> Option<String> {
    let re = Regex::new(r"emoji:(\S+)").unwrap();
    re.captures(opts)
        .map(|cap| cap.get(1).unwrap().as_str().to_string())
}

/// Find the previous non-empty line before `line_num` (0-indexed).
fn find_previous_non_empty_line<'a>(lines: &[&'a str], line_num: usize) -> Option<&'a str> {
    if line_num == 0 {
        return None;
    }
    for i in (0..line_num).rev() {
        if !lines[i].trim().is_empty() {
            return Some(lines[i]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> ParseResult {
        let parser = BashParser;
        parser
            .parse(Path::new("test.sh"), content)
            .expect("Parse failed")
    }

    // --- Server parsing tests ---

    #[test]
    fn parse_single_server() {
        let result = parse("# @servers local=127.0.0.1\n");
        assert_eq!(result.servers.len(), 1);
        let server = result.servers.get("local").unwrap();
        assert_eq!(server.hosts, vec!["127.0.0.1"]);
        assert!(server.is_local());
    }

    #[test]
    fn parse_multiple_servers() {
        let result = parse(
            "# @servers local=127.0.0.1 staging=user@staging.example.com production=deploy@prod.example.com\n",
        );
        assert_eq!(result.servers.len(), 3);
        assert!(result.servers.contains_key("local"));
        assert!(result.servers.contains_key("staging"));
        assert!(result.servers.contains_key("production"));

        assert_eq!(
            result.servers.get("staging").unwrap().hosts,
            vec!["user@staging.example.com"]
        );
    }

    #[test]
    fn parse_server_with_multiple_hosts() {
        let result = parse("# @servers web=web1.example.com,web2.example.com,web3.example.com\n");
        assert_eq!(result.servers.len(), 1);
        let server = result.servers.get("web").unwrap();
        assert_eq!(server.hosts.len(), 3);
        assert_eq!(server.hosts[0], "web1.example.com");
        assert_eq!(server.hosts[1], "web2.example.com");
        assert_eq!(server.hosts[2], "web3.example.com");
    }

    // --- Task parsing tests ---

    #[test]
    fn parse_simple_task() {
        let content = r#"# @servers production=deploy@prod.example.com

# @task on:production
deploy() {
    cd /var/www/app
    git pull origin main
}
"#;
        let result = parse(content);
        assert_eq!(result.tasks.len(), 1);
        let task = result.tasks.get("deploy").unwrap();
        assert_eq!(task.name, "deploy");
        assert_eq!(task.servers, vec!["production"]);
        assert!(!task.parallel);
        assert!(task.confirm.is_none());
        assert!(task.emoji.is_none());
        assert!(task.script.contains("cd /var/www/app"));
        assert!(task.script.contains("git pull origin main"));
    }

    #[test]
    fn parse_task_with_all_options() {
        let content = r#"# @task on:production parallel confirm="Deploy to production?" emoji:rocket
deploy() {
    echo "deploying"
}
"#;
        let result = parse(content);
        let task = result.tasks.get("deploy").unwrap();
        assert_eq!(task.servers, vec!["production"]);
        assert!(task.parallel);
        assert_eq!(task.confirm, Some("Deploy to production?".to_string()));
        assert_eq!(task.emoji, Some("rocket".to_string()));
    }

    #[test]
    fn parse_task_on_multiple_servers() {
        let content = r#"# @task on:staging,production
deploy() {
    echo "deploying"
}
"#;
        let result = parse(content);
        let task = result.tasks.get("deploy").unwrap();
        assert_eq!(task.servers, vec!["staging", "production"]);
    }

    #[test]
    fn parse_multiple_tasks() {
        let content = r#"# @task on:production
pull() {
    cd /var/www && git pull
}

# @task on:production
install() {
    cd /var/www && composer install
}
"#;
        let result = parse(content);
        assert_eq!(result.tasks.len(), 2);
        assert!(result.tasks.contains_key("pull"));
        assert!(result.tasks.contains_key("install"));
    }

    #[test]
    fn parse_task_with_nested_braces() {
        let content = r#"# @task on:production
deploy() {
    if [ -d /var/www ]; then
        cd /var/www
        for f in *.txt; do
            echo "$f"
        done
    fi
}
"#;
        let result = parse(content);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains("if [ -d /var/www ]; then"));
        assert!(task.script.contains("done"));
        assert!(task.script.contains("fi"));
    }

    #[test]
    fn parse_task_with_quotes_containing_braces() {
        let content = r#"# @task on:production
deploy() {
    echo "this has { braces } inside"
    echo 'and { single } too'
}
"#;
        let result = parse(content);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains(r#"echo "this has { braces } inside""#));
    }

    // --- Macro parsing tests ---

    #[test]
    fn parse_single_line_macro() {
        let content = "# @macro deploy pull install migrate cache\n";
        let result = parse(content);
        assert_eq!(result.macros.len(), 1);
        let m = result.macros.get("deploy").unwrap();
        assert_eq!(m.tasks, vec!["pull", "install", "migrate", "cache"]);
    }

    #[test]
    fn parse_multi_line_macro() {
        let content = r#"# @macro full-deploy
#   pull
#   install
#   migrate
#   cache
# @endmacro
"#;
        let result = parse(content);
        assert_eq!(result.macros.len(), 1);
        let m = result.macros.get("full-deploy").unwrap();
        assert_eq!(m.tasks, vec!["pull", "install", "migrate", "cache"]);
    }

    // --- Hook parsing tests ---

    #[test]
    fn parse_before_hook() {
        let content = r#"# @before
notify_start() {
    echo "Deployment starting..."
}
"#;
        let result = parse(content);
        let hooks = result.get_hooks(HookType::Before);
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].script.contains("echo \"Deployment starting...\""));
    }

    #[test]
    fn parse_all_hook_types() {
        let content = r#"# @before
before_fn() {
    echo "before"
}

# @after
after_fn() {
    echo "after"
}

# @error
error_fn() {
    echo "error"
}

# @success
success_fn() {
    echo "success"
}

# @finished
finished_fn() {
    echo "finished"
}
"#;
        let result = parse(content);
        assert_eq!(result.get_hooks(HookType::Before).len(), 1);
        assert_eq!(result.get_hooks(HookType::After).len(), 1);
        assert_eq!(result.get_hooks(HookType::Error).len(), 1);
        assert_eq!(result.get_hooks(HookType::Success).len(), 1);
        assert_eq!(result.get_hooks(HookType::Finished).len(), 1);
    }

    #[test]
    fn parse_multiple_hooks_same_type() {
        let content = r#"# @before
first() {
    echo "first"
}

# @before
second() {
    echo "second"
}
"#;
        let result = parse(content);
        let hooks = result.get_hooks(HookType::Before);
        assert_eq!(hooks.len(), 2);
        assert!(hooks[0].script.contains("first"));
        assert!(hooks[1].script.contains("second"));
    }

    // --- Variable parsing tests ---

    #[test]
    fn parse_variables() {
        let content = r#"APP_ENV="production"
BRANCH=main

# @task on:production
deploy() {
    echo "deploying"
}
"#;
        let result = parse(content);
        assert!(result.variable_preamble.contains(r#"APP_ENV="production""#));
        assert!(result.variable_preamble.contains("BRANCH=main"));
    }

    #[test]
    fn variables_only_before_first_function() {
        let content = r#"APP_ENV="production"

# @task on:production
deploy() {
    LOCAL_VAR="should not be in preamble"
    echo "deploying"
}

NOT_A_VAR="after function"
"#;
        let result = parse(content);
        assert!(result.variable_preamble.contains(r#"APP_ENV="production""#));
        // NOT_A_VAR appears after the function def line, so it should still be excluded
        // because the function definition starts before it
    }

    // --- Helper function tests ---

    #[test]
    fn parse_helper_function() {
        let content = r#"helper() {
    echo "I am a helper"
}

# @task on:production
deploy() {
    helper
    echo "deploying"
}
"#;
        let result = parse(content);
        assert!(result.variable_preamble.contains("helper()"));
        assert!(result.variable_preamble.contains("echo \"I am a helper\""));
        // deploy should NOT be in the preamble (it's annotated)
        assert!(!result.variable_preamble.contains("deploy()"));
    }

    // --- Dedent tests ---

    #[test]
    fn dedent_removes_common_indent() {
        let input = "    line1\n    line2\n    line3";
        assert_eq!(dedent(input), "line1\nline2\nline3");
    }

    #[test]
    fn dedent_handles_mixed_indent() {
        let input = "    line1\n        line2\n    line3";
        assert_eq!(dedent(input), "line1\n    line2\nline3");
    }

    #[test]
    fn dedent_handles_empty_lines() {
        let input = "    line1\n\n    line3";
        assert_eq!(dedent(input), "line1\n\nline3");
    }

    // --- Integration: full file parse ---

    #[test]
    fn parse_full_energize_file() {
        let content = r#"# @servers local=127.0.0.1 staging=user@staging.example.com production=deploy@prod.example.com

APP_ENV="production"
BRANCH=main

# @before
notify_start() {
    echo "Deployment starting..."
}

# @task on:production emoji:rocket confirm="Deploy to production?"
deploy() {
    cd /var/www/app
    git pull origin main
    composer install --no-dev
}

# @task on:production
migrate() {
    cd /var/www/app
    php artisan migrate --force
}

# @macro full-deploy
#   deploy
#   migrate
# @endmacro

# @after
notify_end() {
    echo "Deployment finished."
}

# @error
notify_error() {
    echo "Deployment FAILED."
}

# @success
celebrate() {
    echo "All tasks succeeded!"
}

# @finished
cleanup() {
    echo "Runs regardless of outcome."
}
"#;
        let result = parse(content);

        // Servers
        assert_eq!(result.servers.len(), 3);
        assert!(result.servers.get("local").unwrap().is_local());
        assert!(!result.servers.get("production").unwrap().is_local());

        // Variables
        assert!(result.variable_preamble.contains(r#"APP_ENV="production""#));
        assert!(result.variable_preamble.contains("BRANCH=main"));

        // Tasks
        assert_eq!(result.tasks.len(), 2);
        let deploy = result.tasks.get("deploy").unwrap();
        assert_eq!(deploy.servers, vec!["production"]);
        assert_eq!(deploy.emoji, Some("rocket".to_string()));
        assert_eq!(deploy.confirm, Some("Deploy to production?".to_string()));

        let migrate = result.tasks.get("migrate").unwrap();
        assert_eq!(migrate.servers, vec!["production"]);

        // Macros
        assert_eq!(result.macros.len(), 1);
        let m = result.macros.get("full-deploy").unwrap();
        assert_eq!(m.tasks, vec!["deploy", "migrate"]);

        // Hooks
        assert_eq!(result.get_hooks(HookType::Before).len(), 1);
        assert_eq!(result.get_hooks(HookType::After).len(), 1);
        assert_eq!(result.get_hooks(HookType::Error).len(), 1);
        assert_eq!(result.get_hooks(HookType::Success).len(), 1);
        assert_eq!(result.get_hooks(HookType::Finished).len(), 1);

        // Macro resolution
        assert_eq!(
            result.resolve_tasks_for_target("full-deploy"),
            Some(vec!["deploy".to_string(), "migrate".to_string()])
        );
    }
}
