//! Compile and run a `.rhai` orchestration module, with `import` anchored at the
//! file's own directory so `import "ops/helpers" as h;` resolves to
//! `<file-dir>/ops/helpers.rhai`.

use crate::engine::context::SharedCtx;
use rhai::Scope;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The shared set of registered secret plaintexts, used to redact thrown errors.
type Secrets = Arc<Mutex<HashSet<String>>>;

/// A compiled module ready to run: the engine (with builtins + module resolver), the AST,
/// and the secret set (so thrown errors can be redacted before printing).
type Compiled = (rhai::Engine, rhai::AST, Secrets);

/// Build an engine (builtins + module resolver anchored at the file's own directory, so
/// `import "ops/helpers" as h;` resolves to `<file-dir>/ops/helpers.rhai`, and
/// `import "std/docker" as docker;` resolves to the embedded, version-locked stdlib baked into
/// this binary — see `engine::stdlib`) plus a handle to the secret set (so a thrown
/// error carrying secret-bearing stderr is redacted before printing). Shared by `compile` and
/// `run_fn` (issue #24).
fn build_for(path: &Path, ctx: SharedCtx) -> (rhai::Engine, Secrets) {
    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    crate::engine::stdlib::install(&mut engine, base);
    (engine, secrets)
}

/// Build an engine + AST for `path`.
fn compile(path: &Path, ctx: SharedCtx) -> Result<Compiled, String> {
    let (engine, secrets) = build_for(path, ctx);
    let ast = engine
        .compile_file(path.to_path_buf())
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    Ok((engine, ast, secrets))
}

/// Whether `s` is a valid Rhai identifier (so it can be safely spliced into source as a function
/// call). Leading letter/underscore, then letters/digits/underscores. Makes the "the post-compile
/// name check rejects junk" property INTENTIONAL rather than incidental (issue #18).
fn is_rhai_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Run the module top-level (exec mode).
pub fn run_file(path: &Path, ctx: SharedCtx) -> Result<(), String> {
    let (engine, ast, secrets) = compile(path, ctx)?;
    engine
        .run_ast(&ast)
        .map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
}

/// Load `path` into the engine (same builtins/module-resolution as `nrg exec`) and call the
/// script-defined function `fn_name`, passing each element of `args` as a string parameter.
///
/// This backs `nrg run <fn> [args...]` for `.rhai` files: the file's top level is evaluated
/// first (so `import`s, config, and `set_runtime(...)` run), then the named function is
/// invoked with the raw CLI string args. A missing function, an arity mismatch, or an
/// uncaught `throw` surfaces as `Err` (redacted), which the caller maps to a non-zero exit.
pub fn run_fn(path: &Path, fn_name: &str, args: &[String], ctx: SharedCtx) -> Result<(), String> {
    // Validate the function name as a Rhai identifier BEFORE splicing it into source. The call is
    // built by `format!("{content}\n{fn_name}(...)")`, so a non-identifier name could otherwise
    // inject syntax; rejecting it here makes that impossible by construction (issue #18).
    if !is_rhai_ident(fn_name) {
        return Err(format!(
            "`{fn_name}` is not a valid function name (must be a Rhai identifier: letters, digits, \
             underscore; not starting with a digit)."
        ));
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // We run `nrg run <fn> a b` by APPENDING a `<fn>(a, b);` statement to the file and
    // evaluating the whole thing via `run_ast` — the SAME path as `nrg exec`. We deliberately
    // do NOT use `engine.call_fn`: with nested module imports (e.g. `deploy.rhai` imports
    // `docker.rhai` which imports `runtime.rhai`), `call_fn` fails to resolve a function whose
    // body makes a qualified module call (`deploy::deploy(...)`). `run_ast` handles it
    // correctly. Args are passed as injected scope variables (no string-literal escaping).
    let mut scope = Scope::new();
    let arg_names: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let name = format!("__nrg_arg_{i}");
            scope.push(name.clone(), a.clone());
            name
        })
        .collect();
    let augmented = format!("{content}\n{fn_name}({});\n", arg_names.join(", "));

    let (engine, secrets) = build_for(path, ctx);
    let ast = engine
        .compile(&augmented)
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;

    // GUARD (before anything RUNS — `compile` does not execute): check BOTH that the function is
    // defined AND that an overload with the right arity exists. Checking arity here (not letting
    // Rhai fail at the call site) means `nrg run deploy` with the wrong arg count can't evaluate
    // the whole top level — which may contain `local_exec`/side effects — and only then fail
    // (issue #18). `iter_functions()` already exposes `params`.
    let defined: Vec<usize> = ast
        .iter_functions()
        .filter(|f| f.name == fn_name)
        .map(|f| f.params.len())
        .collect();
    if defined.is_empty() {
        return Err(format!(
            "no function `{fn_name}` defined in {}. `nrg run <fn>` calls a function; use \
             `nrg exec {}` to run a top-level script.",
            path.display(),
            path.display()
        ));
    }
    if !defined.contains(&args.len()) {
        let mut arities: Vec<usize> = defined;
        arities.sort_unstable();
        arities.dedup();
        let arities = arities
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        return Err(format!(
            "function `{fn_name}` expects {arities} argument(s), but {} were given. \
             (Nothing was run.)",
            args.len()
        ));
    }

    engine
        .run_ast_with_scope(&mut scope, &ast)
        .map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
}

