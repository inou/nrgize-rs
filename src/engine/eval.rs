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

/// Build an engine (builtins + module resolver anchored at the file's own directory, so
/// `import "lib/docker" as docker;` resolves to `<file-dir>/lib/docker.rhai`) plus a handle to
/// the secret set (so a thrown error carrying secret-bearing stderr is redacted before printing).
/// Shared by `compile` and `run_fn` (issue #24).
fn build_for(path: &Path, ctx: SharedCtx) -> (rhai::Engine, Secrets) {
    let secrets = ctx.secrets.clone();
    let mut engine = crate::engine::build_engine(ctx);
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    engine.set_module_resolver(FileModuleResolver::new_with_path(base));
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
        let arities = arities.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" or ");
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
    use crate::engine::runner::{FakeRunner, RawOutput};
    use std::fs;

    #[test]
    fn caddy_proxy_boot_throws_when_the_config_write_fails() {
        // Robustness review R3b: lib/caddy.rhai's proxy_boot() used to discard write_remote's
        // result — a non-root deploy user can't write /etc/caddy, but `docker run -d` still
        // returns 0 regardless (Docker's `-v` just creates an empty directory at that path if
        // nothing exists), so Caddy would crash-loop with no config while proxy_boot reported
        // success; the failure only surfaced later as an opaque curl error during the traffic
        // switch. This loads the REAL lib/caddy.rhai (symlinked from the repo, not reimplemented)
        // via a FakeRunner that fails specifically the config-write command, and asserts BOTH
        // that proxy_boot throws with a clear message AND that it never proceeds to start the
        // Caddy container afterward.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(&main, r#"import "lib/caddy" as proxy; proxy::proxy_boot("host1", #{});"#).unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Failed to write Caddy config"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("caddy run")),
            "must not start Caddy after the config write failed: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn docker_run_throws_when_the_env_file_write_fails() {
        // Robustness review R30 (same bug class as R3b, found during its review): docker_run's
        // env-file path is fixed per container NAME (not per run), and accessory_run redeploys
        // reuse a stable name — so an unchecked write on a failed re-run would silently leave
        // `docker run --env-file` reading a STALE file from a prior successful run instead of
        // erroring. This loads the REAL lib/docker.rhai (symlinked, not reimplemented) via a
        // FakeRunner that fails specifically the env-file write, and asserts BOTH the thrown
        // message AND that the container is never actually started afterward.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{ envs: #{ "KEY": "value" } });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Failed to write env-file"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("run -d")),
            "must not start the container after the env-file write failed: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn docker_run_once_throws_when_the_env_file_write_fails() {
        // Same R30 fix, the docker_run_once sibling (used for pre-deploy release tasks like
        // migrations). Its env-file path is keyed by image tag, so repeated releases of the same
        // tag share the same risk of silently reusing a stale file.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run_once("host1", "myapp:v1", "bin/rails db:migrate", #{ envs: #{ "KEY": "value" } });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "cat > ", 1, "Permission denied");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Failed to write env-file"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("run --rm")),
            "must not run the release-task container after the env-file write failed: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn a_previous_runs_persisted_runtime_choice_does_not_leak_into_a_later_run() {
        // Robustness review R27: `set_runtime("podman")` used to persist into the DURABLE
        // project state store, so a run that never calls `set_runtime()` at all (e.g. after the
        // Energize.rhai script is edited to drop that line, reverting to the default) would
        // silently keep resolving to whatever a PAST run last persisted — here we prove a
        // second, independent live invocation against the SAME on-disk project root that never
        // calls set_runtime() still resolves lib/runtime.rhai's container_cmd() to "docker",
        // not the stale "podman" a prior run left behind.
        use crate::engine::context::{shared_with_state, EffectMode};
        use crate::engine::state::StateStore;

        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

        // Run 1: explicitly configure podman. This should persist to disk (so nrg status/logs/
        // app exec can later recover it) — verified below.
        let main1 = dir.path().join("configure.rhai");
        fs::write(&main1, r#"import "lib/runtime" as rt; rt::set_runtime("podman");"#).unwrap();
        let store1 = StateStore::load(dir.path()).unwrap();
        let ctx1 = shared_with_state(FakeRunner::shared(), store1, EffectMode::Live);
        run_file(&main1, ctx1).unwrap();

        let persisted = StateStore::load(dir.path()).unwrap();
        assert_eq!(
            persisted.get("nrg.runtime.cmd"),
            Some("podman".to_string()),
            "set_runtime() must still persist to disk for nrg status/logs/app exec's benefit"
        );

        // Run 2: a SEPARATE invocation against the same project root that never calls
        // set_runtime() at all. It must resolve to the true default ("docker"), not the stale
        // "podman" the first run persisted.
        let main2 = dir.path().join("deploy.rhai");
        fs::write(
            &main2,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app", #{});"#,
        )
        .unwrap();
        let fake2 = FakeRunner::shared();
        let store2 = StateStore::load(dir.path()).unwrap();
        let ctx2 = shared_with_state(fake2.clone(), store2, EffectMode::Live);
        run_file(&main2, ctx2).unwrap();

        assert!(
            fake2.calls().iter().any(|c| c.contains("docker run")),
            "must default to docker (ignoring the previous run's persisted podman choice): {:?}",
            fake2.calls()
        );
        assert!(
            !fake2.calls().iter().any(|c| c.contains("podman run")),
            "must NOT pick up the stale persisted runtime: {:?}",
            fake2.calls()
        );
    }

    #[test]
    fn deploy_re_persists_the_actual_runtime_it_used_even_without_set_runtime() {
        // Found reviewing the R27 fix above: the durable `nrg.runtime.cmd`/`nrg.runtime.name`
        // mirror is only ever WRITTEN by `set_runtime()`/`auto_detect()`. So if a script that
        // once called `set_runtime("podman")` is later edited to drop that call entirely, the
        // NEXT deploy correctly uses docker (the R27 fix works) — but the durable mirror is
        // never told, so it keeps saying "podman" forever, misleading `nrg status`/`nrg logs`/
        // `nrg app exec` about a runtime this service hasn't used since. `deploy()` now
        // re-persists the runtime it actually resolved to on every successful deploy, so the
        // durable copy always reflects the last REAL deploy, not just the last explicit
        // `set_runtime()` call.
        use crate::engine::context::{shared_with_state, EffectMode};
        use crate::engine::state::StateStore;

        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();

        // Run 1: deploy under podman. The durable mirror should now say "podman".
        let main1 = dir.path().join("deploy_v1.rhai");
        fs::write(
            &main1,
            r#"import "lib/runtime" as rt;
               import "lib/deploy" as deploy;
               rt::set_runtime("podman");
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v1", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();
        let fake1 = FakeRunner::shared();
        fake1.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake1.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let store1 = StateStore::load(dir.path()).unwrap();
        let ctx1 = shared_with_state(fake1.clone(), store1, EffectMode::Live);
        run_file(&main1, ctx1).unwrap();

        assert_eq!(
            StateStore::load(dir.path()).unwrap().get("nrg.runtime.cmd"),
            Some("podman".to_string())
        );

        // Run 2: the script is edited to drop `set_runtime("podman")` entirely (reverting to
        // the default), and deploys again. It must use docker (the R27 fix) AND the durable
        // mirror must now say "docker" too — not the stale "podman" from run 1.
        let main2 = dir.path().join("deploy_v2.rhai");
        fs::write(
            &main2,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v2", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();
        let fake2 = FakeRunner::shared();
        fake2.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake2.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let store2 = StateStore::load(dir.path()).unwrap();
        let ctx2 = shared_with_state(fake2.clone(), store2, EffectMode::Live);
        run_file(&main2, ctx2).unwrap();

        assert!(
            fake2.calls().iter().any(|c| c.contains("docker run") || c.contains("docker pull")),
            "the second deploy must actually use docker: {:?}",
            fake2.calls()
        );
        assert!(
            !fake2.calls().iter().any(|c| c.contains("podman")),
            "the second deploy must not use podman: {:?}",
            fake2.calls()
        );
        assert_eq!(
            StateStore::load(dir.path()).unwrap().get("nrg.runtime.cmd"),
            Some("docker".to_string()),
            "the durable mirror must be re-persisted as docker after a deploy that actually \
             used docker, not left stale at the previous deploy's podman"
        );
    }

    #[test]
    fn docker_run_refuses_an_env_value_containing_a_newline() {
        // Robustness review R19: the env-file format is line-based (`KEY=VALUE` per line) — a
        // value containing a literal newline (e.g. a PEM-encoded key from a CI variable) used to
        // silently inject an EXTRA `KEY=VALUE` line into the container's environment instead of
        // being refused. Must throw BEFORE ever writing the env-file (so no stale/partial file is
        // left behind for a later successful retry to read).
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{
                   envs: #{ "SECRET_KEY": "line1\nEVIL=injected" },
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("newline"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("cat > ")),
            "must refuse before ever writing the env-file: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn docker_run_refuses_an_env_key_containing_equals() {
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{
                   envs: #{ "BAD=KEY": "value" },
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("not a valid"), "got: {err}");
    }

    #[test]
    fn docker_run_refuses_an_env_key_containing_a_newline() {
        // The KEY-newline sibling of the value-newline check above — an env var NAME containing
        // a literal newline is just as capable of injecting an extra line into the env-file as a
        // value is.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{
                   envs: #{ "BAD\nKEY": "value" },
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("newline"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("cat > ")),
            "must refuse before ever writing the env-file: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn docker_run_accepts_a_non_string_env_value() {
        // Robustness review (found during this fix's own final review): `envs` map values aren't
        // restricted to strings — `k + "=" + v` already coerced a bare int/bool via Rhai's string
        // concat before this fix, so a config like `envs: #{ PORT: 3000 }` worked. The R19
        // validation must coerce the SAME way before calling `.contains()` on it, or it throws an
        // opaque "Function not found: contains" error for every non-string value instead of
        // validating (or not) anything.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{
                   envs: #{ "PORT": 3000, "DEBUG": true },
               });
               state_set("passed", "true");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));
    }

    #[test]
    fn docker_run_once_refuses_an_env_value_containing_a_newline() {
        // Same R19 fix, the docker_run_once sibling used for pre-deploy release tasks.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run_once("host1", "myapp:v1", "bin/rails db:migrate", #{
                   envs: #{ "SECRET_KEY": "line1\nEVIL=injected" },
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("newline"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("cat > ")),
            "must refuse before ever writing the env-file: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn docker_run_accepts_a_normal_env_value_unaffected_by_the_r19_validation() {
        // Companion regression check: an ordinary env value (no newline, key has no '=') must
        // still work exactly as before.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               docker::docker_run("host1", "myapp:v1", "app-new", #{
                   envs: #{ "DATABASE_URL": "postgres://u:p@db/x" },
               });
               state_set("passed", "true");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));
    }

    #[test]
    fn docker_cleanup_reports_failure_when_container_prune_fails_even_if_image_prune_succeeds() {
        // Found reviewing R6b: docker_cleanup used to return ONLY the image-prune ExecResult
        // unconditionally, discarding the container-prune result entirely — so a caller checking
        // `.ok` (like deploy()'s post-commit loop) couldn't tell an SSH-level failure during the
        // FIRST prune (e.g. a dropped connection) from a clean run, as long as the SECOND prune
        // still happened to succeed. Now returns whichever prune actually failed.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               let r = docker::docker_cleanup("host1");
               state_set("cleanup.ok", "" + r.ok);"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "container prune", 1, "ssh: connection reset by peer");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        assert_eq!(
            ctx.state.lock().unwrap().get("cleanup.ok").as_deref(),
            Some("false"),
            "a failed container-prune must make docker_cleanup report failure, even though the \
             later image-prune succeeded"
        );
        let calls = fake.calls();
        assert!(calls.iter().any(|c| c.contains("container prune")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("image prune")), "{calls:?}");
    }

    #[test]
    fn docker_prune_old_images_keeps_the_newest_n_and_never_removes_protected_tags() {
        // Robustness review R22: docker_prune_old_images removes a repo's own old TAGGED images
        // beyond the `keep_n` most recent, but must NEVER remove a tag in `protect_tags` no matter
        // how old it is (the caller — deploy() — always protects the version just deployed and the
        // one rollback() might still need). Three real tags (v9 newest, v8, v7 oldest) plus a
        // dangling `<none>` entry (which docker_cleanup's own image-prune handles, not this
        // function). keep_n: 1 with v7 (the OLDEST) explicitly protected proves protection is
        // independent of recency — v9 survives as "the 1 most recent", v7 survives because it's
        // protected despite being older than the keep-window, and only v8 (neither newest-kept nor
        // protected) is actually removed.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               let r = docker::docker_prune_old_images("host1", "ghcr.io/org/app", 1, ["v7"]);
               state_set("prune.ok", "" + r.ok);
               state_set("prune.removed", "" + r.removed.len());
               state_set("prune.removed.0", r.removed[0]);"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.respond_cmd(
            "host1",
            "--format '{{.Tag}}|{{.CreatedAt}}'",
            "v9|2024-01-03 10:00:00 +0000 UTC\n\
             v8|2024-01-02 10:00:00 +0000 UTC\n\
             v7|2024-01-01 10:00:00 +0000 UTC\n\
             <none>|2024-01-04 10:00:00 +0000 UTC\n",
        );
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(state.get("prune.ok").as_deref(), Some("true"));
        assert_eq!(
            state.get("prune.removed").as_deref(),
            Some("1"),
            "exactly one tag (v8) should have been removed"
        );
        assert_eq!(state.get("prune.removed.0").as_deref(), Some("ghcr.io/org/app:v8"));

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
            "v8 (neither newest-kept nor protected) must be removed: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v9'")),
            "v9 (the 1 most recent, keep_n=1) must survive: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v7'")),
            "v7 (explicitly protected) must survive despite being the oldest: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("rmi") && c.contains("<none>")),
            "the dangling <none> tag must never be targeted by this function: {calls:?}"
        );
    }

    #[test]
    fn docker_prune_old_images_reports_failure_without_guessing_when_listing_fails() {
        // An SSH-level failure to even LIST images (dropped connection, runtime CLI error) must be
        // reported via `ok: false` with nothing removed — never guessed at by acting on incomplete
        // data.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/docker" as docker;
               let r = docker::docker_prune_old_images("host1", "ghcr.io/org/app", 0, []);
               state_set("prune.ok", "" + r.ok);
               state_set("prune.removed", "" + r.removed.len());"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd(
            "host1",
            "--format '{{.Tag}}|{{.CreatedAt}}'",
            255,
            "ssh: connection reset by peer",
        );
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(state.get("prune.ok").as_deref(), Some("false"));
        assert_eq!(state.get("prune.removed").as_deref(), Some("0"));
        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.contains("rmi")),
            "must not attempt any rmi when the listing itself failed: {calls:?}"
        );
    }

    #[test]
    fn deploy_throws_when_old_container_is_running_but_no_port_state_is_recorded() {
        // Robustness review R4b: when neither `<service>.target.<host>` nor `.port.<host>` state
        // exists, deploy_one_host's old_target used to guess "localhost:<container_port>" (the
        // IN-CONTAINER port) unconditionally. That guess is fine for a genuine first deploy (no
        // old container exists at all — nothing to roll back to regardless). But if a canonical
        // OLD container is ACTUALLY running (fresh CI runner, unshared/lost state.json — NOT a
        // real first deploy), sim_pick_port hands out an essentially arbitrary host port, so
        // "localhost:<container_port>" is almost certainly the WRONG port: a later unwind's
        // "restore proxy" compensation would then "succeed" pointing traffic at a target nothing
        // listens on, while the OTHER compensation still tears down the just-started new
        // container — an outage the rollback itself caused, on a host serving traffic just fine
        // before this deploy attempt. This loads the REAL lib/deploy.rhai (symlinked, not
        // reimplemented) via a FakeRunner whose default response reports EVERY inspect probe as
        // "running" (so both kamal-proxy's own is-it-already-up check and the new old-container
        // check both read "true"), and asserts the throw fires before the new container is ever
        // started or a port is even picked.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
               });"#,
        )
        .unwrap();

        let mut fake_inner = FakeRunner::new();
        fake_inner.default = RawOutput { stdout: "true".to_string(), stderr: String::new(), exit_code: 0 };
        let fake = Arc::new(fake_inner);
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Cannot determine"), "got: {err}");
        assert!(err.contains("app-web"), "got: {err}");
        assert!(err.contains("robustness review R4b"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("nc -z") || c.contains("run -d --restart")),
            "must not pick a port or start the new container before the old-target guard fires: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn deploy_falls_back_to_the_container_port_when_no_old_container_is_running_either() {
        // Companion regression check: when state is ALSO missing but NO old container is running
        // (a genuine first deploy, or a newly added fleet host), the guard must NOT fire — there's
        // nothing to roll back to regardless, so a guessed target can't make anything worse. Uses
        // the plain default FakeRunner (every command reports exit 0, empty stdout): the
        // inspect-running probe's stdout ("") isn't "true", so it honestly reports "not running",
        // matching a genuine first deploy. The deploy is allowed to fail LATER for an unrelated
        // reason (every `nc -z` probe also reports exit 0 = "port busy", so port-picking
        // exhausts its 100 candidates) — the point here is only that it gets PAST the old-target
        // guard without the R4b throw, proven by the distinct "no free host port" error and by
        // `nc -z` calls actually appearing (deliberately NOT letting the whole deploy succeed,
        // since a live health check would otherwise attempt a real, slow HTTP request).
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(!err.contains("Cannot determine"), "the guard must NOT fire here: {err}");
        assert!(err.contains("no free host port"), "expected to reach port-picking: got: {err}");
        assert!(
            fake.calls().iter().any(|c| c.contains("nc -z")),
            "must have reached port-picking (proving old_target fell through cleanly): {:?}",
            fake.calls()
        );
    }

    /// A fake `CommandRunner` for `accessory_run` tests: every `docker inspect -f
    /// '{{.State.Running}}'` probe reports "not running" for the FIRST call, then "running" for
    /// every call after that (simulating a container that starts successfully and stays up). All
    /// other commands (the idempotent `rm -f`, `run -d`, etc.) succeed with empty output. Plain
    /// `FakeRunner` can't express this — it returns the same canned answer for every call matching
    /// a command, and this test genuinely needs the SAME inspect command to answer differently
    /// across two calls to prove the post-start re-check doesn't just always agree with the
    /// pre-start check.
    struct StartsThenStaysUpRunner {
        inspect_calls: std::sync::atomic::AtomicUsize,
    }
    impl crate::engine::runner::CommandRunner for StartsThenStaysUpRunner {
        fn run_ssh(&self, _host: &str, cmd: &str) -> RawOutput {
            if cmd.contains("inspect -f '{{.State.Running}}'") {
                let n = self.inspect_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let running = n >= 1;
                return RawOutput { stdout: running.to_string(), stderr: String::new(), exit_code: 0 };
            }
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    #[test]
    fn accessory_run_removes_a_stopped_but_present_container_before_starting() {
        // Robustness review R10b: a STOPPED-but-present accessory container used to make
        // `docker run --name` fail with "the container name ... is already in use", wedging every
        // future deploy until an operator manually removed it. This asserts the idempotent `rm -f`
        // now runs BEFORE `docker run` in every case (dry-run plan order is the observable
        // contract, same as the caddy/docker_run tests above).
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
        )
        .unwrap();

        let ctx = crate::engine::context::shared_dry(FakeRunner::shared());
        run_file(&main, ctx.clone()).unwrap();
        let plan = ctx.plan.lock().unwrap().clone();
        let rm_pos = plan.iter().position(|a| a.detail.contains("rm -f") && a.detail.contains("redis"));
        let run_pos = plan.iter().position(|a| a.detail.contains("run -d") && a.detail.contains("redis"));
        assert!(rm_pos.is_some() && run_pos.is_some(), "missing rm -f or run -d in plan: {plan:?}");
        assert!(
            rm_pos.unwrap() < run_pos.unwrap(),
            "the idempotent rm -f must run BEFORE docker run --name: {plan:?}"
        );
    }

    #[test]
    fn accessory_run_throws_when_the_container_exits_immediately_after_starting() {
        // Robustness review R10b: `docker run -d`'s exit code only reflects that the container
        // STARTED, not that it's still up a moment later. A FakeRunner whose inspect probe ALWAYS
        // reports "not running" simulates a container that starts (docker run -d succeeds, exit 0)
        // and then crashes near-instantly — the SAME "not running" answer is honest both before
        // the start attempt and in the post-start re-check, so this needs no stateful mock.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("exited immediately after starting"), "got: {err}");
        assert!(err.contains("redis"), "got: {err}");
        assert!(err.contains("robustness review R10b"), "got: {err}");
    }

    #[test]
    fn accessory_run_succeeds_when_the_container_is_still_running_after_starting() {
        // Companion regression check: the R10b post-start re-check must NOT spuriously fail a
        // genuinely healthy start — the ordinary case for almost every real accessory.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy; deploy::accessory_run("host1", "redis", "redis:7", #{});"#,
        )
        .unwrap();

        let fake = Arc::new(StartsThenStaysUpRunner { inspect_calls: std::sync::atomic::AtomicUsize::new(0) });
        run_file(&main, shared(fake.clone())).unwrap();
        // Opus review: without this, the test would pass VACUOUSLY if the mock ever degenerated
        // to reporting "running" on its FIRST call too — accessory_run would then take the
        // `already running` early return (before ever starting/re-checking anything) and this
        // test's only assertion (no throw) would still hold, for the wrong reason. Asserting
        // exactly 2 probes ran proves the pre-start AND post-start checks both actually executed.
        assert_eq!(
            fake.inspect_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must probe exactly twice: once before starting (not yet running), once after (still \
             running) — anything else means this test isn't exercising the real start+recheck path"
        );
    }

    #[test]
    fn run_post_deploy_hook_reports_failed_hosts_but_does_not_throw() {
        // Robustness review R20: deploy()'s post-deploy hook used to discard ssh_exec's result
        // entirely — a hook that failed on some hosts still reported the whole deploy as a full
        // success. run_post_deploy_hook is best-effort BY DESIGN (it runs after the fleet has
        // already committed, so nothing here can roll anything back) — it must not throw, but it
        // must return exactly which host(s) failed and why, instead of silently swallowing it.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               let failed = deploy::run_post_deploy_hook(["host1", "host2"], "bin/rails runner Cache.warm");
               state_set("failed.len", "" + failed.len());
               state_set("failed.0", failed[0]);"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host2", "Cache.warm", 1, "boom: cache backend unreachable");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(state.get("failed.len").as_deref(), Some("1"));
        let msg = state.get("failed.0").unwrap();
        assert!(msg.contains("host2"), "got: {msg}");
        assert!(msg.contains("boom: cache backend unreachable"), "got: {msg}");

        // BOTH hosts must still have been attempted — a failure on host2 must not stop host1 (or
        // vice versa if ordering were reversed): this is what "best-effort" means.
        let calls = fake.calls();
        assert!(calls.iter().any(|c| c.contains("host1")),
            "host1 must still have been attempted: {calls:?}");
        assert!(calls.iter().any(|c| c.contains("host2")), "host2 must still have been attempted: {calls:?}");
    }

    #[test]
    fn run_post_deploy_hook_returns_empty_when_every_host_succeeds() {
        // Companion regression check: the ordinary case (every host succeeds) must return an
        // empty failure list, not spuriously report a failure.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               let failed = deploy::run_post_deploy_hook(["host1", "host2"], "bin/rails runner Cache.warm");
               state_set("failed.len", "" + failed.len());"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        assert_eq!(ctx.state.lock().unwrap().get("failed.len").as_deref(), Some("0"));
    }

    #[test]
    fn kamal_proxy_boot_throws_when_the_image_pull_fails() {
        // Robustness review R25: proxy_boot used to discard the pull's result — a failed pull
        // (network blip, registry auth, rate limit) would silently fall through to `docker run
        // -d`, which happily starts whatever image is ALREADY cached locally (stale, or nothing
        // at all) instead of the fresh one the caller asked for. This loads the REAL
        // lib/proxy.rhai (symlinked, not reimplemented) via a FakeRunner that fails specifically
        // the pull command, and asserts BOTH the thrown message AND that it never proceeds to
        // start the proxy container afterward.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(&main, r#"import "lib/proxy" as proxy; proxy::proxy_boot("host1", #{});"#).unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "pull basecamp/kamal-proxy", 1, "rate limit exceeded");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Failed to pull"), "got: {err}");
        assert!(err.contains("basecamp/kamal-proxy"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("run -d")),
            "must not start kamal-proxy after the pull failed: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn caddy_proxy_boot_throws_when_the_image_pull_fails() {
        // Same R25 fix, the lib/caddy.rhai sibling.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(&main, r#"import "lib/caddy" as proxy; proxy::proxy_boot("host1", #{});"#).unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("host1", "pull caddy:2", 1, "rate limit exceeded");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Failed to pull"), "got: {err}");
        assert!(err.contains("caddy:2"), "got: {err}");
        assert!(
            !fake.calls().iter().any(|c| c.contains("run -d")),
            "must not start Caddy after the pull failed: {:?}",
            fake.calls()
        );
    }

    /// Spawn a real HTTP server whose Nth request (0-indexed) answers `500` if `n` is in
    /// `fail_on`, else `200 OK`. Returns the assigned port and a shared counter of requests
    /// served so far. Used to test `wait_healthy`'s consecutive-pass requirement (robustness
    /// review R12): a caller needs the SAME endpoint to answer differently across sequential
    /// requests (which a canned-response fixture can't do), AND needs to prove exactly how many
    /// requests it took — a test that only checks "eventually passes" can't distinguish "required
    /// N consecutive passes" from "accepted the first pass and got lucky" (both eventually pass
    /// here, since every request past the failing one is a 200).
    fn spawn_flaky_http_server(
        fail_on: std::collections::HashSet<usize>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let counter_thread = counter.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let n = counter_thread.fetch_add(1, Ordering::SeqCst);
                let resp = if fail_on.contains(&n) {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                };
                let _ = stream.write_all(resp);
            }
        });
        (port, counter)
    }

    #[test]
    fn wait_healthy_requires_consecutive_passes_before_returning_healthy() {
        // Robustness review R12: a single passing check used to be enough to consider a container
        // healthy — an app that answers /up once during boot then crashes (or flaps) still got
        // traffic switched to it. With cfg.consecutive: 2, a server that answers 200, 500, 200, 200
        // must NOT pass until the 4th request (the 500 at index 1 resets the streak; only the pair
        // at indices 2-3 are two consecutive 200s) — asserted by checking EXACTLY 4 requests were
        // made, not just that wait_healthy eventually returned without throwing (which the old,
        // single-check behavior would ALSO do, just after only 1 request).
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        let (port, counter) = spawn_flaky_http_server(std::collections::HashSet::from([1]));
        fs::write(
            &main,
            format!(
                r#"import "lib/healthcheck" as health;
                   let r = health::wait_healthy("http://127.0.0.1:{port}/", #{{
                       attempts: 10, interval: 0, consecutive: 2,
                   }});
                   state_set("passed", "true");"#
            ),
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "must take exactly 4 requests (200, 500 resets streak, 200, 200) to see 2 CONSECUTIVE \
             passes — anything else (e.g. 1) means it accepted a single pass instead"
        );
    }

    #[test]
    fn wait_healthy_with_default_consecutive_still_passes_on_the_first_200() {
        // Companion regression check: the historical default (consecutive: 1, i.e. cfg omitted
        // entirely) must still pass on the very first successful check, unchanged from before R12.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        let (port, counter) = spawn_flaky_http_server(std::collections::HashSet::new());
        fs::write(
            &main,
            format!(
                r#"import "lib/healthcheck" as health;
                   health::wait_healthy("http://127.0.0.1:{port}/", #{{ attempts: 3, interval: 0 }});
                   state_set("passed", "true");"#
            ),
        )
        .unwrap();

        let fake = FakeRunner::shared();
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "with the default consecutive: 1, must return after exactly ONE passing request"
        );
    }

    #[test]
    fn wait_healthy_on_host_probes_via_ssh_curl_against_localhost_not_the_control_machine() {
        // Robustness review R7-health: this is the direct proof of the fix. `host` here is a
        // `user@host`-style SSH alias — exactly the form documented for `web_hosts` in
        // docs/deploy.md, and NOT valid as an HTTP authority (userinfo isn't allowed there). The
        // OLD code built "http://" + host + ":" + port + path and GETtted that from the control
        // machine, which would either be a malformed URL or, even if parsed leniently, target the
        // wrong thing. `wait_healthy_on_host` must SSH to that exact alias but curl "localhost" —
        // never the alias — from ON that host.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_on_host("deploy@web1", 3000, #{ attempts: 1 });
               state_set("passed", "true");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.respond_cmd("deploy@web1", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("ssh deploy@web1: ") && c.contains("curl")),
            "must ssh to the exact alias given, not a hostname derived from it: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("http://localhost:3000/up")),
            "must curl localhost on the host itself, never the alias or a control-machine URL: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("deploy@web1:3000") || c.contains("http://deploy@web1")),
            "must never build an HTTP URL out of the raw ssh alias: {calls:?}"
        );
    }

    #[test]
    fn wait_healthy_on_host_throws_after_exhausting_attempts_with_the_last_status() {
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_on_host("web1", 3000, #{ attempts: 2, interval: 0 });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.respond_cmd("web1", "curl -s -o /dev/null", "503");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Health check failed on web1 after 2 attempts"), "got: {err}");
        assert!(err.contains("last status: 503"), "got: {err}");
    }

    #[test]
    fn wait_healthy_on_host_treats_an_ssh_level_failure_as_status_zero() {
        // The curl probe itself can fail at the SSH layer (e.g. the SSH connection drops, or the
        // command can't even run) rather than curl reporting a real HTTP status. `ssh_http_status`
        // must treat that the same as a transport failure (status 0), not crash or hang.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_on_host("web1", 3000, #{ attempts: 1, interval: 0 });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("web1", "curl -s -o /dev/null", 255, "ssh: connection refused");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Health check failed on web1 after 1 attempts"), "got: {err}");
        assert!(err.contains("last status: 0"), "got: {err}");
    }

    /// A `CommandRunner` whose `curl` call reports a NONZERO exit code but STILL prints a
    /// numeric-looking status to stdout (e.g. `curl` itself ran and got a response, but the SSH
    /// wrapper around it reported failure for an unrelated reason — a real, if rare, situation).
    /// This is the one case that distinguishes checking `r.ok` from just trying to `parse_int` the
    /// output: an implementation that dropped the `!r.ok` check but kept the parse/catch would
    /// still "succeed" at parsing "200" and wrongly treat the host as healthy.
    struct FailedExitButNumericStdoutRunner;
    impl crate::engine::runner::CommandRunner for FailedExitButNumericStdoutRunner {
        fn run_ssh(&self, _host: &str, cmd: &str) -> RawOutput {
            if cmd.contains("curl -s -o /dev/null") {
                return RawOutput { stdout: "200".to_string(), stderr: String::new(), exit_code: 1 };
            }
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    #[test]
    fn wait_healthy_on_host_treats_a_nonzero_exit_as_failure_even_with_numeric_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_on_host("web1", 3000, #{ attempts: 1, interval: 0 });"#,
        )
        .unwrap();

        let ctx = shared(Arc::new(FailedExitButNumericStdoutRunner));
        let err = run_file(&main, ctx).unwrap_err();
        assert!(err.contains("Health check failed on web1 after 1 attempts"), "got: {err}");
        assert!(err.contains("last status: 0"), "got: {err} (must not treat a failed ssh_exec as a real 200)");
    }

    /// A fake `CommandRunner` for `wait_healthy_on_host` consecutive-pass tests: the Nth `curl`
    /// call (0-indexed) reports the status at `responses[n]` (or the last entry once exhausted).
    /// Plain `FakeRunner` can't express "the SAME command answers differently across calls" —
    /// this is the `ssh_exec`-routed sibling of `StartsThenStaysUpRunner` above.
    struct SequencedCurlRunner {
        responses: Vec<&'static str>,
        calls: std::sync::atomic::AtomicUsize,
        seen: Mutex<Vec<String>>,
    }
    impl crate::engine::runner::CommandRunner for SequencedCurlRunner {
        fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
            self.seen.lock().unwrap().push(format!("ssh {host}: {cmd}"));
            if cmd.contains("curl -s -o /dev/null") {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let status = self.responses[n.min(self.responses.len() - 1)];
                return RawOutput { stdout: status.to_string(), stderr: String::new(), exit_code: 0 };
            }
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local(&self, _cmd: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_ssh_stdin(&self, _h: &str, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
        fn run_local_stdin(&self, _c: &str, _s: &str) -> RawOutput {
            RawOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }
        }
    }

    #[test]
    fn wait_healthy_on_host_requires_consecutive_passes_before_returning_healthy() {
        // Robustness review R12's consecutive-pass requirement, exercised through the NEW
        // R7-health host-side probe: with cfg.consecutive: 2, a host that answers 200, 503, 200,
        // 200 must NOT pass until the 4th probe (the 503 resets the streak) — asserted by the
        // EXACT probe count, not just "eventually passes" (which the old single-check behavior
        // would also do, just after the 1st probe).
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_on_host("web1", 3000, #{
                   attempts: 10, interval: 0, consecutive: 2,
               });
               state_set("passed", "true");"#,
        )
        .unwrap();

        let runner = Arc::new(SequencedCurlRunner {
            responses: vec!["200", "503", "200", "200"],
            calls: std::sync::atomic::AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let ctx = shared(runner.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));
        assert_eq!(
            runner.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "must take exactly 4 probes (200, 503 resets streak, 200, 200) to see 2 CONSECUTIVE \
             passes — anything else (e.g. 1) means it accepted a single pass instead"
        );
    }

    #[test]
    fn wait_healthy_all_actually_probes_every_host_via_ssh() {
        // Robustness review R8b: tests/deploy_behaviors.rs's own
        // wait_healthy_all_checks_each_host_via_ssh_not_a_control_machine_url only asserts the
        // ABSENCE of a control-machine URL in a dry-run plan — emptying wait_healthy_all's entire
        // body still passes it (found during Fable's R7-health final review). This test runs LIVE
        // and asserts the POSITIVE claim the old test's name promised but never checked: every
        // host in the list is actually curled over its own SSH connection.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_all(["web1", "web2", "web3"], 3000, #{ attempts: 1 });
               state_set("passed", "true");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.respond_cmd("web1", "curl -s -o /dev/null", "200");
        fake.respond_cmd("web2", "curl -s -o /dev/null", "200");
        fake.respond_cmd("web3", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();
        assert_eq!(ctx.state.lock().unwrap().get("passed").as_deref(), Some("true"));

        let calls = fake.calls();
        for host in ["web1", "web2", "web3"] {
            assert!(
                calls.iter().any(|c| c.starts_with(&format!("ssh {host}: ")) && c.contains("curl")),
                "must actually curl {host} over its own ssh connection, not just skip it: {calls:?}"
            );
        }
    }

    #[test]
    fn wait_healthy_all_fails_fast_and_never_probes_a_later_host() {
        // The sibling half of the coverage gap above: when an EARLIER host is unhealthy,
        // wait_healthy_all must throw (propagating wait_healthy_on_host's own exhaustion error)
        // WITHOUT ever probing a LATER host in the list — proving the sequential loop is fail-fast,
        // not "probe all, then report."
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/healthcheck" as health;
               health::wait_healthy_all(["web1", "web2"], 3000, #{ attempts: 1, interval: 0 });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.respond_cmd("web1", "curl -s -o /dev/null", "503");
        fake.respond_cmd("web2", "curl -s -o /dev/null", "200");
        let err = run_file(&main, shared(fake.clone())).unwrap_err();
        assert!(err.contains("Health check failed on web1"), "got: {err}");

        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("ssh web2: ")),
            "must never probe web2 once web1 has already failed: {calls:?}"
        );
    }

    #[test]
    fn post_commit_cleanup_failure_skips_persisting_that_hosts_port_and_target() {
        // Robustness review R6b: deploy()'s post-commit loop used to call docker_rename (x2) /
        // docker_stop / docker_remove / docker_cleanup WITHOUT checking any of their results, then
        // unconditionally persisted <service>.port.<host>/.target.<host> as if the rename dance
        // had completed. Each of those commands bakes `|| true` into the REMOTE shell string (by
        // design, so a harmless retry of an already-done step doesn't fail) — but that can't mask
        // an SSH-level failure (dropped connection, auth failure): the remote shell (and its
        // `|| true`) never even runs, so the returned ExecResult correctly reports `ok: false`.
        // Before this fix that signal was discarded; a host whose SSH connection dropped between
        // commit and cleanup got its state blindly overwritten as if the OLD container had been
        // retired and the NEW one promoted, when in reality NEITHER rename ever happened.
        //
        // This loads the REAL lib/deploy.rhai via a FakeRunner, run in LIVE mode so the fix's
        // ExecResult checks actually execute. Getting all the way to post-commit needs the health
        // check to pass; since R7-health, that's an ssh_exec'd curl on the target host itself (not
        // a real HTTP GET from the control machine), so a FakeRunner response is enough — no real
        // HTTP server needed. `sim_pick_port`'s `nc -z` probe is forced (a nonzero exit means "port
        // free") so it settles on the first candidate.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        fake.fail_cmd("127.0.0.1", "rename", 255, "ssh: broken pipe, connection reset by peer");

        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert!(
            state.get("app.port.127.0.0.1").is_none(),
            "a host whose cleanup failed must NOT get its port persisted as if the swap completed"
        );
        assert!(
            state.get("app.target.127.0.0.1").is_none(),
            "a host whose cleanup failed must NOT get its target persisted as if the swap completed"
        );
        // Service-level state still persists — the fleet-wide traffic switch (inside the
        // transaction, already committed) genuinely succeeded; only this host's post-commit
        // tidying is in question.
        assert_eq!(state.get("app.version").as_deref(), Some("v9"));
        assert_eq!(state.get("app.image").as_deref(), Some("ghcr.io/org/app:v9"));

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.contains("rename")),
            "the rename attempts must still have been made: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("prune")),
            "the REST of that host's cleanup steps must still be attempted (idempotent, try \
             everything, then decide): {calls:?}"
        );
    }

    #[test]
    fn deploy_wires_keep_images_through_to_docker_prune_old_images_with_the_right_protect_tags() {
        // Robustness review R22, end-to-end: a real (LIVE, not dry-run — dry-run's ssh_exec never
        // actually runs, so the prune listing would see empty stdout) deploy() with `keep_images`
        // set must reach `docker_prune_old_images` with the deployed version protected, and must
        // extract the bare repo correctly even from a `registry:port/path` image reference (the
        // same registry-host:port ambiguity `extract_version` already has to handle) — proving
        // `extract_repo` isn't confused by the registry's OWN colon.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "registry.example.com:5000/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1, keep_images: 0,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        fake.respond_cmd(
            "127.0.0.1",
            "--format '{{.Tag}}|{{.CreatedAt}}'",
            "v9|2024-01-03 10:00:00 +0000 UTC\nv8|2024-01-02 10:00:00 +0000 UTC\n",
        );

        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("images 'registry.example.com:5000/app'")),
            "must list the bare repo, unconfused by the registry's own :5000 port: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("rmi 'registry.example.com:5000/app:v8'")),
            "the older, unprotected tag must be pruned: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("rmi 'registry.example.com:5000/app:v9'")),
            "the just-deployed version must be protected regardless of keep_images: 0: {calls:?}"
        );

        // Pruning must never gate the post-commit port/target persistence.
        let state = ctx.state.lock().unwrap();
        assert!(state.get("app.port.127.0.0.1").is_some());
        assert!(state.get("app.target.127.0.0.1").is_some());
    }

    #[test]
    fn deploy_with_keep_images_unset_never_calls_docker_prune_old_images() {
        // Strict opt-in (robustness review R22): omitting cfg.keep_images entirely must leave
        // pruning completely inert — no image listing, no rmi, identical to pre-R22 behavior.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.contains("{{.Tag}}")),
            "keep_images unset must never even list images: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("rmi")),
            "keep_images unset must never remove any image: {calls:?}"
        );
    }

    #[test]
    fn deploy_protects_the_previous_versions_tag_but_only_when_it_is_the_same_repo() {
        // Robustness review R22: the previous version rollback() might still need must survive
        // pruning regardless of age — UNLESS the caller changed image_repo between deploys, in
        // which case `.image`'s old value is a different repo entirely and irrelevant to pruning
        // THIS repo. Pre-seeds `<service>.image` (deploy() reads it as `prev_image` before
        // overwriting it) to simulate "this service was already on some prior version."
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               state_set("app.image", "ghcr.io/org/app:v8");
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1, keep_images: 0,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        fake.respond_cmd(
            "127.0.0.1",
            "--format '{{.Tag}}|{{.CreatedAt}}'",
            "v9|2024-01-03 10:00:00 +0000 UTC\n\
             v8|2024-01-02 10:00:00 +0000 UTC\n\
             v7|2024-01-01 10:00:00 +0000 UTC\n",
        );
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
            "v8 (the PREVIOUS version, same repo) must survive even with keep_images: 0: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v7'")),
            "v7 (neither current nor previous) must still be pruned: {calls:?}"
        );
    }

    #[test]
    fn deploy_does_not_protect_a_previous_versions_tag_from_a_different_repo() {
        // Companion: if the caller changed image_repo between deploys, `.image`'s old value names
        // an UNRELATED repo — its version number is coincidental and must not spuriously protect a
        // same-named tag in the NEW repo.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               state_set("app.image", "ghcr.io/org/OLDREPO:v8");
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1, keep_images: 0,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        fake.respond_cmd(
            "127.0.0.1",
            "--format '{{.Tag}}|{{.CreatedAt}}'",
            "v9|2024-01-03 10:00:00 +0000 UTC\nv8|2024-01-02 10:00:00 +0000 UTC\n",
        );
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.contains("rmi 'ghcr.io/org/app:v8'")),
            "v8 in the NEW repo must NOT be spuriously protected just because an unrelated OLD \
             repo happened to have the same version number: {calls:?}"
        );
    }

    #[test]
    fn deploy_acquires_and_releases_the_cross_machine_lock_on_success() {
        // Robustness review R15: a successful deploy must acquire the remote per-service lock on
        // the first host (an atomic `mkdir`) before doing any real work, and release it (`rm -rf`)
        // once the whole deploy has completed.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.contains("mkdir '/tmp/nrg-deploy-lock-app'")),
            "must acquire the lock before doing any work: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("rm -rf '/tmp/nrg-deploy-lock-app'")),
            "must release the lock after a successful deploy: {calls:?}"
        );
        // The lock's mkdir must happen BEFORE any real work (here, the pull).
        let mkdir_idx = calls.iter().position(|c| c.contains("mkdir '/tmp/nrg-deploy-lock-app'"));
        let pull_idx = calls.iter().position(|c| c.contains("pull "));
        assert!(
            mkdir_idx.unwrap() < pull_idx.unwrap(),
            "lock must be acquired before the pull: {calls:?}"
        );
    }

    #[test]
    fn deploy_refuses_when_the_lock_is_already_held() {
        // Robustness review R15: a concurrent deploy of the SAME service (the lock directory
        // already exists on the lock host) must refuse immediately, before any build/push/pull —
        // proving this is a genuine up-front guard, not just an eventually-detected conflict.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
                   skip_build: true, skip_push: true,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd(
            "web1",
            "mkdir",
            1,
            "mkdir: cannot create directory '/tmp/nrg-deploy-lock-app': File exists",
        );
        let ctx = shared(fake.clone());
        let err = run_file(&main, ctx.clone()).unwrap_err();
        assert!(err.contains("already locked"), "got: {err}");
        assert!(err.contains("robustness review R15"), "got: {err}");

        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.contains("pull ")),
            "must refuse before reaching the pull step: {calls:?}"
        );
    }

    #[test]
    fn deploy_releases_the_lock_even_when_a_later_step_fails() {
        // Robustness review R15: a failure ANYWHERE later in deploy() (here, the pull) must still
        // release the lock before the error propagates — otherwise a single failed deploy would
        // permanently block every future one for this service.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["web1"], "ghcr.io/org/app:v9", "app", #{
                   skip_build: true, skip_push: true,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("web1", "pull", 1, "pull failed: connection reset");
        let ctx = shared(fake.clone());
        let err = run_file(&main, ctx.clone()).unwrap_err();
        assert!(
            err.contains("pull failed") || err.contains("connection reset"),
            "the ORIGINAL pull error must still surface, not be masked by lock release: {err}"
        );

        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.contains("rm -rf '/tmp/nrg-deploy-lock-app'")),
            "the lock must still be released after a later failure: {calls:?}"
        );
    }

    #[test]
    fn deploy_with_skip_lock_never_touches_the_cross_machine_lock() {
        // cfg.skip_lock is an explicit opt-out (the lock depends on remote infrastructure — a
        // writable /tmp, a POSIX shell — this codebase can't unconditionally guarantee for every
        // exotic host) — must leave the lock completely untouched, no mkdir/rm -rf at all.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1, skip_lock: true,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let calls = fake.calls();
        assert!(
            !calls.iter().any(|c| c.contains("nrg-deploy-lock")),
            "skip_lock: true must never touch the lock at all: {calls:?}"
        );
    }

    #[test]
    fn rollback_happy_path_redeploys_the_previous_image_and_swaps_prev() {
        // Robustness review R8b: rollback() had ZERO tests exercising an actual successful
        // rollback — every existing test only covered a REFUSAL path (nested transaction, empty
        // hosts, a mutable ":latest" snapshot, a rejected keep_images override), each of which
        // throws before deploy() is ever reached. This is the first test that runs rollback()
        // all the way through: deploy v1, deploy v2 (which snapshots .prev = v1 automatically),
        // then roll back with NO cfg (the 2-arg overload, using the snapshotted .prev), and assert
        // the full round trip: the live image/version are back to v1, AND the current-before-
        // rollback image (v2) becomes the NEW .prev, so a second rollback would undo this one.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v1", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v2", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });
               deploy::rollback(["127.0.0.1"], "app");"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");
        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(
            state.get("app.image").as_deref(),
            Some("ghcr.io/org/app:v1"),
            "rollback() must actually redeploy the SNAPSHOTTED .prev image"
        );
        assert_eq!(state.get("app.version").as_deref(), Some("v1"));
        assert_eq!(
            state.get("app.prev").as_deref(),
            Some("ghcr.io/org/app:v2"),
            "the image that was live BEFORE the rollback (v2) must become the new rollback \
             target, so a second rollback undoes this one"
        );
        drop(state);

        let calls = fake.calls();
        // >= 2, NOT >= 1: the SETUP's own deploy(v1) already pulls v1 once, so a single v1 pull
        // proves nothing about the rollback (a hollowed-out rollback() that only rewrote
        // .image/.version state without ever calling deploy() still left one v1 pull in the log
        // — caught by this review's own mutation testing). The rollback's internal deploy() must
        // add a SECOND v1 pull of its own.
        assert!(
            calls.iter().filter(|c| c.contains("pull ") && c.contains("v1")).count() >= 2,
            "the rollback's own internal deploy() call must actually pull v1 on the host again \
             (one v1 pull is just the setup deploy's): {calls:?}"
        );
    }

    #[test]
    fn post_commit_cleanup_success_persists_port_and_target_normally() {
        // Companion regression check: when every post-commit command succeeds (the common case),
        // behavior is unchanged from before this fix — port/target ARE persisted.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        // sim_pick_port scans upward from container_port+10000 for the first port a `nc -z` probe
        // reports free (nonzero exit); a blanket `nc -z` failure below makes it settle on the very
        // first candidate, so with container_port: 3000 the picked port is deterministically 13000.
        fs::write(
            &main,
            r#"import "lib/deploy" as deploy;
               deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{
                   container_port: 3000, skip_build: true, skip_push: true,
                   health_attempts: 1,
               });"#,
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", "nc -z", 1, "");
        fake.respond_cmd("127.0.0.1", "curl -s -o /dev/null", "200");

        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(
            state.get("app.port.127.0.0.1").as_deref(),
            Some("13000"),
            "a fully successful cleanup must still persist the host's new port"
        );
        assert_eq!(
            state.get("app.target.127.0.0.1").as_deref(),
            Some("localhost:13000"),
            "a fully successful cleanup must still persist the host's new target"
        );
    }

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
    fn run_fn_wrong_arity_does_not_run_top_level() {
        // Regression (#18): `nrg run deploy` with the wrong arg count must fail on arity WITHOUT
        // evaluating the top level (which here has a side effect that must NOT happen).
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(
            &main,
            r#"local_exec("touch should-not-run"); fn deploy(host) { ssh_exec(host, "x"); }"#,
        )
        .unwrap();
        let fake = FakeRunner::shared();
        // deploy takes 1 arg; call with 0.
        let err = run_fn(&main, "deploy", &[], shared(fake.clone())).unwrap_err();
        assert!(err.contains("expects 1") && err.contains("Nothing was run"), "got: {err}");
        assert!(fake.calls().is_empty(), "the top level must NOT run on an arity mismatch");
    }

    #[test]
    fn run_fn_rejects_non_identifier_name() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Energize.rhai");
        fs::write(&main, r#"fn ok() {}"#).unwrap();
        let fake = FakeRunner::shared();
        let err = run_fn(&main, "ok(); evil_call(", &[], shared(fake.clone())).unwrap_err();
        assert!(err.contains("not a valid function name"), "got: {err}");
        assert!(fake.calls().is_empty());
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
