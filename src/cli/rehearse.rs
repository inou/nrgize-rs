//! Declaration-only Rhai contracts, followed by explicitly opted-in local execution.
//! No project state, deployment locks, SSH transport, or shell-semantics simulation.
use crate::engine::{
    interrupt,
    runner::{RealRunner, RunOptions},
    secret::redact,
};
use clap::Args;
use rhai::{module_resolvers::StaticModuleResolver, Engine, Module, Scope};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Instant,
};

#[derive(Args)]
pub struct RehearseArgs {
    /// Rhai file returning a contract map (not an Energize.rhai deployment script)
    #[arg(default_value = "Rehearsal.rhai")]
    pub file: PathBuf,
    /// Execute trusted commands in a disposable local workspace (not an OS sandbox)
    #[arg(long, conflicts_with = "dry_run")]
    pub execute: bool,
    /// Validate declarations and show planned, execution-unverified steps (the default)
    #[arg(long)]
    pub dry_run: bool,
    /// Include explicitly declared fault scenarios
    #[arg(long)]
    pub faults: bool,
    /// Emit a JSON report; suppress command-output streaming
    #[arg(long)]
    pub json: bool,
    /// Pass this environment variable to commands and redact its value; repeatable
    #[arg(long = "env", value_name = "NAME")]
    pub environment: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    name: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    setup: Vec<Step>,
    scenarios: Vec<Scenario>,
    cleanup: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    #[serde(default)]
    fault: bool,
    steps: Vec<Step>,
    checks: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    name: String,
    run: String,
    #[serde(default)]
    expect_exit: i64,
    #[serde(default = "deadline")]
    timeout_secs: u64,
    #[serde(default)]
    stdin: String,
}
fn deadline() -> u64 {
    60
}

#[derive(Serialize)]
struct Record {
    scenario: String,
    name: String,
    host: &'static str,
    operation: &'static str,
    fault: bool,
    expected_exit: i64,
    exit_code: Option<i64>,
    status: &'static str,
    duration_ms: Option<u128>,
    excerpt: String,
}
#[derive(Serialize)]
struct Report {
    schema_version: u8,
    name: String,
    status: &'static str,
    workspace: Option<String>,
    workspace_removed: bool,
    steps: Vec<Record>,
}

fn valid_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
fn valid_env(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with("NRG_")
        && !s.starts_with("XDG_")
        && ![
            "HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "PWD",
            "OLDPWD",
            "ENV",
            "BASH_ENV",
            "SHELLOPTS",
            "BASHOPTS",
        ]
        .contains(&s)
}

fn load(path: &Path) -> Result<Contract, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("cannot read contract: {e}"))?;
    if source.len() > 1024 * 1024 {
        return Err("contract exceeds 1 MiB".into());
    }
    // No nrg builtins and no filesystem module resolver: declaration evaluation cannot
    // execute commands, resolve credentials, make HTTP requests or write project state.
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 128);
    engine.set_max_operations(1_000_000);
    engine.set_max_string_size(1024 * 1024);
    engine.set_max_array_size(4096);
    engine.set_max_map_size(1024);
    engine.on_print(|_| {});
    engine.on_debug(|_, _, _| {});
    let mut resolver = StaticModuleResolver::new();
    for (name, source) in [
        ("std/contracts", include_str!("../../lib/contracts.rhai")),
        (
            "std/release_recipes",
            include_str!("../../lib/release_recipes.rhai"),
        ),
    ] {
        let ast = engine.compile(source).map_err(|e| e.to_string())?;
        let module =
            Module::eval_ast_as_new(Scope::new(), &ast, &engine).map_err(|e| e.to_string())?;
        resolver.insert(name, module);
    }
    engine.set_module_resolver(resolver);
    let value = engine.eval::<rhai::Dynamic>(&source).map_err(|e| {
        // Exception text may contain command bodies; don't echo declaration contents.
        format!("contract evaluation failed at {}: use a map and declaration helpers; deployment builtins and file imports are unavailable", e.position())
    })?;
    // Deserialization errors can quote invalid values, including env/stdin that has
    // not yet been registered for redaction. Keep declaration contents off this path.
    rhai::serde::from_dynamic(&value).map_err(|_| "invalid contract schema: expected named scenarios with steps/checks and explicit cleanup; unknown fields and incorrect value types are rejected".into())
}