/// Roll `service` back via the stdlib's `deploy::rollback(hosts, service, cfg)`, without
/// requiring the project's own orchestration file to define any wrapper function around it
/// (closes the "requires the script to wire it up" gap `nrg run` alone leaves
/// open). Unlike `run_fn`, which only calls a function the PROJECT itself defines, this
/// synthesizes a direct `import "…/deploy" as deploy;` and calls its `rollback` overload.
/// `hosts`/`service`/`image` are passed as injected scope variables — like `run_fn`'s CLI args —
/// not spliced into source, so none of them need `is_rhai_ident`-style validation.
///
/// Always uses the embedded, version-locked `import "std/deploy"`.
pub fn run_rollback(
    hosts: &[String],
    service: &str,
    image: Option<&str>,
    ctx: SharedCtx,
) -> Result<(), String> {
    {
        let state = ctx.state.lock().unwrap();
        for suffix in ["cmd", "name"] {
            if let Some(value) = state
                .get(&format!("{service}.runtime.{suffix}"))
                .or_else(|| state.get(&format!("nrg.runtime.{suffix}")))
            {
                if !matches!(value.as_str(), "docker" | "podman" | "nerdctl" | "orbstack") {
                    return Err("invalid persisted container runtime".into());
                }
                ctx.session
                    .lock()
                    .unwrap()
                    .insert(format!("nrg.runtime.{suffix}"), value);
            }
        }
        fn register(ctx: &SharedCtx, value: &serde_json::Value) {
            match value {
                serde_json::Value::String(s) if !s.is_empty() => ctx.register_secret(s),
                serde_json::Value::Array(a) => {
                    for v in a {
                        register(ctx, v);
                    }
                }
                serde_json::Value::Object(o) => {
                    for v in o.values() {
                        register(ctx, v);
                    }
                }
                _ => (),
            }
        }
        for key in ["config", "prev_config"] {
            if let Some(config) = state.get(&format!("{service}.{key}")) {
                if let Ok(value) = serde_json::from_str(&config) {
                    register(&ctx, &value);
                }
            }
        }
    }
    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    crate::engine::stdlib::install_embedded_only(&mut engine);

    let result = attempt_rollback(&engine, hosts, service, image)?;

    result.map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
}

