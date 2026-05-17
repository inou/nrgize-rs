# Energize — 5 New Features Implementation Plan

## Overview

Five features, ordered by implementation dependency (each builds on prior work):

1. **Local task execution** (no SSH) — foundation for features 4 & 5
2. **Env file loading** — needed for secrets workflow
3. **Secret encryption** — builds on env file loading
4. **File copying to remote** — new DSL primitive
5. **Docker image transfer** — builds on local execution + file copy

---

## Feature 1: Local Task Execution

**Problem:** Currently every task must target a server. There's no way to run a build step, test, or pre-processing script on the local machine as part of a macro pipeline.

**Note:** `ssh_command.rs` already handles `127.0.0.1`/`localhost`/`local` by routing to `bash -se` instead of SSH. But the user has to define a fake "local" server. This feature makes local execution a first-class concept.

### DSL Changes (Starlark)

```python
# Option A: local=True flag on task() — preferred, minimal surface area
task(
    name = "build-assets",
    local = True,          # NEW — runs on the machine running nrg, no server needed
    script = "npm run build",
)

# When local=True, the `on` parameter is ignored/optional.
# This makes it clear in the config which tasks are local vs remote.
```

### Data Model Changes

**`models.rs` — `TaskDefinition`:**
```rust
pub struct TaskDefinition {
    pub name: String,
    pub script: String,
    pub servers: Vec<String>,
    pub parallel: bool,
    pub confirm: Option<String>,
    pub emoji: Option<String>,
    pub local: bool,           // NEW
}
```

### Execution Changes

**`task_runner.rs`:**
- When `task.local == true`, skip server resolution entirely
- Execute via `tokio::process::Command::new("bash").arg("-se")` with piped stdin (same as current local host path)
- Stream output with server_name = "local", host = "local"
- Reuse `execute_on_host_streaming()` by passing host="local"

**`executor.rs`:**
- Before server resolution, check `task.local` — if true, short-circuit to local execution

**`ssh_command.rs`:**
- No changes needed. `build_process()` with `host="local"` already routes to bash.

### Starlark Parser Changes

**`starlark_parser.rs` — `task()` function:**
- Add `#[starlark(require = named, default = false)] local: bool` parameter
- When `local=true` and `on` is not provided, set `servers` to empty vec
- Relax the validation: tasks with `local=true` don't need servers

### Validation Changes

- If `local=true` and `on` is provided, warn (or just ignore `on`)
- If `local=false` and `on` is empty, that's still an error

### Tests

- Parse a `local=True` task with no `on`
- Parse a `local=True` task with `on` (should still work, `on` ignored)
- Execute a local task that runs `echo hello` and verify output
- Macro containing mix of local and remote tasks
- Pretend mode with local tasks

### Files to modify
- `src/parsing/models.rs` — add `local` field
- `src/parsing/starlark_parser.rs` — add `local` param, relax validation
- `src/parsing/bash_parser.rs` — add `@local` annotation support
- `src/execution/task_runner.rs` — handle `task.local` path
- `src/execution/executor.rs` — short-circuit for local tasks
- `src/cli/run.rs` — pretend mode output for local tasks

---

## Feature 2: Env File Loading

**Problem:** Environment variables are currently only injectable via `--var key=value`. For real deployments you have `.env` files with dozens of variables. There's no way to load them, and no way to export them to the remote shell.

### Two Concerns

1. **Loading .env into the Starlark evaluation context** — so `var("db_pass")` can pull from `.env`
2. **Exporting .env variables as shell exports on the remote** — so the script has access to `$DATABASE_URL` etc.

### DSL Changes (Starlark)

```python
# Global env file loading — loaded at parse time, available to var() calls
env_file(".env.prod")                    # NEW — load from path relative to .star file
env_file("/absolute/path/.env.prod")     # absolute paths work too

# Per-task env file (loaded at execution time, exported on the remote)
task(
    name = "deploy",
    on = ["production"],
    env = ".env.prod",       # NEW — exports these vars into the remote shell
    script = "echo $DATABASE_URL",
)
```

### CLI Changes

```
nrg run deploy --env .env.prod      # CLI flag to load env file, overrides everything
```

**`run.rs` — `RunArgs`:**
```rust
#[arg(long)]
pub env_file: Option<String>,
```

### Env File Parser

**New file: `src/parsing/env_parser.rs`**

Simple `.env` parser. Rules:
- Lines starting with `#` are comments
- Blank lines are skipped
- Format: `KEY=VALUE` or `KEY="VALUE"` or `KEY='VALUE'`
- Quoted values: strip outer quotes, handle `\n` escape in double-quotes
- `export KEY=VALUE` prefix is tolerated (stripped)
- Returns `HashMap<String, String>`

