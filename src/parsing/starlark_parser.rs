use super::models::*;
use super::ParseError;
use indexmap::IndexMap;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::none::NoneType;
use starlark::values::Value;
use starlark::any::ProvidesStaticType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

/// Accumulated state from evaluating a Starlark file.
/// The registered DSL functions write into this during evaluation.
#[derive(Debug, Default, ProvidesStaticType)]
pub struct StarlarkState {
    pub servers: RefCell<IndexMap<String, ServerDefinition>>,
    pub tasks: RefCell<IndexMap<String, TaskDefinition>>,
    pub macros: RefCell<IndexMap<String, MacroDefinition>>,
    pub hooks: RefCell<Vec<HookDefinition>>,
    pub env_files: RefCell<Vec<String>>,
}

/// Register our DSL functions into a GlobalsBuilder.
#[starlark_module]
fn nrg_dsl(builder: &mut GlobalsBuilder) {
    /// Define servers. Each keyword argument is a server name.
    /// Value can be a string (single host) or list of strings (multi-host).
    fn servers<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let dict = starlark::values::dict::DictRef::from_value(kwargs)
            .ok_or_else(|| anyhow::anyhow!("servers() expects keyword arguments"))?;

        let mut servers = state.servers.borrow_mut();

        for (key, value) in dict.iter() {
            let name = key.to_str();

            let hosts: Vec<String> = if let Some(list) = starlark::values::list::ListRef::from_value(value) {
                list.iter().map(|v| v.to_str().to_string()).collect()
            } else {
                vec![value.to_str().to_string()]
            };

            servers.insert(
                name.to_string(),
                ServerDefinition::new(name.to_string(), hosts),
            );
        }

        Ok(NoneType)
    }

    /// Define a task.
    fn task<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = NoneType)] on: Value<'v>,
        #[starlark(require = named, default = "")] script: &str,
        #[starlark(require = named, default = false)] parallel: bool,
        #[starlark(require = named, default = NoneType)] confirm: Value<'v>,
        #[starlark(require = named, default = NoneType)] emoji: Value<'v>,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = NoneType)] env: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let server_list: Vec<String> = if on.is_none() {
            vec![]
        } else if let Some(list) = starlark::values::list::ListRef::from_value(on) {
            list.iter().map(|v| v.to_str().to_string()).collect()
        } else {
            vec![on.to_str().to_string()]
        };

        if !local && server_list.is_empty() {
            return Err(anyhow::anyhow!(
                "Task '{}' must specify 'on' (server list) or 'local = True'",
                name
            ));
        }

        let confirm_opt = if confirm.is_none() {
            None
        } else {
            Some(confirm.to_str().to_string())
        };

        let emoji_opt = if emoji.is_none() {
            None
        } else {
            Some(emoji.to_str().to_string())
        };

        let env_file_opt = if env.is_none() {
            None
        } else {
            Some(env.to_str().to_string())
        };

        let mut tasks = state.tasks.borrow_mut();
        tasks.insert(
            name.to_string(),
            TaskDefinition {
                name: name.to_string(),
                script: dedent_script(script),
                servers: server_list,
                parallel,
                confirm: confirm_opt,
                emoji: emoji_opt,
                local,
                env_file: env_file_opt,
                upload: None,
                docker_deploy: None,
            },
        );

        Ok(NoneType)
    }

    /// Define a macro (ordered list of task names).
    /// Named `define_macro` in Starlark because `macro` is a reserved word in Rust.
    /// Users write `define_macro(...)` in their .star files.
    fn define_macro<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] tasks: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let task_list: Vec<String> = starlark::values::list::ListRef::from_value(tasks)
            .ok_or_else(|| anyhow::anyhow!("define_macro() tasks must be a list"))?
            .iter()
            .map(|v| v.to_str().to_string())
            .collect();

        let mut macros = state.macros.borrow_mut();
        macros.insert(
            name.to_string(),
            MacroDefinition {
                name: name.to_string(),
                tasks: task_list,
            },
        );

        Ok(NoneType)
    }

    /// Define a before hook.
    fn before(
        #[starlark(require = named)] script: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_hook(eval, HookType::Before, script)
    }

    /// Define an after hook.
    fn after(
        #[starlark(require = named)] script: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_hook(eval, HookType::After, script)
    }

    /// Define an error hook.
    fn error(
        #[starlark(require = named)] script: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_hook(eval, HookType::Error, script)
    }

    /// Define a success hook.
    fn success(
        #[starlark(require = named)] script: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_hook(eval, HookType::Success, script)
    }

    /// Define a finished hook.
    fn finished(
        #[starlark(require = named)] script: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_hook(eval, HookType::Finished, script)
    }

    /// Reference a CLI variable with optional default.
    fn var<'v>(
        name: &str,
        #[starlark(require = named, default = NoneType)] default: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        // Check the module for a __cli_vars__ dict
        let module_var = eval.module().get("__cli_vars__");
        if let Some(cli_vars) = module_var {
            if let Some(dict) = starlark::values::dict::DictRef::from_value(cli_vars) {
                for (k, v) in dict.iter() {
                    if k.to_str() == name {
                        return Ok(v);
                    }
                }
            }
        }

        // Fall back to default
        if default.is_none() {
            Err(anyhow::anyhow!(
                "Variable '{}' not set and no default provided",
                name
            ))
        } else {
            Ok(default)
        }
    }

    /// Load an .env file. Values become available to var() and are exported to remote shells.
    fn env_file(
        path: &str,
        #[starlark(require = named, default = false)] encrypted: bool,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let label = if encrypted {
            format!("encrypted:{}", path)
        } else {
            path.to_string()
        };
        state.env_files.borrow_mut().push(label);

        // If not encrypted, load the env file and inject values into __cli_vars__
        // (CLI --var values take precedence since they're already in the dict)
        if !encrypted {
            // We'll resolve the path relative to the .star file at a higher level
            // For now just record it; env loading happens in parse_starlark()
        }

        Ok(NoneType)
    }

    /// Upload local files to remote servers.
    fn upload<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] src: &str,
        #[starlark(require = named)] dest: &str,
        #[starlark(require = named)] on: Value<'v>,
        #[starlark(require = named, default = NoneType)] emoji: Value<'v>,
        #[starlark(require = named, default = NoneType)] confirm: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let server_list: Vec<String> = if let Some(list) = starlark::values::list::ListRef::from_value(on) {
            list.iter().map(|v| v.to_str().to_string()).collect()
        } else {
            vec![on.to_str().to_string()]
        };

        let emoji_opt = if emoji.is_none() { None } else { Some(emoji.to_str().to_string()) };
        let confirm_opt = if confirm.is_none() { None } else { Some(confirm.to_str().to_string()) };

        let mut tasks = state.tasks.borrow_mut();
        tasks.insert(
            name.to_string(),
            TaskDefinition {
                name: name.to_string(),
                script: String::new(),
                servers: server_list,
                parallel: false,
                confirm: confirm_opt,
                emoji: emoji_opt,
                local: false,
                env_file: None,
                upload: Some(UploadSpec {
                    src: src.to_string(),
                    dest: dest.to_string(),
                }),
                docker_deploy: None,
            },
        );

        Ok(NoneType)
    }

    /// Build a Docker image locally and deploy it to remote servers.
    /// Expands into subtasks: build (optional), save, transfer, load.
    fn docker_deploy<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] image: &str,
        #[starlark(require = named)] on: Value<'v>,
        #[starlark(require = named, default = NoneType)] build: Value<'v>,
        #[starlark(require = named, default = NoneType)] build_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] build_args: Value<'v>,
        #[starlark(require = named, default = NoneType)] emoji: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
            .downcast_ref::<StarlarkState>()
            .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

        let server_list: Vec<String> = if let Some(list) = starlark::values::list::ListRef::from_value(on) {
            list.iter().map(|v| v.to_str().to_string()).collect()
        } else {
            vec![on.to_str().to_string()]
        };

        let _emoji_opt = if emoji.is_none() { None } else { Some(emoji.to_str().to_string()) };

        let build_file = if build.is_none() { None } else { Some(build.to_str().to_string()) };
        let ctx = if build_context.is_none() { None } else { Some(build_context.to_str().to_string()) };

        let mut b_args = HashMap::new();
        if !build_args.is_none() {
            if let Some(dict) = starlark::values::dict::DictRef::from_value(build_args) {
                for (k, v) in dict.iter() {
                    b_args.insert(k.to_str().to_string(), v.to_str().to_string());
                }
            }
        }

        // Image tag sanitized for temp filenames
        let safe_image = image.replace([':', '/', '.'], "-");
        let tmp_path = format!("/tmp/nrg-docker-{}.tar.gz", safe_image);

        let mut subtasks = Vec::new();

        // Step 1: Build (optional — only if Dockerfile specified)
        if build_file.is_some() {
            let dockerfile = build_file.as_deref().unwrap_or("Dockerfile");
            let context = ctx.as_deref().unwrap_or(".");
            let mut build_cmd = format!("docker build -t {} -f {} {}", image, dockerfile, context);
            for (k, v) in &b_args {
                build_cmd.push_str(&format!(" --build-arg {}={}", k, v));
            }

            subtasks.push(TaskDefinition {
                name: format!("{}:build", name),
                script: build_cmd,
                servers: vec![],
                parallel: false,
                confirm: None,
                emoji: Some("🔨".to_string()),
                local: true,
                env_file: None,
                upload: None,
                docker_deploy: None,
            });
        }

        // Step 2: Save image to tarball
        subtasks.push(TaskDefinition {
            name: format!("{}:save", name),
            script: format!("docker save {} | gzip > {}", image, tmp_path),
            servers: vec![],
            parallel: false,
            confirm: None,
            emoji: Some("📦".to_string()),
            local: true,
            env_file: None,
            upload: None,
            docker_deploy: None,
        });

        // Step 3: Transfer tarball to remote
        subtasks.push(TaskDefinition {
            name: format!("{}:transfer", name),
            script: String::new(),
            servers: server_list.clone(),
            parallel: false,
            confirm: None,
            emoji: Some("📤".to_string()),
            local: false,
            env_file: None,
            upload: Some(UploadSpec {
                src: tmp_path.clone(),
                dest: tmp_path.clone(),
            }),
            docker_deploy: None,
        });

        // Step 4: Load on remote + cleanup
        subtasks.push(TaskDefinition {
            name: format!("{}:load", name),
            script: format!("docker load < {} && rm -f {}", tmp_path, tmp_path),
            servers: server_list,
            parallel: false,
            confirm: None,
            emoji: Some("🐳".to_string()),
            local: false,
            env_file: None,
            upload: None,
            docker_deploy: None,
        });

        // Insert all subtasks
        let mut tasks = state.tasks.borrow_mut();
        let task_names: Vec<String> = subtasks.iter().map(|t| t.name.clone()).collect();
        for t in subtasks {
            tasks.insert(t.name.clone(), t);
        }

        // Create a macro that chains them
        let mut macros = state.macros.borrow_mut();
        macros.insert(
            name.to_string(),
            MacroDefinition {
                name: name.to_string(),
                tasks: task_names,
            },
        );

        Ok(NoneType)
    }
}

