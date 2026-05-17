//! FileLoader implementation for `load()` statements in Starlark files.
//!
//! Resolves paths relative to the file that contains the `load()` call,
//! evaluates the loaded module with the same globals (runtime primitives),
//! freezes it, and caches the result so each file is only evaluated once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dupe::Dupe;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::FileLoader;
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

/// A file loader that resolves paths relative to a base directory,
/// evaluates modules with shared globals, and caches results.
pub struct NrgFileLoader {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Shared globals (runtime primitives + standard builtins).
    globals: Globals,
    /// Cache of already-loaded modules. Uses RefCell because FileLoader::load takes &self.
    cache: RefCell<HashMap<PathBuf, FrozenModule>>,
    /// Whether trace logging is enabled.
    trace: bool,
}

impl NrgFileLoader {
    pub fn new(base_dir: PathBuf, globals: Globals, trace: bool) -> Self {
        Self {
            base_dir,
            globals,
            cache: RefCell::new(HashMap::new()),
            trace,
        }
    }

    /// Resolve a load path to an absolute filesystem path.
    ///
    /// Rules:
    ///   - Relative paths are resolved against base_dir
    ///   - ".star" extension is added if not present
    ///   - Absolute paths are used as-is
    fn resolve_path(&self, path: &str) -> PathBuf {
        let path = if path.ends_with(".star") {
            path.to_string()
        } else {
            format!("{}.star", path)
        };

        let p = Path::new(&path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }

    /// Load, evaluate, and freeze a single module.
    fn load_module(&self, resolved: &Path) -> anyhow::Result<FrozenModule> {
        let content = std::fs::read_to_string(resolved).map_err(|e| {
            anyhow::anyhow!(
                "Failed to read '{}': {}",
                resolved.display(),
                e
            )
        })?;

        if self.trace {
            eprintln!("[nrg] load: evaluating {}", resolved.display());
        }

        let filename = resolved.to_string_lossy().to_string();
        let ast = AstModule::parse(&filename, content, &Dialect::Extended).map_err(|e| {
            anyhow::anyhow!("Parse error in {}:\n{}", resolved.display(), e)
        })?;

        let module = Module::new();
        {
            let mut eval = Evaluator::new(&module);

            // Set up a nested loader so loaded files can themselves load() other files.
            // The nested loader shares the same base_dir as the parent — all loads
            // resolve relative to the project root, not the importing file.
            // This matches Bazel/Buck semantics and avoids confusing relative chains.
            eval.set_loader(self);

            eval.eval_module(ast, &self.globals).map_err(|e| {
                anyhow::anyhow!("Error evaluating {}:\n{}", resolved.display(), e)
            })?;
        }

        // Freeze the module so its values can be imported into the parent scope.
        // Evaluator is dropped above so the borrow on `module` is released.
        module.freeze().map_err(|e| {
            anyhow::anyhow!("Failed to freeze module {}: {:?}", resolved.display(), e)
        })
    }
}

impl FileLoader for NrgFileLoader {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        let resolved = self.resolve_path(path);

        // Check cache first.
        if let Some(frozen) = self.cache.borrow().get(&resolved) {
            if self.trace {
                eprintln!("[nrg] load: cache hit for {}", resolved.display());
            }
            return Ok(frozen.dupe());
        }

        // Load, evaluate, freeze.
        let frozen = self.load_module(&resolved).map_err(starlark::Error::new_other)?;

        // Cache for future loads.
        self.cache.borrow_mut().insert(resolved, frozen.dupe());

        Ok(frozen)
    }
}