This is ~60 lines of Rust, no external dep needed.

### Data Model Changes

**`models.rs` — `ParseResult`:**
```rust
pub struct ParseResult {
    // ... existing fields ...
    pub env_files: Vec<String>,    // NEW — global env files to load
}
```

**`models.rs` — `TaskDefinition`:**
```rust
pub struct TaskDefinition {
    // ... existing fields ...
    pub env_file: Option<String>,  // NEW — per-task env file
}
```

### Flow

1. **Parse time:** `env_file()` DSL call loads the file and injects key-value pairs into `__cli_vars__` dict (same mechanism as `--var`, so `var()` calls pick them up). CLI `--var` takes precedence over `.env` values.
2. **Execution time:** If task has `env_file`, load it and merge into the `env` HashMap passed to `ssh_command::build_script()`, which already does `export KEY="VALUE"` for every entry.
3. **CLI `--env`:** Loaded first, before Starlark evaluation. Merged into the vars HashMap with lower precedence than `--var`.

### Precedence (highest wins)

1. `--var key=value` (CLI)
2. Starlark assignments (`VAR = "value"`)
3. `env_file(".env")` in .star file
4. `--env .env` (CLI flag)

### Tests

- Parse `.env` file with comments, blank lines, quoted values, `export` prefix
- `var()` resolves from env file when no CLI var is set
- CLI `--var` overrides env file value
- Per-task env file exports show up in `build_script()` output
- Missing env file produces clear error
- Multiple `env_file()` calls (later ones override earlier for duplicate keys)

### Files to modify
- `src/parsing/env_parser.rs` — **NEW** — .env file parser
- `src/parsing/mod.rs` — export env_parser
- `src/parsing/starlark_parser.rs` — add `env_file()` DSL function
- `src/parsing/models.rs` — add `env_files` to ParseResult, `env_file` to TaskDefinition
- `src/execution/executor.rs` — load per-task env files, merge into env
- `src/execution/ssh_command.rs` — no changes (already exports env)
- `src/cli/run.rs` — add `--env` flag, load before parsing

---

## Feature 3: Secret Encryption

**Problem:** `.env` files contain plaintext secrets. Committing them to git is a security risk. Users need a way to encrypt sensitive values so they can safely version-control their deployment configs.

### Approach: `age` Encryption