/// Helper to add a hook via evaluator extra state.
fn add_hook(
    eval: &mut Evaluator<'_, '_, '_>,
    hook_type: HookType,
    script: &str,
) -> anyhow::Result<NoneType> {
    let state = eval
        .extra
        .ok_or_else(|| anyhow::anyhow!("Missing evaluator state"))?
        .downcast_ref::<StarlarkState>()
        .ok_or_else(|| anyhow::anyhow!("Invalid evaluator state"))?;

    state.hooks.borrow_mut().push(HookDefinition {
        hook_type,
        script: script.to_string(),
    });

    Ok(NoneType)
}

/// Remove common leading whitespace from a multi-line script string.
fn dedent_script(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();

    // Skip leading/trailing empty lines
    let first_non_empty = lines.iter().position(|l| !l.trim().is_empty());
    let last_non_empty = lines.iter().rposition(|l| !l.trim().is_empty());

    let (start, end) = match (first_non_empty, last_non_empty) {
        (Some(s), Some(e)) => (s, e),
        _ => return s.to_string(),
    };

    let relevant = &lines[start..=end];

    let min_indent = relevant
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    relevant
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

/// Parse a Starlark file with optional CLI variables.
pub fn parse_starlark(
    path: &Path,
    content: &str,
    cli_vars: &HashMap<String, String>,
) -> Result<ParseResult, ParseError> {
    let filename = path.to_string_lossy().to_string();

    // Parse the AST with a permissive dialect
    let dialect = Dialect {
        enable_top_level_stmt: true,
        enable_f_strings: true,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(&filename, content.to_owned(), &dialect)
        .map_err(|e| ParseError::Other(format!("Starlark parse error: {}", e)))?;

    // Build globals with our DSL functions
    let globals = GlobalsBuilder::standard()
        .with(nrg_dsl)
        .build();

    // Create module and inject CLI vars
    let module = Module::new();
    {
        let heap = module.heap();
        // Inject CLI vars as a dict
        let dict = heap.alloc(starlark::values::dict::AllocDict(
            cli_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
        ));
        module.set("__cli_vars__", dict);
    }

    // Create state to accumulate results
    let state = StarlarkState::default();

    // Evaluate
    {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&state);

        eval.eval_module(ast, &globals)
            .map_err(|e| ParseError::Other(format!("Starlark evaluation error: {}", e)))?;
    }

    // Build ParseResult from accumulated state
    let servers = state.servers.into_inner();
    let tasks = state.tasks.into_inner();
    let macros = state.macros.into_inner();
    let hooks = state.hooks.into_inner();
    let env_files = state.env_files.into_inner();

    // Validation: servers required only if there are non-local tasks
    let has_remote_tasks = tasks.values().any(|t| !t.local && t.upload.is_none());
    if servers.is_empty() && has_remote_tasks {
        return Err(ParseError::Other(
            "No servers defined. Use servers() to declare at least one server.".into(),
        ));
    }

    if tasks.is_empty() && macros.is_empty() {
        return Err(ParseError::Other(
            "No tasks defined. Use task() to declare at least one task.".into(),
        ));
    }

    Ok(ParseResult {
        servers,
        tasks,
        macros,
        hooks,
        variable_preamble: String::new(), // Starlark handles variables internally
        env_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> ParseResult {
        parse_starlark(Path::new("test.star"), content, &HashMap::new())
            .expect("Parse failed")
    }

    fn parse_with_vars(content: &str, vars: HashMap<String, String>) -> ParseResult {
        parse_starlark(Path::new("test.star"), content, &vars)
            .expect("Parse failed")
    }

    // --- Server parsing ---

    #[test]
    fn parse_single_server() {
        let result = parse(r#"
servers(local = "127.0.0.1")
task(name = "test", on = ["local"], script = "echo hi")
"#);
        assert_eq!(result.servers.len(), 1);
        let server = result.servers.get("local").unwrap();
        assert_eq!(server.hosts, vec!["127.0.0.1"]);
        assert!(server.is_local());
    }

    #[test]
    fn parse_multiple_servers() {
        let result = parse(r#"
servers(
    local = "127.0.0.1",
    staging = "user@staging.example.com",
    production = "deploy@prod.example.com",
)
task(name = "test", on = ["local"], script = "echo hi")
"#);
        assert_eq!(result.servers.len(), 3);
        assert!(result.servers.contains_key("local"));
        assert!(result.servers.contains_key("staging"));
        assert!(result.servers.contains_key("production"));
    }

    #[test]
    fn parse_multi_host_server() {
        let result = parse(r#"
servers(
    web = ["web1.example.com", "web2.example.com", "web3.example.com"],
)
task(name = "test", on = ["web"], script = "echo hi")
"#);
        let server = result.servers.get("web").unwrap();
        assert_eq!(server.hosts.len(), 3);
        assert_eq!(server.hosts[0], "web1.example.com");
        assert_eq!(server.hosts[2], "web3.example.com");
    }

    // --- Task parsing ---

    #[test]
    fn parse_simple_task() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = "echo deploying",
)
"#);
        assert_eq!(result.tasks.len(), 1);
        let task = result.tasks.get("deploy").unwrap();
        assert_eq!(task.name, "deploy");
        assert_eq!(task.servers, vec!["production"]);
        assert!(!task.parallel);
        assert!(task.confirm.is_none());
        assert!(task.emoji.is_none());
        assert_eq!(task.script, "echo deploying");
    }

    #[test]
    fn parse_task_with_all_options() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = "echo deploying",
    parallel = True,
    confirm = "Deploy to production?",
    emoji = "🚀",
)
"#);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.parallel);
        assert_eq!(task.confirm, Some("Deploy to production?".to_string()));
        assert_eq!(task.emoji, Some("🚀".to_string()));
    }

    #[test]
    fn parse_task_multiline_script() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = """
        cd /var/www/app
        git pull origin main
        composer install --no-dev
    """,
)
"#);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains("cd /var/www/app"));
        assert!(task.script.contains("git pull origin main"));
        assert!(task.script.contains("composer install --no-dev"));
        // Should be dedented
        assert!(!task.script.starts_with(' '));
    }

    #[test]
    fn parse_multiple_tasks() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(name = "pull", on = ["production"], script = "git pull")
task(name = "install", on = ["production"], script = "composer install")
task(name = "migrate", on = ["production"], script = "php artisan migrate")
"#);
        assert_eq!(result.tasks.len(), 3);
        assert!(result.tasks.contains_key("pull"));
        assert!(result.tasks.contains_key("install"));
        assert!(result.tasks.contains_key("migrate"));
    }

    // --- Macro parsing ---

    #[test]
    fn parse_define_macro() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(name = "pull", on = ["production"], script = "git pull")
