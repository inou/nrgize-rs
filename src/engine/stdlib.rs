//! The embedded standard library (roadmap 3.2): `lib/*.rhai`'s core stdlib modules baked into
//! the binary at compile time via `include_str!`, so a project can `import "std/docker"` etc.
//! without ever vendoring `lib/` onto disk — version-locked to the binary, so every project uses
//! the exact same stdlib instead of drifting to its own fork of a hand-copied `lib/`.
//!
//! `import "lib/X"` (a real file on disk, resolved by `FileModuleResolver` — unchanged, still
//! requires vendoring) keeps working exactly as before this feature existed: this module only
//! ADDS the `"std/X"` resolution path alongside it, never touches or falls back through the
//! `"lib/X"` namespace for a project's own engine. `nrg vendor` (see `cli::vendor`) materializes
//! these embedded sources onto disk for projects that want to customize a module.

use rhai::module_resolvers::{FileModuleResolver, ModuleResolversCollection, StaticModuleResolver};
use rhai::{Engine, Module, Scope};
use std::path::PathBuf;

/// (bare name, verbatim source) for every core stdlib module, embedded at COMPILE TIME —
/// deliberately NOT `lib/examples/*`, which are full sample `Energize.rhai` files meant to be
/// copied by hand as a project's OWN starting point, not imported as library modules.
///
/// Listed in DEPENDENCY ORDER (each module's own `import "lib/X"` only ever names one EARLIER in
/// this list — verified against every `lib/*.rhai` file's own imports) so `bootstrap_embedded`
/// can compile them one at a time with every dependency it needs already resolvable.
const EMBEDDED: &[(&str, &str)] = &[
    ("runtime", include_str!("../../lib/runtime.rhai")),
    ("docker", include_str!("../../lib/docker.rhai")),
    ("proxy", include_str!("../../lib/proxy.rhai")),
    ("caddy", include_str!("../../lib/caddy.rhai")),
    ("healthcheck", include_str!("../../lib/healthcheck.rhai")),
    ("registry", include_str!("../../lib/registry.rhai")),
    ("deploy", include_str!("../../lib/deploy.rhai")),
    ("recipe", include_str!("../../lib/recipe.rhai")),
];

/// Every embedded module's bare name and verbatim source, in the SAME order `nrg vendor`
/// (`cli::vendor`) writes them to disk as `lib/<name>.rhai`.
pub fn embedded_modules() -> &'static [(&'static str, &'static str)] {
    EMBEDDED
}

/// Compile every embedded module against `engine` (which must already have every ctx-bound
/// builtin registered, e.g. `ssh_exec`/`state_set` — loading a module runs its top-level `fn`
/// declarations, and while THAT doesn't call them, the resulting `Module`'s functions will be
/// CALLED later through the same engine, so it must already have everything they'll eventually
/// need registered — matching what `FileModuleResolver` implicitly requires for an on-disk file
/// today).
///
/// Each `lib/*.rhai` file's own internal `import "lib/X"` (written for the vendored/on-disk
/// case — see e.g. `lib/deploy.rhai`'s `import "lib/docker" as docker;`) is resolved here against
/// a PRIVATE, bootstrap-only resolver containing ONLY the embedded modules compiled so far,
/// keyed `"lib/<name>"` — built up incrementally and never exposed outside this function. This
/// is what makes the embedded stdlib fully self-contained regardless of whether the CALLING
/// project has vendored anything: `engine`'s REAL module resolver (installed by `install`,
/// below, right after this returns) is completely untouched by this bootstrap process.
fn bootstrap_embedded(engine: &mut Engine) -> StaticModuleResolver {
    let mut internal = StaticModuleResolver::new();
    let mut public = StaticModuleResolver::new();
    for (name, source) in EMBEDDED {
        engine.set_module_resolver(internal.clone());
        let ast = engine.compile(source).unwrap_or_else(|e| {
            panic!("embedded stdlib module {name:?} failed to compile (this is a bug in nrg itself, not a project's script): {e}")
        });
        let module = Module::eval_ast_as_new(Scope::new(), &ast, engine).unwrap_or_else(|e| {
            panic!("embedded stdlib module {name:?} failed to load (this is a bug in nrg itself, not a project's script): {e}")
        });
        internal.insert(format!("lib/{name}"), module.clone());
        public.insert(format!("std/{name}"), module);
    }
    public
}

/// Install the module resolver every `nrg exec`/`nrg run`/`nrg rollback` entry point uses:
/// `import "lib/X"` resolves from disk at `base` (a real, vendored/overridden file — exactly the
/// `FileModuleResolver` behavior this codebase already had, unchanged); `import "std/X"`
/// resolves from the embedded, version-locked copy baked into this binary — works with ZERO
/// vendoring (roadmap 3.2). The embedded resolver is checked FIRST: it only recognizes the exact
/// `"std/<name>"` keys it was built with and reports every other path as not-found, so `"lib/X"`
/// always falls through to `FileModuleResolver` unchanged. Checking it first (rather than last)
/// matters — `FileModuleResolver` resolves ANY relative path against `base`, so if it ran first
/// an on-disk `<base>/std/<name>.rhai` (created by accident, or by a project that doesn't know
/// `"std/X"` is a baked-in name) would silently shadow the embedded, version-locked module that
/// `import "std/X"` is documented to always resolve to.
pub fn install(engine: &mut Engine, base: PathBuf) {
    let embedded = bootstrap_embedded(engine);
    let mut collection = ModuleResolversCollection::new();
    collection.push(embedded);
    collection.push(FileModuleResolver::new_with_path(base));
    engine.set_module_resolver(collection);
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
    fn std_import_resolves_with_zero_vendored_files_on_disk() {
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let dir = tempfile::tempdir().unwrap(); // an empty base — nothing vendored there
        install(&mut engine, dir.path().to_path_buf());
        let name: String = engine
            .eval(r#"import "std/runtime" as rt; rt::runtime_name()"#)
            .unwrap();
        assert_eq!(name, "docker"); // the stdlib's own documented default
    }

    #[test]
    fn lib_import_is_unaffected_and_still_requires_a_real_file_on_disk() {
        // The embedded resolver must NEVER be consulted for "lib/X" from a project's own engine
        // — only "std/X". An unvendored "lib/runtime" must fail exactly as it always has.
        let ctx = shared(FakeRunner::shared());
        let mut engine = crate::engine::build_engine(ctx);
        let dir = tempfile::tempdir().unwrap();
        install(&mut engine, dir.path().to_path_buf());
        let err = engine
            .eval::<rhai::Dynamic>(r#"import "lib/runtime" as rt; rt::runtime_name()"#)
            .unwrap_err();
        assert!(
            format!("{err}").contains("lib/runtime") || format!("{err}").contains("Module not found"),
            "got: {err}"
        );
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
        assert_eq!(name, "docker", "must resolve to the embedded module, not the on-disk file");
    }
}
