//! Compile and run a `.rhai` orchestration module, with `import` anchored at the
//! file's own directory so `import "lib/docker" as docker;` resolves to
//! `<file-dir>/lib/docker.rhai`.

use crate::engine::context::SharedCtx;
use rhai::module_resolvers::FileModuleResolver;
use rhai::{Dynamic, Scope};
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
    let secrets = ctx.lock().unwrap().secrets.clone();
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
    let (engine, ast, secrets) = compile(path, ctx)?;

    // GUARD: `call_fn` evaluates the file's ENTIRE top-level before it resolves the function —
    // so calling a missing function on a top-level script (e.g. an `Energize.rhai` that IS a
    // deploy) would run that deploy as a side effect, then fail "not found". Refuse up front if
    // no such function is defined, BEFORE anything runs.
    if !ast.iter_functions().any(|f| f.name == fn_name) {
        return Err(format!(
            "no function `{fn_name}` defined in {}. `nrg run <fn>` calls a function; use \
             `nrg exec {}` to run a top-level script.",
            path.display(),
            path.display()
        ));
    }

    let mut scope = Scope::new();
    // Pass each CLI arg as a Rhai string. The function decides how to coerce them.
    let arg_values: Vec<Dynamic> = args.iter().map(|a| Dynamic::from(a.clone())).collect();
    // `call_fn` evaluates the AST first (loading imports + running top-level config), then
    // calls the function. The return value is discarded — these are effectful entry points.
    let _ret: Dynamic = engine
        .call_fn(&mut scope, &ast, fn_name, arg_values)
        .map_err(|e| crate::engine::secret::redact(&format!("{e}"), &secrets.lock().unwrap()))?;
    Ok(())
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
    fn parse_error_is_reported_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("bad.rhai");
        fs::write(&main, "let x = ;").unwrap();
        let err = run_file(&main, shared(FakeRunner::shared())).unwrap_err();
        assert!(err.contains("parse error"));
    }
}