task(name = "install", on = ["production"], script = "composer install")
task(name = "migrate", on = ["production"], script = "php artisan migrate")

define_macro(
    name = "full-deploy",
    tasks = ["pull", "install", "migrate"],
)
"#);
        assert_eq!(result.macros.len(), 1);
        let m = result.macros.get("full-deploy").unwrap();
        assert_eq!(m.tasks, vec!["pull", "install", "migrate"]);
    }

    // --- Hook parsing ---

    #[test]
    fn parse_all_hooks() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(name = "deploy", on = ["production"], script = "echo deploy")

before(script = "echo before")
after(script = "echo after")
error(script = "echo error")
success(script = "echo success")
finished(script = "echo finished")
"#);
        assert_eq!(result.get_hooks(HookType::Before).len(), 1);
        assert_eq!(result.get_hooks(HookType::After).len(), 1);
        assert_eq!(result.get_hooks(HookType::Error).len(), 1);
        assert_eq!(result.get_hooks(HookType::Success).len(), 1);
        assert_eq!(result.get_hooks(HookType::Finished).len(), 1);

        assert_eq!(result.get_hooks(HookType::Before)[0].script, "echo before");
    }

    #[test]
    fn parse_multiple_hooks_same_type() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(name = "deploy", on = ["production"], script = "echo deploy")