/// Compile and run the synthesized rollback call against `std/deploy`. Returns `Err(String)` for
/// a compile failure (an nrg-internal bug — the synthesized source is always valid Rhai, this
/// can't come from a project's own script) and `Ok(Err(..))` for a RUNTIME failure (the
/// stdlib's own `rollback()` logic).
fn attempt_rollback(
    engine: &rhai::Engine,
    hosts: &[String],
    service: &str,
    image: Option<&str>,
) -> Result<Result<(), Box<rhai::EvalAltResult>>, String> {
    let mut scope = Scope::new();
    let hosts_arr: rhai::Array = hosts.iter().cloned().map(rhai::Dynamic::from).collect();
    scope.push("__nrg_hosts", hosts_arr);
    scope.push("__nrg_service", service.to_string());
    scope.push("__nrg_image", image.unwrap_or("").to_string());

    let source = r#"
        import "std/deploy" as deploy;
        let __nrg_cfg = #{};
        if __nrg_image != "" { __nrg_cfg.image = __nrg_image; }
        deploy::rollback(__nrg_hosts, __nrg_service, __nrg_cfg);
    "#;
    let ast = engine
        .compile(source)
        .map_err(|e| format!("internal error compiling the rollback call: {e}"))?;
    Ok(engine.run_ast_with_scope(&mut scope, &ast))
}

/// Bootstrap a fresh fleet's network + proxy via the stdlib's
/// `docker::docker_network_create_all(hosts, network)` (skipped if `network` is empty) and
/// `caddy::proxy_boot_all(hosts, cfg)`.
///
/// Deliberately does NOT handle installing a container runtime on hosts that lack one — that's
/// `nrg setup`'s own native (non-Rhai) preflight, done over raw SSH before this is ever called,
/// since it needs to run BEFORE any Rhai-side container primitive could possibly work, and
/// because whether to run a root-level installer script is a judgment call the CLI layer gates
/// behind its own `--yes` confirmation, independent of this function.
pub fn run_setup(
    hosts: &[String],
    proxy_version: Option<&str>,
    network: Option<&str>,
    ctx: SharedCtx,
) -> Result<(), String> {
    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    crate::engine::stdlib::install_embedded_only(&mut engine);

    let result = attempt_setup(&engine, hosts, proxy_version, network)?;

    result.map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
}

/// Compile and run the synthesized network-create + proxy-boot call against `std/docker` and
/// `std/caddy`. Same `Ok(Err(..))` vs. `Err(String)` split as `attempt_rollback`.
fn attempt_setup(
    engine: &rhai::Engine,
    hosts: &[String],
    proxy_version: Option<&str>,
    network: Option<&str>,
) -> Result<Result<(), Box<rhai::EvalAltResult>>, String> {
    let mut scope = Scope::new();
    let hosts_arr: rhai::Array = hosts.iter().cloned().map(rhai::Dynamic::from).collect();
    scope.push("__nrg_hosts", hosts_arr);
    scope.push(
        "__nrg_proxy_version",
        proxy_version.unwrap_or("").to_string(),
    );
    scope.push("__nrg_network", network.unwrap_or("").to_string());

    let source = r#"
        import "std/docker" as docker;
        import "std/caddy" as proxy;
        if __nrg_network != "" { docker::docker_network_create_all(__nrg_hosts, __nrg_network); }
        let __nrg_cfg = #{};
        if __nrg_proxy_version != "" { __nrg_cfg.proxy_version = __nrg_proxy_version; }
        proxy::proxy_boot_all(__nrg_hosts, __nrg_cfg);
    "#;
    let ast = engine
        .compile(source)
        .map_err(|e| format!("internal error compiling the setup call: {e}"))?;
    Ok(engine.run_ast_with_scope(&mut scope, &ast))
}

/// A function defined at the top level of an orchestration file: its name and parameter count.
pub struct FnInfo {
    pub name: String,
    pub params: usize,
}

/// Compile `path` (parse only — nothing runs) and return the script-defined functions, sorted
/// by name. Backs `nrg tasks` / `nrg doctor`: it lists/validates the callable functions in an
/// `Energize.rhai`. Compilation neither executes the top level nor resolves `import`s, but the
/// expression-nesting cap must be lifted (same as `build_engine`) or a real deploy file with
/// nested `#{}` config maps fails to parse with "Expression exceeds maximum complexity".
pub fn list_functions(path: &Path) -> Result<Vec<FnInfo>, String> {
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(0, 0);
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let ast = engine
        .compile(&content)
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    let mut fns: Vec<FnInfo> = ast
        .iter_functions()
        .map(|f| FnInfo {
            name: f.name.to_string(),
            params: f.params.len(),
        })
        .collect();
    fns.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(fns)
}

#[cfg(test)]
mod tests;
