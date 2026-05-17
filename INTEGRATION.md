# Runtime Primitives — Integration Guide

The new `src/runtime/` module and `src/cli/exec.rs` are self-contained. To wire them
into the existing codebase, make these changes to existing files:

---

## 1. `Cargo.toml` — Add dependency

Add `ureq` for blocking HTTP requests (used by `http_get` / `http_post`):

```toml
[dependencies]
# ... existing deps ...
ureq = "3"
```

`serde_json` is already present in the project. No other new dependencies needed.

---

## 2. `src/main.rs` — Add `mod runtime`

Add the module declaration alongside the existing ones:

```rust
mod cli;
mod execution;
mod parsing;
mod runtime;   // <-- ADD THIS
mod secrets;
mod ssh;
```

---

## 3. `src/cli/mod.rs` — Add `exec` subcommand

### 3a. Add the module declaration:

```rust
pub mod exec;   // <-- ADD THIS
```

### 3b. Add the Exec variant to the CLI enum (assuming clap derive):

```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing variants ...

    /// Evaluate a Starlark file in orchestration mode with runtime primitives.
    /// Unlike `run`, this executes the file as a script — top-level calls to
    /// ssh_exec(), local_exec(), etc. happen during evaluation.
    Exec {
        /// Path to the .star file to evaluate. Defaults to searching for Energize.star.
        #[arg(value_name = "FILE")]
        file: Option<String>,
    },
}
```

### 3c. Add the match arm in the command dispatch:

```rust
Commands::Exec { file } => {
    cli::exec::run_exec(file.as_deref())?;
}
```

---

## 4. That's it.

No changes needed to:
- `execution/ssh_command.rs` — the runtime module has its own SSH execution via `std::process::Command`
- `execution/executor.rs` — the runtime module has its own local execution
- `parsing/starlark_parser.rs` — the existing parser is untouched; exec mode has its own evaluator
- `secrets/mod.rs` — the runtime `secret()` function has its own lookup logic

The runtime module is fully independent. The existing `nrg run <task>` mode continues
to work exactly as before.

---

## Testing

```bash
# Build
cargo build

# Test with a simple script
cat > test.star << 'EOF'
result = local_exec("echo hello from starlark")
print(result.stdout)
print("exit code: " + str(result.exit_code))
print("ok: " + str(result.ok))

# Test env
tag = env_or("DEPLOY_TAG", "v1.0.0")
print("Deploy tag: " + tag)

# Test state
state_set("test_key", "test_value")
val = state_get("test_key")
print("State: " + val)

# Test http (will fail unless something is listening, but shows the function works)
# r = http_get("http://localhost:8080/health")
# print("HTTP status: " + str(r.status))

print("All runtime primitives working!")
EOF

cargo run -- exec test.star
```

## Example: Full deployment script

See `RUNTIME_PRIMITIVES_PLAN.md` for a complete Kamal-equivalent deployment
example using these primitives.