[age](https://github.com/FiloSottile/age) is the right tool here. It's modern, simple, widely available, and has a Rust crate (`age`). But adding a Rust dep for this is heavy. Better approach: **shell out to `age` CLI** — it's a single binary, easy to install, and keeps nrg's binary small.

### How It Works

1. **Setup:** User generates a key pair:
   ```
   nrg secrets init          # generates .nrg-key (private) and .nrg-key.pub
   ```
   `.nrg-key` goes into `.gitignore`. `.nrg-key.pub` is committed.

2. **Encrypt a value:**
   ```
   nrg secrets encrypt "my-secret-value"
   # Output: ENC[age1...]
   ```

3. **Encrypt an entire .env file:**
   ```
   nrg secrets seal .env.prod
   # Creates .env.prod.enc (encrypted version)
   ```

4. **In the Starlark file:**
   ```python
   env_file(".env.prod.enc", encrypted = True)   # decrypted at parse time using .nrg-key
   ```

5. **At execution time:** Values are decrypted locally, then exported as plaintext to the remote shell (same as regular env vars). Secrets never touch disk on the remote in plaintext — they exist only in the shell environment of the running process.

### CLI Subcommand

```
nrg secrets init              # Generate age key pair
nrg secrets encrypt <value>   # Encrypt a single value, print ENC[...] token
nrg secrets decrypt <token>   # Decrypt a single ENC[...] token
nrg secrets seal <file>       # Encrypt entire .env file → .env.enc
nrg secrets unseal <file>     # Decrypt .env.enc → .env (for editing)
```

### Implementation Details

**New file: `src/cli/secrets.rs`**
- Subcommand handler for `nrg secrets`
- Shells out to `age` binary for encrypt/decrypt
- Key file discovery: look for `.nrg-key` walking up from CWD, then `~/.config/nrg/key`

**New file: `src/secrets/mod.rs`**
- `encrypt_value(plaintext, pubkey_path) -> String`
- `decrypt_value(ciphertext, key_path) -> String`
- `seal_file(env_path, pubkey_path) -> Result<PathBuf>`
- `unseal_file(enc_path, key_path) -> Result<PathBuf>`
- `find_key_file() -> Option<PathBuf>`
- `find_pubkey_file() -> Option<PathBuf>`

**Starlark integration:**
- `env_file(".env.enc", encrypted=True)` — before injecting into `__cli_vars__`, run each value through `decrypt_value()` if it matches `ENC[...]` pattern.

**Doctor check:**
- Add check: "age binary found" (only if encrypted env files are used)
- Add check: ".nrg-key exists" (only if encrypted env files are used)

### Encrypted .env File Format

The sealed file is just the entire `.env` file encrypted as one blob with `age`:
```
age-encryption.org/v1
-> X25519 ...
...
```

This is simpler than encrypting individual values and means `nrg secrets seal/unseal` is just `age -e`/`age -d`.

For inline `ENC[...]` tokens in regular `.env` files, each token is a base64-encoded age ciphertext that gets individually decrypted.

### Tests

- `nrg secrets init` creates key files
- Round-trip: encrypt value → decrypt value
- Round-trip: seal file → unseal file
- `env_file("x.enc", encrypted=True)` decrypts at parse time
- Missing `age` binary gives clear error
- Missing key file gives clear error
- Doctor detects missing age/key when encrypted env is used

### Files to modify/create
- `src/secrets/mod.rs` — **NEW** — encryption/decryption logic
- `src/secrets/` — **NEW** module
- `src/cli/secrets.rs` — **NEW** — CLI subcommand
- `src/cli/mod.rs` — register `secrets` subcommand
- `src/parsing/starlark_parser.rs` — `encrypted` param on `env_file()`
- `src/parsing/env_parser.rs` — decrypt `ENC[...]` tokens during parse
- `src/cli/doctor.rs` — add age/key checks

---

## Feature 4: File Copying to Remote

**Problem:** Deployments often need to upload files: config files, build artifacts, SSL certs, compiled assets. Currently the only way is to inline `scp` in a script, which is ugly and doesn't integrate with nrg's server resolution, SSH config, or output streaming.

### DSL Changes (Starlark)

```python
# New DSL function: upload()
upload(
    name = "push-config",
    src = "./nginx.conf",                           # local path (relative to .star file)
    dest = "/etc/nginx/sites-available/myapp",      # remote path
    on = ["production"],
    emoji = "📤",
)

# Upload a directory (recursive)
upload(
    name = "push-assets",
    src = "./dist/",                    # trailing slash = directory contents
    dest = "/var/www/app/public/",
    on = ["production"],
)

# Upload can appear in macros alongside regular tasks
define_macro(
    name = "deploy",
    tasks = ["build-assets", "push-assets", "restart"],
)
```

### Data Model Changes

We have a design choice: make `upload` a special kind of task, or a separate entity. Making it a task variant is cleaner because it slots into macros naturally.

**`models.rs` — `TaskDefinition`:**
```rust
pub struct TaskDefinition {
    // ... existing fields ...
    pub local: bool,
    pub env_file: Option<String>,
    pub upload: Option<UploadSpec>,    // NEW — if set, this is an upload task
}

#[derive(Debug, Clone, PartialEq)]
pub struct UploadSpec {
    pub src: String,
    pub dest: String,
}
```

When `upload` is `Some(...)`, the task runner uses `rsync` or `scp` instead of SSH script execution.

### Implementation

**`task_runner.rs` — new function `execute_upload()`:**
```rust
async fn execute_upload(
    server_name: &str,
    host: &str,
    upload: &UploadSpec,
    ssh_config: &SshConfig,
    on_output: Option<&OutputCallback>,
) -> (i32, String) {
    // Use rsync if available, fall back to scp
    // rsync -avz --progress -e ssh SRC user@host:DEST
}
```

- Resolve host via `ssh_config` (same as SSH execution)
- Use `rsync -az -e ssh` for the transfer (progress streamed to callback)
- Fall back to `scp -r` if rsync isn't available
- Local source path resolved relative to the `.star` file's directory

**`task_runner.rs` — `run_task()` modification:**
- Check `task.upload.is_some()` — if yes, call `execute_upload()` instead of `execute_on_host_streaming()`

**`starlark_parser.rs` — new DSL function:**
```rust
fn upload<'v>(
    #[starlark(require = named)] name: &str,
    #[starlark(require = named)] src: &str,
    #[starlark(require = named)] dest: &str,
    #[starlark(require = named)] on: Value<'v>,
    #[starlark(require = named, default = NoneType)] emoji: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<NoneType> {
    // Creates a TaskDefinition with upload = Some(UploadSpec { src, dest })
    // script is empty, servers come from `on`
}
```

### Pretend Mode

In pretend mode, print the rsync/scp command that would execute instead of the SSH command.

### Tests

- Parse `upload()` DSL call
- Upload task appears in task list and macros
- `execute_upload()` builds correct rsync command
- Pretend mode shows rsync command
- Missing source file gives clear error before execution
- Directory upload (trailing slash)

### Files to modify/create
- `src/parsing/models.rs` — add `UploadSpec`, add `upload` field to TaskDefinition
- `src/parsing/starlark_parser.rs` — add `upload()` DSL function
- `src/execution/task_runner.rs` — add `execute_upload()`, modify `run_task()`
- `src/cli/run.rs` — pretend mode for upload tasks

---

## Feature 5: Docker Image Transfer

**Problem:** For projects that use Docker but don't have a registry, you need to build locally, save the image, transfer it to the remote, and load it there. This is a multi-step workflow that's tedious to script manually.

### DSL Changes (Starlark)

```python
# New DSL function: docker_deploy()
docker_deploy(
    name = "ship-app",
    image = "myapp:latest",                    # local docker image tag
    build = "./Dockerfile",                     # optional — build before shipping
    build_context = ".",                         # optional — docker build context
    build_args = {"APP_ENV": "production"},      # optional — build args
    on = ["production"],
    emoji = "🐳",
)
```

### What It Does (Under the Hood)

`docker_deploy()` is syntactic sugar for a 4-step pipeline:

1. **Build** (optional): `docker build -t {image} -f {build} {build_context}`
2. **Save**: `docker save {image} | gzip > /tmp/nrg-docker-{hash}.tar.gz`
3. **Transfer**: rsync/scp the tarball to the remote (reuses Feature 4's upload logic)
4. **Load**: `ssh host 'docker load < /tmp/nrg-docker-{hash}.tar.gz && rm /tmp/nrg-docker-{hash}.tar.gz'`

### Data Model Changes

**`models.rs`:**
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct DockerDeploySpec {
    pub image: String,
    pub build_file: Option<String>,     // Dockerfile path
    pub build_context: Option<String>,  // build context directory
    pub build_args: HashMap<String, String>,
}

pub struct TaskDefinition {
    // ... existing fields ...
    pub docker_deploy: Option<DockerDeploySpec>,   // NEW
}
```

### Implementation

**`task_runner.rs` — new function `execute_docker_deploy()`:**

This is a local+remote hybrid task:
1. Steps 1-2 run locally (build and save)
2. Step 3 uses `execute_upload()` from Feature 4
3. Step 4 runs via SSH on the remote

All steps stream output through the callback.

**`starlark_parser.rs` — `docker_deploy()` DSL function:**
- Registers a TaskDefinition with `docker_deploy = Some(...)` and `local = false`
- The task runner detects `docker_deploy.is_some()` and runs the specialized pipeline

### Alternative: Expansion Into Tasks

Instead of a monolithic `docker_deploy()`, we could expand it into individual tasks at parse time:

```
docker_deploy("ship-app", ...)
→ generates tasks: ship-app:build, ship-app:save, ship-app:transfer, ship-app:load
→ generates macro: ship-app = [ship-app:build, ship-app:save, ship-app:transfer, ship-app:load]
```

**This approach is better** because:
- Each step is visible in `nrg tasks`
- Each step can be re-run individually
- Failure reporting is per-step
- No special execution path needed in task_runner

I recommend this expansion approach.

### Tests

- Parse `docker_deploy()` DSL call
- Expansion generates 3-4 tasks and a macro
- Build step is skipped if no `build` param
- Pretend mode shows all docker commands
- Missing Docker gives clear error

### Files to modify/create
- `src/parsing/models.rs` — add `DockerDeploySpec`
- `src/parsing/starlark_parser.rs` — add `docker_deploy()` DSL function with task expansion
- `src/execution/task_runner.rs` — minimal changes if using expansion approach
- `src/cli/doctor.rs` — add Docker check (only if docker_deploy is used)

---

## Implementation Order

```
Feature 1: Local Execution     ~2 hours    [no dependencies]
Feature 2: Env File Loading    ~2 hours    [no dependencies, but pairs well with 1]
Feature 3: Secret Encryption   ~3 hours    [depends on 2]
Feature 4: File Copy           ~2 hours    [depends on 1 for local/remote distinction]
Feature 5: Docker Transfer     ~3 hours    [depends on 1 + 4]
```

Total: ~12 hours of implementation + testing + docs.

## Documentation Updates

After each feature:
- Update `README.md` with new DSL functions, CLI flags, examples
- Update `Energize.star` example with new features where applicable
- Add section to README for `nrg secrets` subcommand (Feature 3)
- Add "File Operations" section to README (Feature 4)
- Add "Docker Deployment" section to README (Feature 5)

## Test Strategy

Each feature adds:
- Unit tests for new parsing (starlark_parser tests)
- Unit tests for new data model behavior
- Unit tests for new execution logic (where possible without real SSH)
- Integration test in the existing test suite pattern

Target: maintain the current 77+ test count, adding ~30-40 new tests across all features.
