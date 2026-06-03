//! Compile and run a `.rhai` orchestration module, with `import` anchored at the
//! file's own directory so `import "lib/docker" as docker;` resolves to
//! `<file-dir>/lib/docker.rhai`.

use crate::engine::context::SharedCtx;
use rhai::module_resolvers::FileModuleResolver;
use std::path::Path;

/// Run the module top-level (exec mode).
pub fn run_file(path: &Path, ctx: SharedCtx) -> Result<(), String> {
    let mut engine = crate::engine::build_engine(ctx);
    let base = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    engine.set_module_resolver(FileModuleResolver::new_with_path(base));
    let ast = engine
        .compile_file(path.to_path_buf())
        .map_err(|e| format!("parse error in {}: {e}", path.display()))?;
    engine.run_ast(&ast).map_err(|e| format!("{e}"))?;
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
    fn parse_error_is_reported_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("bad.rhai");
        fs::write(&main, "let x = ;").unwrap();
        let err = run_file(&main, shared(FakeRunner::shared())).unwrap_err();
        assert!(err.contains("parse error"));
    }
}
