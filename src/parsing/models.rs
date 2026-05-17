use indexmap::IndexMap;
use std::time::Duration;

/// A server definition with one or more hosts.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerDefinition {
    pub name: String,
    pub hosts: Vec<String>,
}

impl ServerDefinition {
    pub fn new(name: impl Into<String>, hosts: Vec<String>) -> Self {
        Self {
            name: name.into(),
            hosts,
        }
    }

    /// Returns true if all hosts are local (127.0.0.1, localhost, local).
    pub fn is_local(&self) -> bool {
        self.hosts.iter().all(|h| {
            let host_part = h.split('@').last().unwrap_or(h);
            matches!(host_part, "127.0.0.1" | "localhost" | "local")
        })
    }
}

/// Specifies a file upload operation (local → remote).
#[derive(Debug, Clone, PartialEq)]
pub struct UploadSpec {
    pub src: String,
    pub dest: String,
}

/// Specifies a Docker image build+transfer operation.
#[derive(Debug, Clone, PartialEq)]
pub struct DockerDeploySpec {
    pub image: String,
    pub build_file: Option<String>,
    pub build_context: Option<String>,
    pub build_args: std::collections::HashMap<String, String>,
}

/// A task definition parsed from a task file.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskDefinition {
    pub name: String,
    pub script: String,
    pub servers: Vec<String>,
    pub parallel: bool,
    pub confirm: Option<String>,
    pub emoji: Option<String>,
    pub local: bool,
    pub env_file: Option<String>,
    pub upload: Option<UploadSpec>,
    pub docker_deploy: Option<DockerDeploySpec>,
}

impl TaskDefinition {
    #[allow(dead_code)]
    pub fn display_name(&self) -> &str {
        &self.name
    }

    pub fn display_name_with_emoji(&self) -> String {
        match &self.emoji {
            Some(emoji) => format!("{} {}", emoji, self.name),
            None => self.name.clone(),
        }
    }
}

/// A macro definition (ordered list of task names).
#[derive(Debug, Clone, PartialEq)]
pub struct MacroDefinition {
    pub name: String,
    pub tasks: Vec<String>,
}

/// Lifecycle hook types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookType {
    Before,
    After,
    Success,
    Error,
    Finished,
}

impl HookType {
    /// Returns all hook type variants.
    pub fn all() -> &'static [HookType] {
        &[
            HookType::Before,
            HookType::After,
            HookType::Success,
            HookType::Error,
            HookType::Finished,
        ]
    }

    /// Returns the annotation keyword used in Bash format.
    pub fn keyword(&self) -> &'static str {
        match self {
            HookType::Before => "before",
            HookType::After => "after",
            HookType::Success => "success",
            HookType::Error => "error",
            HookType::Finished => "finished",
        }
    }
}

/// A lifecycle hook definition.
#[derive(Debug, Clone, PartialEq)]
pub struct HookDefinition {
    pub hook_type: HookType,
    pub script: String,
}

/// The result of parsing a task file.
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub servers: IndexMap<String, ServerDefinition>,
    pub tasks: IndexMap<String, TaskDefinition>,
    pub macros: IndexMap<String, MacroDefinition>,
    pub hooks: Vec<HookDefinition>,
    pub variable_preamble: String,
    #[allow(dead_code)]
    pub env_files: Vec<String>,
}

impl ParseResult {
    pub fn new() -> Self {
        Self {
            servers: IndexMap::new(),
            tasks: IndexMap::new(),
            macros: IndexMap::new(),
            hooks: Vec::new(),
            variable_preamble: String::new(),
            env_files: Vec::new(),
        }
    }

    /// Resolve a target name to a list of task names.
    /// If the target is a macro, expand it. If it's a task, return it as a single-element list.
    /// Returns None if the target doesn't exist.
    pub fn resolve_tasks_for_target(&self, target: &str) -> Option<Vec<String>> {
        if let Some(macro_def) = self.macros.get(target) {
            Some(macro_def.tasks.clone())
        } else if self.tasks.contains_key(target) {
            Some(vec![target.to_string()])
        } else {
            None
        }
    }

    /// Get all hooks of a given type, in definition order.
    pub fn get_hooks(&self, hook_type: HookType) -> Vec<&HookDefinition> {
        self.hooks
            .iter()
            .filter(|h| h.hook_type == hook_type)
            .collect()
    }

    /// List all available target names (tasks + macros).
    pub fn available_targets(&self) -> Vec<String> {
        let mut targets: Vec<String> = self.macros.keys().cloned().collect();
        targets.extend(self.tasks.keys().cloned());
        targets
    }
}

impl Default for ParseResult {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of executing a single task.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub exit_code: i32,
    #[allow(dead_code)]
    pub outputs: IndexMap<String, String>,
    pub duration: Duration,
    pub failed_host: Option<String>,
}

impl TaskResult {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_definition_is_local() {
        let local = ServerDefinition::new("local", vec!["127.0.0.1".into()]);
        assert!(local.is_local());