fn validate(contract: &Contract) -> Result<(), String> {
    if !valid_name(&contract.name)
        || contract.scenarios.is_empty()
        || contract.scenarios.len() > 64
        || contract.cleanup.is_empty()
    {
        return Err(
            "contract needs a printable name, 1..64 scenarios, and explicit cleanup steps".into(),
        );
    }
    for (key, value) in &contract.env {
        if !valid_env(key) || value.contains('\0') {
            return Err("invalid or reserved contract environment variable".into());
        }
    }
    let mut names = HashSet::from(["setup".to_string(), "cleanup".to_string()]);
    for scenario in &contract.scenarios {
        if !valid_name(&scenario.name)
            || !names.insert(scenario.name.clone())
            || scenario.steps.is_empty()
            || scenario.checks.is_empty()
        {
            return Err(
                "each scenario needs a unique printable name, actions, and postcondition checks"
                    .into(),
            );
        }
        if scenario.checks.iter().any(|s| s.expect_exit != 0) {
            return Err("postcondition checks must expect exit 0".into());
        }
    }
    let steps: Vec<_> = contract
        .setup
        .iter()
        .chain(
            contract
                .scenarios
                .iter()
                .flat_map(|s| s.steps.iter().chain(&s.checks)),
        )
        .chain(&contract.cleanup)
        .collect();
    if steps.len() > 256 {
        return Err("contract exceeds 256 steps".into());
    }
    for step in steps {
        if !valid_name(&step.name)
            || step.run.trim().is_empty()
            || step.run.contains('\0')
            || !(1..=3600).contains(&step.timeout_secs)
            || !(0..=125).contains(&step.expect_exit)
        {
            return Err("steps need printable names, nonempty commands, timeout_secs 1..3600, and expect_exit 0..125".into());
        }
    }
    if contract
        .setup
        .iter()
        .chain(&contract.cleanup)
        .any(|s| s.expect_exit != 0)
        || contract
            .scenarios
            .iter()
            .filter(|s| !s.fault)
            .flat_map(|s| &s.steps)
            .any(|s| s.expect_exit != 0)
    {
        return Err("only fault scenario actions may expect nonzero exits".into());
    }
    Ok(())
}

fn plan(contract: &Contract, faults: bool) -> (Report, Vec<&Step>) {
    let mut report = Report {
        schema_version: 1,
        name: contract.name.clone(),
        status: "execution-unverified",
        workspace: None,
        workspace_removed: false,
        steps: Vec::new(),
    };
    let mut steps = Vec::new();
    let mut append = |scenario: &str, operation, fault, step| {
        let step: &Step = step;
        report.steps.push(Record {
            scenario: scenario.into(),
            name: step.name.clone(),
            host: "local",
            operation,
            fault,
            expected_exit: step.expect_exit,
            exit_code: None,
            status: if fault && !faults {
                "skipped"
            } else {
                "planned; execution-unverified"
            },
            duration_ms: None,
            excerpt: String::new(),
        });
        steps.push(step);
    };
    for s in &contract.setup {
        append("setup", "setup", false, s);
    }
    for scenario in &contract.scenarios {
        for s in &scenario.steps {
            append(&scenario.name, "action", scenario.fault, s);
        }
        for s in &scenario.checks {
            append(&scenario.name, "check", scenario.fault, s);
        }
    }
    for s in &contract.cleanup {
        append("cleanup", "cleanup", false, s);
    }
    (report, steps)
}

