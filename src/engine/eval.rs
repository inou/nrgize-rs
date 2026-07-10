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

    /// Spawn a minimal real HTTP server on an OS-assigned loopback port that answers every
    /// request with `200 OK`. Returns the assigned port. Used to let a LIVE-mode deploy's health
    /// check (`sim_http_healthy`, which does a REAL GET even in live mode) actually succeed — a
    /// `FakeRunner` only intercepts ssh/local exec, not HTTP. The listener loops for the life of
    /// the test process; not joined (fine — a background thread is abandoned, not leaked across
    /// runs, when the test process exits).
    fn spawn_ok_http_server() -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        port
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
        // check to pass, and `sim_http_healthy` does a REAL HTTP GET even in live mode (FakeRunner
        // only intercepts ssh/local exec) — so a genuine local HTTP server stands in for the new
        // container, and `sim_pick_port`'s `nc -z` probe is forced (via a targeted FakeRunner
        // failure — a nonzero `nc -z` exit means "port free") to hand out EXACTLY that server's
        // port, so the health check's URL actually reaches it.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        let server_port = spawn_ok_http_server();
        let container_port = server_port as i64 - 10000; // sim_pick_port starts at container_port+10000
        fs::write(
            &main,
            format!(
                r#"import "lib/deploy" as deploy;
                   deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{{
                       container_port: {container_port}, skip_build: true, skip_push: true,
                       health_attempts: 1,
                   }});"#
            ),
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", &format!("nc -z localhost {server_port}"), 1, "");
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
    fn post_commit_cleanup_success_persists_port_and_target_normally() {
        // Companion regression check: when every post-commit command succeeds (the common case),
        // behavior is unchanged from before this fix — port/target ARE persisted.
        let dir = tempfile::tempdir().unwrap();
        let repo_lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo_lib, dir.path().join("lib")).unwrap();
        let main = dir.path().join("Energize.rhai");

        let server_port = spawn_ok_http_server();
        let container_port = server_port as i64 - 10000;
        fs::write(
            &main,
            format!(
                r#"import "lib/deploy" as deploy;
                   deploy::deploy(["127.0.0.1"], "ghcr.io/org/app:v9", "app", #{{
                       container_port: {container_port}, skip_build: true, skip_push: true,
                       health_attempts: 1,
                   }});"#
            ),
        )
        .unwrap();

        let fake = FakeRunner::shared();
        fake.fail_cmd("127.0.0.1", &format!("nc -z localhost {server_port}"), 1, "");

        let ctx = shared(fake.clone());
        run_file(&main, ctx.clone()).unwrap();

        let state = ctx.state.lock().unwrap();
        assert_eq!(
            state.get("app.port.127.0.0.1").as_deref(),
            Some(&server_port.to_string()[..]),
            "a fully successful cleanup must still persist the host's new port"
        );
        assert_eq!(
            state.get("app.target.127.0.0.1").as_deref(),
            Some(format!("localhost:{server_port}")).as_deref(),
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
