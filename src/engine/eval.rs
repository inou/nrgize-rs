//! Compile and run a `.rhai` orchestration module, with `import` anchored at the
//! file's own directory so `import "lib/docker" as docker;` resolves to
//! `<file-dir>/lib/docker.rhai`.

use crate::engine::context::SharedCtx;
use rhai::module_resolvers::FileModuleResolver;
use rhai::Scope;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The shared set of registered secret plaintexts, used to redact thrown errors.
type Secrets = Arc<Mutex<HashSet<String>>>;

/// A compiled module ready to run: the engine (with builtins + module resolver), the AST,
/// and the secret set (so thrown errors can be redacted before printing).
type Compiled = (rhai::Engine, rhai::AST, Secrets);

/// Build an engine + AST for `path` with the module resolver anchored at the file's own
/// directory (so `import "lib/docker" as docker;` resolves to `<file-dir>/lib/docker.rhai`).
fn compile(path: &Path, ctx: SharedCtx) -> Result<Compiled, String> {
    // Keep a handle to the secret set so a thrown error (which can carry a secret-bearing
    // command stderr) is redacted before it's printed by the caller.
    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    engine.set_module_resolver(FileModuleResolver::new_with_path(base));
    let ast = engine
        .compile_file(path.to_path_buf())
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    Ok((engine, ast, secrets))
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

    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    engine.set_module_resolver(FileModuleResolver::new_with_path(base));
    let ast = engine
        .compile(&augmented)
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;

    // GUARD (before anything RUNS — `compile` does not execute): refuse if the function isn't
    // defined, so `nrg run <typo>` against a top-level script can't run the script as a side
    // effect and then fail "not found".
    if !ast.iter_functions().any(|f| f.name == fn_name) {
        return Err(format!(
            "no function `{fn_name}` defined in {}. `nrg run <fn>` calls a function; use \
             `nrg exec {}` to run a top-level script.",
            path.display(),
            path.display()
        ));
    }

    engine
        .run_ast_with_scope(&mut scope, &ast)
        .map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
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
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;
    use std::fs;

    #[test]
    fn runs_a_script_that_imports_a_lib_module() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        // lib/docker.rhai defines pull() which calls the GLOBAL ssh_exec builtin.
        fs::write(
            dir.path().join("lib/docker.rhai"),
            r#"fn pull(host, img) { ssh_exec(host, "docker pull " + img); }"#,
        )
        .unwrap();
        // main calls the imported module fn.
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker; docker::pull("web1", "nginx:latest");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        run_file(&main, shared(fake.clone())).unwrap();
        // PROVES the global builtin executed from inside the imported module fn.
        assert_eq!(
            fake.calls(),
            vec!["ssh web1: docker pull nginx:latest".to_string()]
        );
    }

    #[test]
    fn run_fn_resolves_calls_into_a_module_that_itself_imports() {
        // Regression: `nrg run ship` where ship calls a module fn (`mid::build`) and that module
        // itself `import`s another (`base`). Rhai's `call_fn` cannot resolve this nested-import
        // case (it fails "Function not found: ship"); run_fn appends the call + run_ast instead.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        fs::write(dir.path().join("lib/base.rhai"), r#"fn tag() { "v1" }"#).unwrap();
        fs::write(
            dir.path().join("lib/mid.rhai"),
            r#"import "lib/base" as base; fn build(host) { ssh_exec(host, "build " + base::tag()); }"#,
        )
        .unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/mid" as mid; fn ship(host) { mid::build(host); }"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        run_fn(&main, "ship", &["web1".to_string()], shared(fake.clone())).unwrap();
        assert_eq!(fake.calls(), vec!["ssh web1: build v1".to_string()]);
    }

    #[test]
    fn run_fn_refuses_missing_function_without_running_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Energize.rhai");
        // A top-level script with NO function definitions and a side effect that must NOT run.
        fs::write(&main, r#"local_exec("touch should-not-run");"#).unwrap();
        let fake = FakeRunner::shared();
        let err = run_fn(&main, "deploy", &[], shared(fake.clone())).unwrap_err();
        assert!(err.contains("no function"), "got: {err}");
        assert!(
            fake.calls().is_empty(),
            "the top-level must NOT run when the named function is missing"
        );
    }

    #[test]
    fn list_functions_lifts_the_expression_depth_cap() {
        // A long `+` chain exceeds Rhai's default function-body expression-depth cap (32) — the
        // real stdlib hits this with its command/message string concatenations. `list_functions`
        // (backing `nrg tasks`/`nrg doctor`) must lift the cap like `build_engine` does.
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Energize.rhai");
        let chain: String = (0..50).map(|i| i.to_string()).collect::<Vec<_>>().join(" + ");
        fs::write(&main, format!("fn total() {{ {chain} }}")).unwrap();
        let fns = list_functions(&main).unwrap();
        assert!(fns.iter().any(|f| f.name == "total"));
    }

    #[test]
    fn parse_error_is_reported_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("bad.rhai");
        fs::write(&main, "let x = ;").unwrap();
        let err = run_file(&main, shared(FakeRunner::shared())).unwrap_err();
        assert!(err.contains("parse error"));
    }
}