fn live(
    report: &mut Report,
    steps: &[&Step],
    mut env: BTreeMap<String, String>,
    secrets: &[String],
    source: &Path,
    stream: bool,
) -> Result<(), String> {
    let cancelled = interrupt::install();
    let workspace = tempfile::Builder::new()
        .prefix("nrg-rehearse-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let root = workspace.path().canonicalize().map_err(|e| e.to_string())?;
    let root_text = root.to_string_lossy().into_owned();
    report.workspace = Some(root_text.clone());
    for (name, dir) in [
        ("HOME", "home"),
        ("TMPDIR", "tmp"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ] {
        let directory = root.join(dir);
        std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
        env.insert(name.into(), directory.to_string_lossy().into_owned());
    }
    env.entry("PATH".into())
        .or_insert_with(|| std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
    env.insert("NRG_WORKSPACE".into(), root_text.clone());
    env.insert("NRG_SOURCE".into(), source.to_string_lossy().into_owned());
    env.insert(
        "NRG_BIN".into(),
        std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned(),
    );
    let runner = RealRunner {
        interrupted: Some(cancelled.clone()),
    };
    let cleanup_runner = RealRunner::default();
    let mut failed = false;
    for (record, step) in report.steps.iter_mut().zip(steps) {
        if record.status == "skipped" {
            continue;
        }
        let cleanup = record.operation == "cleanup";
        if !cleanup && (failed || cancelled.load(Ordering::Relaxed)) {
            record.status = "skipped";
            continue;
        }
        eprintln!(
            "[nrg] rehearsal {} / {} (local {})",
            crate::cli::audit::display_safe(&record.scenario),
            crate::cli::audit::display_safe(&record.name),
            record.operation
        );
        let options = RunOptions {
            cwd: Some(root_text.clone()),
            env: env.clone(),
            stdin: step.stdin.clone(),
            timeout_secs: Some(step.timeout_secs),
            stream,
        };
        let started = Instant::now();
        let result = if cleanup { &cleanup_runner } else { &runner }
            .run_isolated_local(&step.run, &options, secrets);
        record.exit_code = Some(result.exit_code);
        record.duration_ms = Some(started.elapsed().as_millis());
        let matched =
            result.exit_code == step.expect_exit && (cleanup || !cancelled.load(Ordering::Relaxed));
        record.status = if matched { "passed" } else { "failed" };
        // Output is redacted by the runner before tail truncation. Keep expected-fault
        // evidence too; matching a nonzero exit is meaningful only with its checks.
        if result.exit_code != 0 || !matched {
            record.excerpt = format!(
                "{}\n{}",
                result
                    .stderr
                    .chars()
                    .rev()
                    .take(1535)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>(),
                result
                    .stdout
                    .chars()
                    .rev()
                    .take(512)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            );
        }
        failed |= !matched;
    }
    report.status = if cancelled.load(Ordering::Relaxed) {
        "interrupted"
    } else if failed {
        "failed"
    } else {
        "passed"
    };
    // Explicit close reports removal failures instead of silently relying on Drop.
    workspace
        .close()
        .map_err(|e| format!("cannot remove rehearsal workspace: {e}"))?;
    report.workspace_removed = true;
    Ok(())
}

pub fn execute(args: &RehearseArgs) -> i32 {
    let mut secrets = HashSet::new();
    let result = (|| -> Result<Report, String> {
        if crate::cli::destination().is_some() {
            return Err(
                "rehearse does not accept --dest; it always creates a fresh local workspace".into(),
            );
        }
        let mut passed_env = BTreeMap::new();
        for name in &args.environment {
            if !valid_env(name) {
                return Err("invalid or reserved --env name".into());
            }
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    secrets.insert(value.clone());
                }
                passed_env.insert(name.clone(), value);
            } else if args.execute {
                return Err(format!("required --env variable {name} is missing"));
            }
        }
        let contract = load(&args.file)?;
        for value in contract.env.values().chain(
            contract
                .setup
                .iter()
                .chain(
                    contract
                        .scenarios
                        .iter()
                        .flat_map(|s| s.steps.iter().chain(&s.checks)),
                )
                .chain(&contract.cleanup)
                .map(|s| &s.stdin),
        ) {
            if !value.is_empty() {
                secrets.insert(value.clone());
            }
        }
        validate(&contract)?;
        if args.execute && !args.faults && contract.scenarios.iter().all(|s| s.fault) {
            return Err("no scenarios selected; this contract requires --faults".into());
        }
        let (mut report, steps) = plan(&contract, args.faults);
        // Names can accidentally contain registered values too. Commands, env values and
        // stdin are never included in reports, including during declaration-only runs.
        report.name = redact(&report.name, &secrets);
        for record in &mut report.steps {
            record.name = redact(&record.name, &secrets);
            record.scenario = redact(&record.scenario, &secrets);
        }
        if args.execute {
            let path = args.file.canonicalize().map_err(|e| e.to_string())?;
            let mut env = contract.env.clone();
            env.extend(passed_env);
            if let Err(e) = live(
                &mut report,
                &steps,
                env,
                &secrets.iter().cloned().collect::<Vec<_>>(),
                path.parent().unwrap(),
                !args.json,
            ) {
                report.status = "failed";
                eprintln!(
                    "Error: {}",
                    crate::cli::audit::display_safe(&redact(&e, &secrets))
                );
            }
            report.workspace = report.workspace.map(|p| redact(&p, &secrets));
        }
        Ok(report)
    })();
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            eprintln!(
                "Error: {}",
                crate::cli::audit::display_safe(&redact(&e, &secrets))
            );
            return 2;
        }
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        for r in &report.steps {
            println!(
                "{} / {}: {} (expected {}, actual {})",
                r.scenario,
                r.name,
                r.status,
                r.expected_exit,
                r.exit_code.map_or("unverified".into(), |c| c.to_string())
            );
            if !r.excerpt.trim().is_empty() {
                println!("{}", crate::cli::audit::display_safe(&r.excerpt));
            }
        }
        println!("Rehearsal '{}': {}", report.name, report.status);
        if !args.execute {
            println!("No commands ran. Use --execute for trusted local commands; --faults enables declared fault scenarios.");
        }
    }
    match report.status {
        "passed" | "execution-unverified" => 0,
        "interrupted" => 130,
        _ => 1,
    }
}
