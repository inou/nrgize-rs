# Product Requirements Document: Scotty — SSH Task Runner

**Version:** 1.1
**Date:** 2026-04-02
**Inspired by:** [spatie/scotty](https://github.com/spatie/scotty) v1.2.9
**Target implementation:** Rust

---

## 1. Executive Summary

Scotty is a CLI tool for defining and executing shell tasks on local and remote servers over SSH. It supports two task file formats: plain Bash with comment-based annotations (simple, zero-learning-curve) and Starlark — a Python-like configuration language — for users who need variables, conditionals, loops, and programmatic task composition. Scotty handles parsing, SSH connectivity, parallel execution, lifecycle hooks, and rich terminal output.

Inspired by Laravel Envoy and spatie/scotty, this is a ground-up Rust implementation with zero PHP dependency, significantly faster startup, no runtime dependencies, and a single static binary.

---

## 2. Problem Statement

Deployment and server management scripts are typically ad-hoc bash scripts with no structure, no parallelism, no lifecycle hooks, and poor error handling. Existing tools like Ansible are heavy; Laravel Envoy requires PHP on the developer machine. There is a gap for a lightweight, beautiful, dependency-free task runner that operates purely over SSH.

---

## 3. User Personas

**P1 — Full-stack Developer:** Deploys Laravel/Node/Rails apps to 1-3 servers. Wants a single file that defines deployment steps, runs them with one command, and shows clear success/failure.

**P2 — DevOps Engineer:** Manages fleets of 5-50 servers. Needs parallel execution, macro composition, lifecycle hooks, and the ability to integrate Scotty into CI/CD pipelines (non-interactive, exit-code-based).

**P3 — Power User / Migrator:** May be coming from Laravel Envoy or other task runners. Wants a structured configuration language with variables, conditionals, and loops — without needing PHP. Values Python-like syntax and deterministic execution.

---

## 4. Functional Requirements

### 4.1 Task File Formats

#### 4.1.1 Bash Format (`Scotty.sh`)

The primary format. Plain Bash with comment-based annotations.

**Server declarations:**
```bash
# @servers local=127.0.0.1 staging=user@staging.example.com production=deploy@prod.example.com
```

**Task definitions:**
```bash
# @task on:production emoji:rocket confirm="Deploy to production?"
deploy() {
    cd /var/www/app
    git pull origin main
    composer install --no-dev
    php artisan migrate --force
}
```

Task annotation options: `on:<servers>` (comma-separated), `parallel`, `confirm="<message>"`, `emoji:<emoji>`.

**Macro definitions (single-line and multi-line):**
```bash
# @macro deploy pull install migrate cache

# @macro full-deploy
#   pull
#   install
#   migrate
#   cache
# @endmacro
```

**Lifecycle hooks:**
```bash
# @before
notify_start() {
    echo "Deployment starting..."
}

# @after
notify_end() {
    echo "Deployment finished."
}

# @error
notify_error() {
    echo "Deployment FAILED."
}

# @success
celebrate() {
    echo "All tasks succeeded!"
}

# @finished
cleanup() {
    echo "Runs regardless of outcome."
}
```

**Variables and helper functions:**
Top-level `UPPERCASE=value` assignments before the first function are extracted as a variable preamble and injected into every task. Non-annotated functions are treated as helper functions and also injected.

CLI variables passed via `--var key=value` are converted to uppercase and injected.

#### 4.1.2 Starlark Format (`Scotty.star`)

The programmable format. Uses [Starlark](https://github.com/bazelbuild/starlark) — a deterministic, Python-like configuration language designed by Google for Bazel and adopted by Meta for Buck2. Starlark is hermetic (no filesystem/network access) and deterministic (same input always produces the same output), making it ideal for reproducible deployment definitions.

**Server declarations:**
```python
servers(
    local = "127.0.0.1",
    staging = "user@staging.example.com",
    production = "deploy@prod.example.com",
)
```

**Task definitions:**
```python
task(
    name = "deploy",
    on = ["production"],
    confirm = "Deploy to production?",
    emoji = "🚀",
    script = """
        cd /var/www/app
        git pull origin main
        composer install --no-dev
        php artisan migrate --force
    """,
)
```

**Multiple hosts per server:**
```python
servers(
    web = ["web1.example.com", "web2.example.com", "web3.example.com"],
    db = "db.example.com",
)
```

**Macro definitions:**
```python
macro(
    name = "full-deploy",
    tasks = ["pull", "install", "migrate", "cache"],
)
```

**Lifecycle hooks:**
```python
before(script = "echo 'Deployment starting...'")
after(script = "echo 'Deployment finished.'")
error(script = "echo 'Deployment FAILED.'")
success(script = "echo 'All tasks succeeded!'")
finished(script = "echo 'Runs regardless of outcome.'")
```

**Variables and conditionals:**
```python
# Variables — accessible in task scripts via shell interpolation
APP_ENV = "production"
BRANCH = var("branch", default = "main")  # CLI --var override with default

task(
    name = "deploy",
    on = ["production"],
    script = """
        cd /var/www/app
        git pull origin {branch}
    """.format(branch = BRANCH),
)

# Conditional task generation
if APP_ENV == "production":
    task(
        name = "cache-warm",
        on = ["production"],
        script = "php artisan cache:warm",
    )
```

**Loading other files:**
```python
load("common.star", "shared_servers", "shared_vars")
```

**Built-in functions provided by Scotty:**
- `servers(**kwargs)` — declare named servers (string for single host, list for multiple)
- `task(name, on, script, parallel=False, confirm=None, emoji=None)` — define a task
- `macro(name, tasks)` — define a macro (ordered list of task names)
- `before(script)`, `after(script)`, `error(script)`, `success(script)`, `finished(script)` — lifecycle hooks
- `var(name, default=None)` — reference a CLI variable with optional default
- `load(file, *symbols)` — import symbols from another `.star` file

### 4.2 File Resolution

When no explicit path is given, search the current directory in this order:

1. `Scotty.star`
2. `scotty.star`
3. `Scotty.sh`
4. `scotty.sh`

Overrides: `--path=<absolute>` or `--conf=<filename>`.

Parser selection: `.star` extension → Starlark parser; `.sh` extension → Bash parser.

### 4.3 CLI Commands

#### 4.3.1 `run <target> [options]`

Execute a task or macro.

**Arguments:** `target` — task name or macro name.

**Options:**

| Flag | Description |
|------|-------------|
| `--continue` | Don't stop on first task failure |
| `--pretend` | Dry-run: print SSH commands without executing |
| `--path=<path>` | Explicit file path |
| `--conf=<name>` | Filename in CWD |
| `--summary` | Hide real-time output, show only result table |
| `--var key=value` | Pass variables (repeatable) |

**Behavior:**

1. Resolve and parse the task file.
2. Resolve target to task list (single task or macro expansion).
3. If task has `confirm`, prompt the user.
4. For each task:
   a. Run all `@before` hooks (locally).
   b. Build SSH commands per server/host.
   c. Execute sequentially or in parallel per `parallel` flag.
   d. Run `@after` hooks on success, `@error` hooks on failure.
   e. Break on error unless `--continue`.
5. If all succeeded, run `@success` hooks.
6. Run `@finished` hooks unconditionally.
7. Return exit code 0 on success, non-zero on failure.

**Real-time output requirements:**

- Spinner animation (braille frames: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`).
- Server names color-coded (rotating: yellow, cyan, magenta, blue, green).
- Elapsed time shown per task.
- Pause/resume with `p` key.
- Ctrl+C graceful shutdown (kill child processes, restore terminal).
- Command trace filtering (strip `SCOTTY_TRACE:` lines, SSH warnings, ANSI codes).

**Result summary table:** task name, status (checkmark/cross), duration, failed host if any.

#### 4.3.2 `tasks [options]`

List available tasks and macros from the resolved file.

**Output:** macros (green) with their task sequence, tasks with server assignments and parallel flag, emojis if present.

#### 4.3.3 `ssh [name] [options]`

Open an interactive SSH session to a server defined in the task file.

- If `name` omitted, prompt for server selection.
- Filter out local servers.
- For servers with multiple hosts, prompt for host selection.
- Execute `ssh <host>` interactively.

#### 4.3.4 `init`

Scaffold a new task file.

- Prompt for format (Bash or Starlark).
- Prompt for server host.
- Check if file already exists (abort if so).
- Write template file (`Scotty.sh` or `Scotty.star`).

#### 4.3.5 `doctor [options]`

Validate configuration and connectivity.

**Checks:**

1. Task file exists and is parseable.
2. Servers are defined.
3. Tasks are defined.
4. Macro references resolve (no broken task references).
5. SSH connectivity to each remote server (5s timeout).
6. Remote tool availability: node, npm, git (10s timeout per server). Configurable tool list.

**Output:** checkmark/cross per check, versions for tools, connection timing.

### 4.4 SSH Execution

#### 4.4.1 Remote Execution

Commands are sent via heredoc over SSH:

```bash
ssh <host> 'bash -se' << \EOF-SCOTTY
export VAR="value"
set -e
<script>
EOF-SCOTTY
```

- `set -e` for strict error handling.
- Environment variables exported at top.
- `SCOTTY_HOST` auto-set to current host.
- Debug trap: `trap 'echo "SCOTTY_TRACE:$BASH_COMMAND" >&2' DEBUG` for command tracing.
- No timeout (tasks may run for extended periods).

#### 4.4.2 Local Execution

For servers with host `127.0.0.1`, `localhost`, or `local`: execute directly via local shell process, no SSH.

#### 4.4.3 SSH Config Resolution

Parse `~/.ssh/config` to resolve host aliases. Support `Host`, `HostName`, `User` directives. Handle `key=value` and space-separated formats, quoted values.

### 4.5 Server Definitions

- Single host: `name=user@host` (Bash) or `name = "user@host"` (Starlark)
- Multiple hosts per server: `name = ["host1", "host2"]` (Starlark) — runs task on each host. In Bash format, declare multiple comma-separated hosts: `# @servers web=host1,host2`.
- Local detection: hosts matching `127.0.0.1`, `localhost`, or `local`.

### 4.6 Parallel vs Sequential Execution

**Sequential (default):** hosts processed one at a time. Failure stops the chain unless `--continue`.

**Parallel:** all hosts start simultaneously. Output interleaved. Process waits for all to complete. Exit code is the sum of all individual exit codes (non-zero if any failed).

Polling interval: 80ms for output gathering during execution.

### 4.7 Hook System

Five lifecycle hook types:

| Hook | When | Scope |
|------|------|-------|
| `before` | Before each task | Per-task |
| `after` | After each task (on success) | Per-task |
| `error` | After a task fails | Per-task |
| `success` | After ALL tasks succeed | Global |
| `finished` | Always, after everything | Global |

All hooks execute locally (not over SSH). Multiple handlers per hook type are supported, executed in order.

---

## 5. Non-Functional Requirements

### 5.1 Performance

- CLI startup: < 10ms (advantage of Rust over PHP/Laravel Zero).
- File parsing: < 5ms for typical files.
- SSH connection overhead: determined by SSH, not the tool.
- Output polling: 80ms intervals during task execution.

### 5.2 Compatibility

- Two supported formats: Bash (`.sh`) and Starlark (`.star`). No PHP dependency.
- Bash format is simple and does not require any runtime beyond Bash on the remote.
- Starlark format provides Python-like programmability with deterministic execution.
- SSH config parsing compatible with OpenSSH format.
- Migration guide provided for users coming from Laravel Envoy.

### 5.3 Distribution

- Single static binary (no runtime dependencies).
- Cross-platform: macOS (arm64, x86_64), Linux (x86_64, arm64), Windows (x86_64).
- Install via: direct download, Homebrew, Cargo.

### 5.4 Error Handling

- Parse errors: clear message with line number and file path.
- SSH errors: surface connection failures with host and exit code.
- Missing targets: list available tasks/macros in error message.
- Signal handling: Ctrl+C kills child processes and restores terminal state.

---

## 6. Data Model

```
ParseResult
  ├── servers: Map<String, ServerDefinition>
  │     └── ServerDefinition { name, hosts: Vec<String> }
  ├── tasks: Map<String, TaskDefinition>
  │     └── TaskDefinition { name, script, servers: Vec<String>, parallel, confirm, emoji }
  ├── macros: Map<String, MacroDefinition>
  │     └── MacroDefinition { name, tasks: Vec<String> }
  ├── hooks: Vec<HookDefinition>
  │     └── HookDefinition { hook_type: HookType, script }
  └── variable_preamble: String

HookType: Before | After | Success | Error | Finished

TaskResult { exit_code, outputs: Map<String, String>, duration_secs, failed_host }
```

---

## 7. Out of Scope (v1)

- Notification integrations (Slack, Discord, etc.) — the original has `NotificationDefinition` but it's unused.
- Plugin system.
- Built-in secret management.
- Config file for global defaults (`~/.scottyrc`).
- Web UI or dashboard.

---

## 8. Success Metrics

- Both Bash and Starlark formats fully functional with feature parity.
- Zero PHP or external runtime dependencies.
- Binary size < 10MB.
- Single static binary — no runtime dependencies.
- CLI startup < 10ms.
- Test coverage > 90% for parsing and execution logic.
- Envoy migration guide covers common patterns.
