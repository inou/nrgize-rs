# Rust Implementation Plan: Scotty SSH Task Runner

**Based on:** scotty-prd.md
**Date:** 2026-04-02

---

## 1. Technology Stack

| Concern | Crate | Why |
|---------|-------|-----|
| CLI framework | `clap` (derive) | Industry standard, subcommand support, arg parsing, shell completions |
| Terminal UI | `ratatui` + `crossterm` | Spinner, colors, raw-mode key capture (pause/resume), cursor control |
| Async runtime | `tokio` | Parallel process spawning, timeouts, signal handling |
| Process execution | `tokio::process::Command` | Async child process management with stdout/stderr streaming |
| Regex | `regex` | Bash format parsing (comment annotations, brace matching) |
| Starlark parsing | `starlark` (Meta) | Python-like config language; deterministic, hermetic, production-proven (Bazel/Buck2) |
| SSH config | Custom parser | Small enough to own; avoids pulling in a full SSH library |
| Testing | Built-in `#[cfg(test)]` + `assert_cmd` + `predicates` | Unit tests + CLI integration tests |
| Error handling | `thiserror` + `miette` | Structured errors with source spans (line numbers in parse errors) |
| Serialization | `serde` (optional) | Only if we add config file support later |
| Build / release | `cargo-dist` | Cross-platform binary distribution, Homebrew formula generation |

---

## 2. Project Structure

```
scotty-rs/
├── Cargo.toml
├── src/
│   ├── main.rs                  # Entry point, clap dispatch
│   ├── cli/
│   │   ├── mod.rs               # Clap derive structs
│   │   ├── run.rs               # `run` command handler
│   │   ├── tasks.rs             # `tasks` command handler
│   │   ├── ssh.rs               # `ssh` command handler
│   │   ├── init.rs              # `init` command handler
│   │   ├── doctor.rs            # `doctor` command handler
│   │   └── ui.rs                # Spinner, colors, result table, pause/resume
│   ├── parsing/
│   │   ├── mod.rs               # ParserTrait, file resolution, parser dispatch
│   │   ├── bash_parser.rs       # Bash format parser
│   │   ├── starlark_parser.rs   # Starlark format parser (DSL functions + evaluation)
│   │   └── models.rs            # ParseResult, TaskDefinition, ServerDefinition, etc.
│   ├── execution/
│   │   ├── mod.rs               # Re-exports
│   │   ├── executor.rs          # Orchestrator: hooks → tasks → hooks
│   │   ├── task_runner.rs       # Per-task runner (sequential/parallel)
│   │   ├── ssh_command.rs       # SSH command builder (heredoc format)
│   │   └── task_result.rs       # TaskResult struct
│   └── ssh/
│       ├── mod.rs
│       └── config.rs            # ~/.ssh/config parser
├── tests/
│   ├── fixtures/                # .sh and .star test files
│   ├── parsing/                 # Parser unit tests
│   ├── execution/               # Execution integration tests
│   └── cli/                     # End-to-end CLI tests
└── docs/
```

---

## 3. Implementation Phases

### Phase 1: Core Data Model + Bash Parser (Week 1)

**Goal:** Parse `Scotty.sh` files into a fully populated `ParseResult`.

**Files:** `src/parsing/models.rs`, `src/parsing/bash_parser.rs`, `src/parsing/mod.rs`

**Tasks:**

1. **Define data model** (`models.rs`)
   - `ServerDefinition { name: String, hosts: Vec<String> }` with `is_local()` method
   - `TaskDefinition { name: String, script: String, servers: Vec<String>, parallel: bool, confirm: Option<String>, emoji: Option<String> }` with `display_name()` / `display_name_with_emoji()`
   - `MacroDefinition { name: String, tasks: Vec<String> }`
   - `HookDefinition { hook_type: HookType, script: String }`
   - `HookType` enum: `Before`, `After`, `Success`, `Error`, `Finished`
   - `ParseResult { servers, tasks, macros, hooks, variable_preamble }` with `resolve_tasks_for_target()`, `get_hooks()`, `available_targets()`
   - `TaskResult { exit_code: i32, outputs: IndexMap<String, String>, duration: Duration, failed_host: Option<String> }` with `succeeded()`

2. **Implement Bash parser** (`bash_parser.rs`)
   - `parse_servers()` — regex: `^#\s*@servers\s+(.+)$`
   - `parse_tasks()` — regex: `^#\s*@task\s+(.+)$\n(\w+)\(\)\s*\{` + brace-balanced body extraction
   - `parse_macros()` — single-line and multi-line variants
   - `parse_hooks()` — iterate `HookType` variants, match pattern per type
   - `parse_variables()` — extract top-level `UPPERCASE=value` assignments
   - `extract_helper_functions()` — non-annotated function definitions
   - `extract_function_body()` — brace-balanced extraction with quote-state tracking
   - `dedent()` — normalize indentation