        let localhost = ServerDefinition::new("local", vec!["localhost".into()]);
        assert!(localhost.is_local());

        let local_keyword = ServerDefinition::new("local", vec!["local".into()]);
        assert!(local_keyword.is_local());

        let remote = ServerDefinition::new("prod", vec!["user@prod.example.com".into()]);
        assert!(!remote.is_local());
    }

    #[test]
    fn server_definition_is_local_with_user() {
        let local_with_user = ServerDefinition::new("local", vec!["user@127.0.0.1".into()]);
        assert!(local_with_user.is_local());
    }

    #[test]
    fn server_definition_mixed_hosts_not_local() {
        let mixed = ServerDefinition::new(
            "mixed",
            vec!["127.0.0.1".into(), "remote.example.com".into()],
        );
        assert!(!mixed.is_local());
    }

    #[test]
    fn task_display_name() {
        let task = TaskDefinition {
            name: "deploy".into(),
            script: "echo hi".into(),
            servers: vec!["prod".into()],
            parallel: false,
            confirm: None,
            emoji: None,
            local: false,
            env_file: None,
            upload: None,
            docker_deploy: None,
        };
        assert_eq!(task.display_name(), "deploy");
        assert_eq!(task.display_name_with_emoji(), "deploy");
    }

    #[test]
    fn task_display_name_with_emoji() {
        let task = TaskDefinition {
            name: "deploy".into(),
            script: "echo hi".into(),
            servers: vec!["prod".into()],
            parallel: false,
            confirm: None,
            emoji: Some("🚀".into()),
            local: false,
            env_file: None,
            upload: None,
            docker_deploy: None,
        };
        assert_eq!(task.display_name_with_emoji(), "🚀 deploy");
    }

    #[test]
    fn parse_result_resolve_task() {
        let mut result = ParseResult::new();
        result.tasks.insert(
            "deploy".into(),
            TaskDefinition {
                name: "deploy".into(),
                script: "echo deploy".into(),
                servers: vec!["prod".into()],
                parallel: false,
                confirm: None,
                emoji: None,
                local: false,
                env_file: None,
                upload: None,
                docker_deploy: None,
            },
        );

        assert_eq!(
            result.resolve_tasks_for_target("deploy"),
            Some(vec!["deploy".to_string()])
        );
        assert_eq!(result.resolve_tasks_for_target("nonexistent"), None);
    }

    #[test]
    fn parse_result_resolve_macro() {
        let mut result = ParseResult::new();
        result.macros.insert(
            "full-deploy".into(),
            MacroDefinition {
                name: "full-deploy".into(),
                tasks: vec!["pull".into(), "install".into(), "migrate".into()],
            },
        );

        assert_eq!(
            result.resolve_tasks_for_target("full-deploy"),
            Some(vec![
                "pull".to_string(),
                "install".to_string(),
                "migrate".to_string()
            ])
        );
    }

    #[test]
    fn parse_result_get_hooks() {
        let mut result = ParseResult::new();
        result.hooks.push(HookDefinition {
            hook_type: HookType::Before,
            script: "echo before1".into(),
        });
        result.hooks.push(HookDefinition {
            hook_type: HookType::After,
            script: "echo after".into(),
        });
        result.hooks.push(HookDefinition {
            hook_type: HookType::Before,
            script: "echo before2".into(),
        });

        let before_hooks = result.get_hooks(HookType::Before);
        assert_eq!(before_hooks.len(), 2);
        assert_eq!(before_hooks[0].script, "echo before1");
        assert_eq!(before_hooks[1].script, "echo before2");

        let after_hooks = result.get_hooks(HookType::After);
        assert_eq!(after_hooks.len(), 1);

        let success_hooks = result.get_hooks(HookType::Success);
        assert_eq!(success_hooks.len(), 0);
    }

    #[test]
    fn parse_result_available_targets() {
        let mut result = ParseResult::new();
        result.tasks.insert(
            "deploy".into(),
            TaskDefinition {
                name: "deploy".into(),
                script: "echo deploy".into(),
                servers: vec![],
                parallel: false,
                confirm: None,
                emoji: None,
                local: false,
                env_file: None,
                upload: None,
                docker_deploy: None,
            },
        );
        result.macros.insert(
            "full".into(),
            MacroDefinition {
                name: "full".into(),
                tasks: vec!["deploy".into()],
            },
        );

        let targets = result.available_targets();
        assert!(targets.contains(&"deploy".to_string()));
        assert!(targets.contains(&"full".to_string()));
    }

    #[test]
    fn task_result_succeeded() {
        let success = TaskResult {
            exit_code: 0,
            outputs: IndexMap::new(),
            duration: Duration::from_secs(1),
            failed_host: None,
        };
        assert!(success.succeeded());

        let failure = TaskResult {
            exit_code: 1,
            outputs: IndexMap::new(),
            duration: Duration::from_secs(1),
            failed_host: Some("prod.example.com".into()),
        };
        assert!(!failure.succeeded());
    }
}
