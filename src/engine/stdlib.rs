//! The embedded standard library: the core `lib/*.rhai` modules baked into the binary at compile
//! time via `include_str!`, so a script can `import "std/docker"` etc. Version-locked to the
//! binary, so every project uses the exact same stdlib.

use rhai::module_resolvers::{FileModuleResolver, ModuleResolversCollection, StaticModuleResolver};
use rhai::{Engine, Module, Scope};
use std::path::PathBuf;

/// (bare name, verbatim source) for every core stdlib module, embedded at COMPILE TIME —
/// deliberately NOT `lib/examples/*`, which are full sample `Energize.rhai` files meant to be
/// copied by hand as a project's OWN starting point, not imported as library modules.
///
/// Listed in DEPENDENCY ORDER (each module's own `import "std/X"` only ever names one EARLIER in
/// this list) so `bootstrap_embedded` can compile them one at a time with every dependency it
/// needs already resolvable.
const EMBEDDED: &[(&str, &str)] = &[
    ("contracts", include_str!("../../lib/contracts.rhai")),
    ("release", include_str!("../../lib/release.rhai")),
    (
        "release_recipes",
        include_str!("../../lib/release_recipes.rhai"),
    ),
    ("mise", include_str!("../../lib/mise.rhai")),
    ("runtime", include_str!("../../lib/runtime.rhai")),
    ("docker", include_str!("../../lib/docker.rhai")),
    ("caddy", include_str!("../../lib/caddy.rhai")),
    ("healthcheck", include_str!("../../lib/healthcheck.rhai")),
    ("registry", include_str!("../../lib/registry.rhai")),
    ("deploy", include_str!("../../lib/deploy.rhai")),
    ("recipe", include_str!("../../lib/recipe.rhai")),
    ("notify", include_str!("../../lib/notify.rhai")),
    ("bunny", include_str!("../../lib/bunny.rhai")),
];

/// Compile every embedded module against `engine` (which must already have every ctx-bound
/// builtin registered, e.g. `ssh_exec`/`state_set` — loading a module runs its top-level `fn`
/// declarations, and while THAT doesn't call them, the resulting `Module`'s functions will be
/// CALLED later through the same engine, so it must already have everything they'll eventually
/// need registered — matching what `FileModuleResolver` implicitly requires for an on-disk file
/// today).
///
/// Each module's own `import "std/X"` is resolved while bootstrapping against a resolver holding
/// only the modules compiled so far; the finished resolver is what `install` exposes.
fn bootstrap_embedded(engine: &mut Engine) -> StaticModuleResolver {
    let mut resolver = StaticModuleResolver::new();
    for (name, source) in EMBEDDED {
        engine.set_module_resolver(resolver.clone());
        let ast = engine.compile(source).unwrap_or_else(|e| {
            panic!("embedded stdlib module {name:?} failed to compile (this is a bug in nrg itself, not a project's script): {e}")
        });
        let module = Module::eval_ast_as_new(Scope::new(), &ast, engine).unwrap_or_else(|e| {
            panic!("embedded stdlib module {name:?} failed to load (this is a bug in nrg itself, not a project's script): {e}")
        });
        resolver.insert(format!("std/{name}"), module);
    }
    resolver
}

/// Install the module resolver every `nrg exec`/`nrg run` entry point uses: `import "std/X"`
/// resolves from the embedded copy baked into this binary; any other path is a file relative to
/// `base`. The embedded resolver is checked FIRST: `FileModuleResolver` resolves ANY relative
/// path against `base`, so if it ran first an on-disk `<base>/std/<name>.rhai` would silently
/// shadow the version-locked module `import "std/X"` is documented to always resolve to.
pub fn install(engine: &mut Engine, base: PathBuf) {
    let embedded = bootstrap_embedded(engine);
    let mut collection = ModuleResolversCollection::new();
    collection.push(embedded);
    collection.push(FileModuleResolver::new_with_path(base));
    engine.set_module_resolver(collection);
}

/// Install only the embedded stdlib, for the synthesized scripts of `nrg rollback`/`nrg setup`
/// that import nothing from disk.
pub fn install_embedded_only(engine: &mut Engine) {
    let embedded = bootstrap_embedded(engine);
    engine.set_module_resolver(embedded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared;
    use crate::engine::runner::FakeRunner;

    #[test]
    fn every_embedded_module_compiles_and_loads_cleanly() {
        // A direct unit-level canary for the panics inside bootstrap_embedded: if any lib/*.rhai
        // file is ever edited in a way that breaks compilation or load, this fails FAST and
        // precisely (naming the module), instead of surfacing as a confusing panic the first
        // time some unrelated `nrg exec` happens to touch `install`.
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let resolver = bootstrap_embedded(&mut engine);
        for (name, _) in EMBEDDED {
            assert!(
                resolver.contains_path(&format!("std/{name}")),
                "expected std/{name} to have been registered"
            );
        }
    }

    #[test]
    fn std_import_resolves_with_an_empty_project_dir() {
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let dir = tempfile::tempdir().unwrap(); // an empty base
        install(&mut engine, dir.path().to_path_buf());
        let name: String = engine
            .eval(r#"import "std/runtime" as rt; rt::runtime_name()"#)
            .unwrap();
        assert_eq!(name, "docker"); // the stdlib's own documented default
    }

    #[test]
    fn a_relative_import_is_a_local_file() {
        // A project's own `ops/foo.rhai` is an ordinary relative import, not a stdlib mirror.
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ops")).unwrap();
        std::fs::write(dir.path().join("ops/foo.rhai"), "fn n() { 7 }").unwrap();
        install(&mut engine, dir.path().to_path_buf());
        let n: i64 = engine.eval(r#"import "ops/foo" as f; f::n()"#).unwrap();
        assert_eq!(n, 7);
    }

    #[test]
    fn an_on_disk_std_file_never_shadows_the_embedded_stdlib() {
        // FileModuleResolver resolves ANY relative path against `base`, including "std/X" — so
        // if it were consulted before the embedded resolver, a project with a real (accidental
        // or malicious) `<base>/std/runtime.rhai` could silently override the version-locked
        // embedded module `import "std/runtime"` is documented to always resolve to.
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("std")).unwrap();
        std::fs::write(
            dir.path().join("std/runtime.rhai"),
            r#"fn runtime_name() { "SHADOWED-FROM-DISK" }"#,
        )
        .unwrap();
        install(&mut engine, dir.path().to_path_buf());
        let name: String = engine
            .eval(r#"import "std/runtime" as rt; rt::runtime_name()"#)
            .unwrap();
        assert_eq!(
            name, "docker",
            "must resolve to the embedded module, not the on-disk file"
        );
    }
}