3. **File resolution** (`mod.rs`)
   - `resolve_file(path: Option<&str>, conf: Option<&str>) -> Result<PathBuf>`
   - Search order: `Scotty.star`, `scotty.star`, `Scotty.sh`, `scotty.sh`
   - `select_parser(path: &Path) -> Box<dyn Parser>` — `.star` → StarlarkParser, `.sh` → BashParser

4. **Unit tests** — comprehensive tests for Bash parsing: server declarations, task annotations, macros (single-line and multi-line), hooks, variable extraction, helper functions, brace-balanced body extraction, edge cases

**Deliverable:** `cargo test` passes for all Bash parsing scenarios.

---

### Phase 2: SSH Execution Engine (Week 2)

**Goal:** Execute parsed tasks on local and remote servers.

**Files:** `src/execution/`, `src/ssh/config.rs`

**Tasks:**

1. **SSH config parser** (`ssh/config.rs`)
   - Parse `~/.ssh/config` — group by `Host` blocks
   - `find_configured_host(host: &str) -> Option<String>` — resolve aliases
   - Handle `Host`, `HostName`, `User` directives
   - Support `key=value` and whitespace-separated formats, quoted values
   - Skip `Match` blocks

2. **SSH command builder** (`execution/ssh_command.rs`)
   - `build_command(host: &str, script: &str, env: &HashMap<String, String>) -> String`
   - Local detection: `["127.0.0.1", "localhost", "local"]`
   - Remote format:
     ```
     ssh {resolved_host} 'bash -se' << \EOF-SCOTTY
     export KEY="value"
     set -e
     {script}
     EOF-SCOTTY
     ```
   - `build_process(...)` → `tokio::process::Command` with piped stdout/stderr
   - Auto-set `SCOTTY_HOST` env var
   - Debug trap injection: `trap 'echo "SCOTTY_TRACE:$BASH_COMMAND" >&2' DEBUG`

3. **Task runner** (`execution/task_runner.rs`)
   - `run(task, config, env, on_output, on_tick) -> TaskResult`
   - Resolve server map: server name → host(s)
   - Build `Command` per host
   - Sequential path: spawn one at a time, poll stdout/stderr every 80ms
   - Parallel path: spawn all, poll all every 80ms via `tokio::select!` or polling loop
   - Accumulate outputs per host, track exit codes
   - Return `TaskResult` with cumulative exit code

4. **Executor** (`execution/executor.rs`)
   - `run(target, config, env, continue_on_error, pretend, callbacks) -> IndexMap<String, TaskResult>`
   - Resolve target → task list (macro expansion or single task)
   - Prepend variable preamble + env vars to each task script
   - Execute lifecycle: before → task → after/error → success → finished
   - Hook execution: local shell process, no timeout
   - Pretend mode: build and print commands without executing

5. **Tests** — SSH command builder (local vs remote, env injection, heredoc format), SSH config parser (aliases, HostName/User resolution, quoted values), TaskResult (exit codes, success/failure)

**Deliverable:** Can execute `scotty run deploy` against a real server (manual test) and programmatic integration tests pass.

---

### Phase 3: CLI Commands + Terminal UI (Week 3)

**Goal:** Full CLI with rich terminal output.

**Files:** `src/cli/`, `src/main.rs`

**Tasks:**

1. **Clap CLI definition** (`cli/mod.rs`)
   ```rust
   #[derive(Parser)]
   #[command(name = "scotty", about = "A beautiful SSH task runner")]
   enum Cli {
       Run(RunArgs),
       Tasks(TasksArgs),
       Ssh(SshArgs),
       Init(InitArgs),
       Doctor(DoctorArgs),
   }
   ```

2. **Terminal UI module** (`cli/ui.rs`)
   - `Spinner` struct with braille frames `['⠋','⠙','⠹','⠸','⠼','⠴','⠦','⠧','⠇','⠏']`
   - Color rotation for server names: yellow, cyan, magenta, blue, green
   - `render_task_header(task, index, total)` — task name with emoji, index/total counter
   - `render_output_line(server_name, line, color)` — prefixed, colored output
   - `render_result_table(results)` — summary table with checkmark/cross, duration
   - `render_banner()` — ASCII art or styled app name + version
   - Pause/resume: raw-mode key listener on separate tokio task, toggle flag
   - Signal handling: `tokio::signal::ctrl_c()` → kill child processes, restore terminal
   - Output filtering: strip `SCOTTY_TRACE:` lines, SSH warnings, ANSI escape sequences