before(script = "echo first")
before(script = "echo second")
"#);
        let hooks = result.get_hooks(HookType::Before);
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].script, "echo first");
        assert_eq!(hooks[1].script, "echo second");
    }

    // --- Variable / var() ---

    #[test]
    fn parse_var_with_default() {
        let result = parse(r#"
BRANCH = var("branch", default = "main")

servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = "git pull origin " + BRANCH,
)
"#);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains("git pull origin main"));
    }

    #[test]
    fn parse_var_with_cli_override() {
        let mut vars = HashMap::new();
        vars.insert("branch".to_string(), "develop".to_string());

        let result = parse_with_vars(
            r#"
BRANCH = var("branch", default = "main")

servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = "git pull origin " + BRANCH,
)
"#,
            vars,
        );
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains("git pull origin develop"));
    }

    // --- Conditionals ---

    #[test]
    fn parse_conditional_task() {
        let result = parse(r#"
APP_ENV = "production"

servers(production = "deploy@prod.example.com")
task(name = "deploy", on = ["production"], script = "echo deploy")

if APP_ENV == "production":
    task(name = "cache-warm", on = ["production"], script = "php artisan cache:warm")
"#);
        assert_eq!(result.tasks.len(), 2);
        assert!(result.tasks.contains_key("cache-warm"));
    }

    #[test]
    fn parse_conditional_task_not_triggered() {
        let result = parse(r#"
APP_ENV = "staging"

servers(staging = "user@staging.example.com")
task(name = "deploy", on = ["staging"], script = "echo deploy")

if APP_ENV == "production":
    task(name = "cache-warm", on = ["staging"], script = "php artisan cache:warm")
"#);
        assert_eq!(result.tasks.len(), 1);
        assert!(!result.tasks.contains_key("cache-warm"));
    }

    // --- Loops ---

    #[test]
    fn parse_loop_generated_tasks() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")

services = ["web", "worker", "scheduler"]
for svc in services:
    task(
        name = "restart-" + svc,
        on = ["production"],
        script = "systemctl restart " + svc,
    )
"#);
        assert_eq!(result.tasks.len(), 3);
        assert!(result.tasks.contains_key("restart-web"));
        assert!(result.tasks.contains_key("restart-worker"));
        assert!(result.tasks.contains_key("restart-scheduler"));

        let web_task = result.tasks.get("restart-web").unwrap();
        assert_eq!(web_task.script, "systemctl restart web");
    }

    // --- String formatting in scripts ---

    #[test]
    fn parse_string_format_in_script() {
        let result = parse(r#"
BRANCH = "main"

servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    script = "cd /var/www/app\ngit pull origin " + BRANCH,
)
"#);
        let task = result.tasks.get("deploy").unwrap();
        assert!(task.script.contains("git pull origin main"));
    }

    // --- Error cases ---

    #[test]
    fn error_no_servers() {
        let result = parse_starlark(
            Path::new("test.star"),
            r#"task(name = "test", on = ["local"], script = "echo hi")"#,
            &HashMap::new(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No servers defined"));
    }

    #[test]
    fn error_no_tasks() {
        let result = parse_starlark(
            Path::new("test.star"),
            r#"servers(local = "127.0.0.1")"#,
            &HashMap::new(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No tasks defined"));
    }

    #[test]
    fn error_syntax_error() {
        let result = parse_starlark(
            Path::new("test.star"),
            "this is not valid starlark }{",
            &HashMap::new(),
        );
        assert!(result.is_err());
    }

    // --- Macro resolution integration ---

    #[test]
    fn macro_resolution_works() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(name = "pull", on = ["production"], script = "git pull")
task(name = "install", on = ["production"], script = "composer install")
define_macro(name = "deploy", tasks = ["pull", "install"])
"#);
        assert_eq!(
            result.resolve_tasks_for_target("deploy"),
            Some(vec!["pull".to_string(), "install".to_string()])
        );
    }

    // --- Full integration test ---

    #[test]
    fn parse_full_starlark_file() {
        let content = r#"
servers(
    local = "127.0.0.1",
    staging = "user@staging.example.com",
    production = "deploy@prod.example.com",
)

before(script = 'echo "Deployment starting..."')

task(
    name = "deploy",
    on = ["production"],
    confirm = "Deploy to production?",
    emoji = "🚀",
    script = """
        cd /var/www/app
        git pull origin main
        composer install --no-dev
    """,
)

task(
    name = "migrate",
    on = ["production"],
    script = """
        cd /var/www/app
        php artisan migrate --force
    """,
)

define_macro(name = "full-deploy", tasks = ["deploy", "migrate"])

after(script = 'echo "Deployment finished."')
error(script = 'echo "Deployment FAILED."')
success(script = 'echo "All tasks succeeded!"')
finished(script = 'echo "Runs regardless of outcome."')
"#;
        let result = parse(content);

        // Servers
        assert_eq!(result.servers.len(), 3);
        assert!(result.servers.get("local").unwrap().is_local());
        assert!(!result.servers.get("production").unwrap().is_local());

        // Tasks
        assert_eq!(result.tasks.len(), 2);
        let deploy = result.tasks.get("deploy").unwrap();
        assert_eq!(deploy.servers, vec!["production"]);
        assert_eq!(deploy.emoji, Some("🚀".to_string()));
        assert_eq!(deploy.confirm, Some("Deploy to production?".to_string()));
        assert!(deploy.script.contains("git pull origin main"));

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

    // --- Dedent ---

    #[test]
    fn dedent_script_removes_common_indent() {
        let input = "\n        line1\n        line2\n        line3\n    ";
        let result = dedent_script(input);
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn dedent_script_handles_mixed_indent() {
        let input = "\n        line1\n            line2\n        line3\n    ";
        let result = dedent_script(input);
        assert_eq!(result, "line1\n    line2\nline3");
    }

    // --- Local task ---

    #[test]
    fn parse_local_task() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(
    name = "build",
    local = True,
    script = "npm run build",
)
task(name = "deploy", on = ["production"], script = "echo deploy")
"#);
        let build = result.tasks.get("build").unwrap();
        assert!(build.local);
        assert!(build.servers.is_empty());
        assert_eq!(build.script, "npm run build");

        let deploy = result.tasks.get("deploy").unwrap();
        assert!(!deploy.local);
    }

    #[test]
    fn parse_local_task_no_server_required() {
        // A file with only local tasks shouldn't require servers()
        let result = parse_starlark(
            Path::new("test.star"),
            r#"
servers(local = "127.0.0.1")
task(name = "build", local = True, script = "make")
"#,
            &HashMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn parse_task_without_on_or_local_fails() {
        let result = parse_starlark(
            Path::new("test.star"),
            r#"
servers(prod = "deploy@prod.example.com")
task(name = "bad", script = "echo oops")
"#,
            &HashMap::new(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must specify 'on'"));
    }

    // --- Upload ---

    #[test]
    fn parse_upload_task() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
upload(
    name = "push-config",
    src = "./nginx.conf",
    dest = "/etc/nginx/sites-available/myapp",
    on = ["production"],
    emoji = "📤",
)
task(name = "restart", on = ["production"], script = "systemctl restart nginx")
"#);
        let up = result.tasks.get("push-config").unwrap();
        assert!(up.upload.is_some());
        let spec = up.upload.as_ref().unwrap();
        assert_eq!(spec.src, "./nginx.conf");
        assert_eq!(spec.dest, "/etc/nginx/sites-available/myapp");
        assert_eq!(up.servers, vec!["production"]);
    }

    #[test]
    fn upload_in_macro() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
upload(name = "push-assets", src = "./dist/", dest = "/var/www/app/public/", on = ["production"])
task(name = "restart", on = ["production"], script = "systemctl restart app")
define_macro(name = "deploy", tasks = ["push-assets", "restart"])
"#);
        assert_eq!(
            result.resolve_tasks_for_target("deploy"),
            Some(vec!["push-assets".to_string(), "restart".to_string()])
        );
    }

    // --- Env file ---

    #[test]
    fn parse_env_file_dsl() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
env_file(".env.prod")
task(name = "deploy", on = ["production"], script = "echo deploy")
"#);
        assert_eq!(result.env_files.len(), 1);
        assert_eq!(result.env_files[0], ".env.prod");
    }

    #[test]
    fn parse_encrypted_env_file() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
env_file(".env.prod.enc", encrypted = True)
task(name = "deploy", on = ["production"], script = "echo deploy")
"#);
        assert_eq!(result.env_files.len(), 1);
        assert!(result.env_files[0].starts_with("encrypted:"));
    }

    #[test]
    fn parse_task_with_env() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
task(
    name = "deploy",
    on = ["production"],
    env = ".env.prod",
    script = "echo $DATABASE_URL",
)
"#);
        let task = result.tasks.get("deploy").unwrap();
        assert_eq!(task.env_file, Some(".env.prod".to_string()));
    }

    // --- Docker deploy ---

    #[test]
    fn parse_docker_deploy_with_build() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
docker_deploy(
    name = "ship-app",
    image = "myapp:latest",
    build = "./Dockerfile",
    build_context = ".",
    on = ["production"],
)
task(name = "status", on = ["production"], script = "docker ps")
"#);
        // Should expand into subtasks + macro
        assert!(result.macros.contains_key("ship-app"));
        let m = result.macros.get("ship-app").unwrap();
        assert_eq!(m.tasks.len(), 4); // build, save, transfer, load

        assert!(result.tasks.contains_key("ship-app:build"));
        assert!(result.tasks.contains_key("ship-app:save"));
        assert!(result.tasks.contains_key("ship-app:transfer"));
        assert!(result.tasks.contains_key("ship-app:load"));

        // Build is local
        let build = result.tasks.get("ship-app:build").unwrap();
        assert!(build.local);
        assert!(build.script.contains("docker build"));

        // Save is local
        let save = result.tasks.get("ship-app:save").unwrap();
        assert!(save.local);
        assert!(save.script.contains("docker save"));

        // Transfer is upload
        let transfer = result.tasks.get("ship-app:transfer").unwrap();
        assert!(transfer.upload.is_some());

        // Load is remote
        let load = result.tasks.get("ship-app:load").unwrap();
        assert!(!load.local);
        assert!(load.script.contains("docker load"));
    }

    #[test]
    fn parse_docker_deploy_no_build() {
        let result = parse(r#"
servers(production = "deploy@prod.example.com")
docker_deploy(
    name = "ship",
    image = "myapp:latest",
    on = ["production"],
)
task(name = "status", on = ["production"], script = "docker ps")
"#);
        let m = result.macros.get("ship").unwrap();
        assert_eq!(m.tasks.len(), 3); // save, transfer, load (no build)
        assert!(!result.tasks.contains_key("ship:build"));
        assert!(result.tasks.contains_key("ship:save"));
    }
}