3. **`run` command** (`cli/run.rs`)
   - Wire up: resolve file → parse → resolve target → confirm → execute with UI callbacks
   - Handle `--pretend`, `--continue`, `--summary`, `--var`
   - Exit with appropriate code

4. **`tasks` command** (`cli/tasks.rs`)
   - Resolve file → parse → list macros and tasks with formatting

5. **`ssh` command** (`cli/ssh.rs`)
   - Resolve file → parse → prompt for server → `std::process::Command::new("ssh").arg(host).exec()` (replace process)

6. **`init` command** (`cli/init.rs`)
   - Prompt for format and host → write template file
   - Templates embedded as `include_str!()`

7. **`doctor` command** (`cli/doctor.rs`)
   - 7 checks: file exists, parses, has servers, has tasks, macros resolve, SSH connectivity (5s timeout), remote tools (10s timeout)
   - Async connectivity checks (parallel across servers)

8. **Integration tests** — end-to-end CLI tests for `run`, `tasks`, `init`, `ssh`, `doctor` commands using `assert_cmd` + fixture files

**Deliverable:** All 5 CLI commands functional with rich terminal output.

---

### Phase 4: Starlark Parser (Week 4 — estimated 2-3 days)

**Goal:** Parse `Scotty.star` files using the `starlark` crate, providing a Python-like programmable task definition format.

**Files:** `src/parsing/starlark_parser.rs`

**Approach:** Use Meta's `starlark` crate to evaluate `.star` files. We register custom built-in functions (`servers()`, `task()`, `macro()`, hooks) that populate a `ParseResult` during evaluation. Starlark handles all the heavy lifting — parsing, variable scoping, conditionals, loops, `load()` imports — we just define the DSL.

**Tasks:**

1. **Define Starlark globals** (`starlark_parser.rs`)
   - Register built-in functions in the Starlark environment:
     - `servers(**kwargs)` — each kwarg is a server name; value is a string (single host) or list of strings (multi-host). Populates `ParseResult.servers`.
     - `task(name, on, script, parallel=False, confirm=None, emoji=None)` — defines a task. Populates `ParseResult.tasks`.
     - `macro(name, tasks)` — defines a macro. `tasks` is a list of task name strings. Populates `ParseResult.macros`.
     - `before(script)`, `after(script)`, `error(script)`, `success(script)`, `finished(script)` — lifecycle hooks. Populates `ParseResult.hooks`.
     - `var(name, default=None)` — looks up CLI `--var` values, falls back to default. Returns a string for use in Starlark expressions.
   - Use `starlark::environment::GlobalsBuilder` to register these functions.
   - Use a `starlark::values::Value` wrapper or a Rust `RefCell` to accumulate `ParseResult` state during evaluation.

2. **Implement the parser entry point**
   - `parse(path: &Path, cli_vars: &HashMap<String, String>) -> Result<ParseResult>`
   - Read the `.star` file
   - Create a Starlark `Module` with CLI vars injected
   - Evaluate the file — the registered functions build up the `ParseResult` as side effects
   - Return the accumulated `ParseResult`
   - Handle `load()` statements via `starlark::eval::FileLoader` trait — resolve relative paths, evaluate imported files

3. **Error handling**
   - Map Starlark evaluation errors to `miette` diagnostics with file path + line number
   - Validate after evaluation: ensure at least one server and one task are defined
   - Clear error messages for common mistakes (e.g., `task()` without `on` servers, referencing undefined server names)

4. **Init template** — create a `Scotty.star` template for the `init` command:
   ```python
   servers(
       local = "127.0.0.1",
       production = "user@example.com",
   )

   task(
       name = "deploy",
       on = ["production"],
       script = """
           echo "Hello from Scotty!"
       """,
   )
   ```

5. **Tests**
   - Basic: servers, single task, macro, hooks
   - Variables: `var()` with defaults, CLI overrides, string formatting in scripts
   - Conditionals: `if`/`else` generating different tasks based on variables
   - Loops: `for` generating tasks dynamically (e.g., per-server config)
   - Multi-host servers: list values in `servers()`
   - `load()`: importing shared definitions from another `.star` file
   - Error cases: missing required args, undefined servers, syntax errors with line numbers

**Deliverable:** `Scotty.star` files parse into identical `ParseResult` structures as equivalent `Scotty.sh` files. Full Starlark language features (variables, conditionals, loops, imports) work.

**Why this is simpler than the old Blade approach:** The `starlark` crate handles all parsing, evaluation, scoping, and control flow. We don't build a compiler, evaluator, or template engine — we just define ~6 built-in functions and let Starlark do the rest. This is 2-3 days of work instead of 5-7.

---

### Phase 5: Polish, Testing, Distribution (Week 4-5)

**Goal:** Production-ready release.

**Tasks:**

1. **Error messages** — every parse error includes file path + line number via `miette` source spans; map Starlark errors to miette diagnostics
2. **Shell completions** — `clap_complete` for bash, zsh, fish
3. **Man page** — `clap_mangen`
4. **Cross-compilation** — CI matrix: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`
5. **Release pipeline** — GitHub Actions with `cargo-dist`, Homebrew tap
6. **README** — installation, usage, format comparison (Bash vs Starlark), migration guide from Envoy
7. **Benchmark** — startup time comparison vs PHP Scotty
8. **Final test pass** — all unit, integration, and CLI tests green across platforms

---

## 4. Key Design Decisions

### 4.1 Programmable Format: Starlark

**Decision:** Use Starlark (via Meta's `starlark` crate) as the programmable task definition format, replacing Blade/PHP entirely.

**Rationale:**
- Python-like syntax familiar to DevOps/SRE users
- Deterministic and hermetic by design — same input always produces the same output, no side effects
- Production-proven at scale (Google Bazel, Meta Buck2)
- The `starlark` crate handles all parsing, evaluation, and scoping — we only define DSL functions
- No PHP dependency, no custom template engine, no expression evaluator to build and maintain
- `load()` provides file imports natively
- Estimated 2-3 days of integration work vs 5-7 days for a Blade parser

**Trade-offs accepted:**
- No backward compatibility with Laravel Envoy `.blade.php` files (migration guide provided)
- Starlark is hermetic — task files cannot read environment variables or files at parse time (they can only use `var()` for CLI-injected values). This is a feature for reproducibility but may surprise users expecting shell-like behavior.
- Pre-1.0 crate (0.13) — API may change between minor versions. Pin the version.

### 4.2 Async vs Sync

Use `tokio` for the execution engine (parallel process management, timeouts, signal handling) but keep parsing synchronous — it's fast enough and simpler.

### 4.3 Output Architecture

The `run` command needs real-time streaming output with UI updates. Architecture:

```
[tokio task: process poller]  →  mpsc channel  →  [main task: UI renderer]
[tokio task: key listener]    →  mpsc channel  →  [main task: pause/spinner]
[tokio task: signal handler]  →  oneshot       →  [main task: cleanup]
```

This keeps the UI responsive while tasks execute.

### 4.4 IndexMap for Ordering

Use `indexmap::IndexMap` instead of `HashMap` for tasks, macros, and servers to preserve definition order (important for `tasks` listing and macro execution order).

---

## 5. Crate Dependencies (Cargo.toml)

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
crossterm = "0.28"
ratatui = "0.29"
regex = "1"
indexmap = "2"
thiserror = "2"
miette = { version = "7", features = ["fancy"] }
starlark = "0.13"             # Python-like config language (Starlark parser/evaluator)
dirs = "5"                    # Home directory resolution
dialoguer = "0.11"            # Interactive prompts (init, ssh, confirm)

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
tempfile = "3"
```

---

## 6. Risk Register

| Risk | Impact | Mitigation |
|------|--------|------------|
| `starlark` crate is pre-1.0 (0.13) | API breakage on minor version bumps | Pin exact version in Cargo.toml; monitor releases |
| Starlark hermetic model surprises users | Can't read env vars or files at parse time | Document clearly; `var()` function covers the CLI-injection use case |
| Envoy users expect backward compatibility | Can't parse `.blade.php` files | Provide migration guide with common pattern translations |
| SSH key passphrase prompts | Process hangs waiting for TTY input | Use `ssh -o BatchMode=yes` for doctor checks; let normal runs inherit TTY |
| Windows SSH support | Windows doesn't have `ssh` by default | Document requirement for OpenSSH (built into Windows 10+); test on CI |
| Large output buffering | Memory pressure on high-output tasks | Stream output, don't buffer entire output; cap stored output per host |
| Terminal state corruption on crash | Cursor hidden, raw mode stuck | Register panic hook to restore terminal via `crossterm::terminal::disable_raw_mode()` |

---

## 7. Estimated Effort

| Phase | Effort | Cumulative |
|-------|--------|------------|
| Phase 1: Data model + Bash parser | 5 days | 5 days |
| Phase 2: SSH execution engine | 5 days | 10 days |
| Phase 3: CLI + Terminal UI | 5 days | 15 days |
| Phase 4: Starlark parser | 2-3 days | 17-18 days |
| Phase 5: Polish + distribution | 3-5 days | 20-23 days |
| **Total** | **~4 weeks** | |

Replacing Blade with Starlark saved ~1 week and eliminated the riskiest phase. The `starlark` crate handles parsing/evaluation — we only define DSL functions.
