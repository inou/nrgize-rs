# Energize Runtime Primitives — Implementation Plan

**Goal:** Transform Energize from a task-definition tool into a deployment toolkit where Starlark files can orchestrate arbitrary deployment workflows (including Kamal-equivalent flows) by calling side-effectful Rust primitives during evaluation.

**Non-goal:** Replicating Kamal's opinions. No built-in Docker model, no built-in proxy, no built-in role system. The user composes these from primitives in `.star` files.

---

## Architectural Shift

**Current model:** Starlark → parse → extract task definitions (models) → Rust task_runner decides order → executor runs commands

**New model:** Starlark → evaluate with built-in functions → Starlark code calls Rust primitives (SSH, HTTP, local exec) → primitives execute and return results → Starlark makes decisions based on results

The key change: Starlark evaluation becomes the execution engine. The `#[starlark_module]` built-in functions have **side effects** — they actually SSH into servers, run commands, make HTTP requests. Starlark code is the orchestrator, not a config format.

**Backward compatibility:** The existing "define tasks, `nrg run <taskname>`" mode continues to work unchanged. The new runtime primitives are additional globals available in the Starlark environment. A `.star` file can use both: define named tasks AND use runtime primitives at the top level or inside task bodies.

---

## Phase 1: Core Execution Primitives

### 1.1 `ExecResult` — Starlark-visible return type

A custom Starlark type that all execution functions return.

```rust
use starlark::values::{StarlarkValue, Value, Heap, ProvidesStaticType, Allocative, NoSerialize};
use starlark::starlark_simple_value;
use starlark_derive::starlark_value;
use std::fmt;

#[derive(Debug, Clone, ProvidesStaticType, Allocative, NoSerialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub host: Option<String>, // None for local execution
}

impl fmt::Display for ExecResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ExecResult(exit_code={}, stdout_len={})", self.exit_code, self.stdout.len())
    }
}

starlark_simple_value!(ExecResult);

#[starlark_value(type = "ExecResult")]
impl<'v> StarlarkValue<'v> for ExecResult {
    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        match attribute {
            "stdout" => Some(heap.alloc(self.stdout.as_str())),
            "stderr" => Some(heap.alloc(self.stderr.as_str())),
            "exit_code" => Some(heap.alloc(self.exit_code)),
            "host" => Some(match &self.host {
                Some(h) => heap.alloc(h.as_str()),
                None => Value::new_none(),
            }),
            "ok" => Some(heap.alloc(self.exit_code == 0)),
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        matches!(attribute, "stdout" | "stderr" | "exit_code" | "host" | "ok")
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["stdout".into(), "stderr".into(), "exit_code".into(), "host".into(), "ok".into()]
    }
}
```

**Attributes available in Starlark:**
- `result.stdout` — captured stdout as string
- `result.stderr` — captured stderr as string
- `result.exit_code` — integer exit code
- `result.host` — hostname or None for local
- `result.ok` — convenience bool, true when exit_code == 0

**File:** `src/runtime/types.rs` (new)

---

### 1.2 `ssh_exec(host, cmd)` — Remote command execution with return value

```rust
#[starlark_module]
fn runtime_builtins(builder: &mut GlobalsBuilder) {
    fn ssh_exec<'v>(
        host: &str,
        cmd: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        // 1. Resolve SSH config for host (reuse existing ssh::config module)
        // 2. Execute command, capturing stdout + stderr (refactor ssh_command.rs)
        // 3. Return ExecResult allocated on the Starlark heap
        let result = do_ssh_exec(host, cmd)?;
        Ok(heap.alloc(ExecResult {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            host: Some(host.to_string()),
        }))
    }
}
```

**Refactoring required in `execution/ssh_command.rs`:**
The current SSH execution likely streams output directly to the terminal. It needs a second mode that captures stdout/stderr into strings and returns them. Extract a `do_ssh_exec(host: &str, cmd: &str) -> Result<RawExecResult>` function that both the existing task runner and the new Starlark built-in can call.

**SSH connection pooling consideration:**
A Starlark script might call `ssh_exec("10.0.0.1", ...)` 20 times. Opening a new SSH connection each time is wasteful. Add a connection pool (keyed by host) to `ssh::config` that reuses connections within a single `nrg` invocation. The pool lives in a `thread_local!` or is passed through the Starlark `Evaluator`'s extra data.

---

### 1.3 `local_exec(cmd)` — Local command execution with return value

```rust
fn local_exec<'v>(
    cmd: &str,
    heap: &'v Heap,
) -> anyhow::Result<Value<'v>> {
    // Shell out via std::process::Command with sh -c
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()?;
    Ok(heap.alloc(ExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into(),
        stderr: String::from_utf8_lossy(&output.stderr).into(),
        exit_code: output.status.code().unwrap_or(-1),
        host: None,
    }))
}
```

**Refactoring required in `execution/executor.rs`:**
Similar to ssh_command — extract a capture-mode execution function. The existing streaming-output mode stays for the task runner; the new function captures and returns.

---

### 1.4 `ssh_exec_all(hosts, cmd)` — Parallel remote execution

This is the fan-out primitive. Starlark is single-threaded, so parallelism must happen inside Rust.

```rust
fn ssh_exec_all<'v>(
    hosts: Value<'v>,  // expects a Starlark list of strings
    cmd: &str,
    heap: &'v Heap,
) -> anyhow::Result<Value<'v>> {
    // 1. Unpack hosts list from Starlark Value
    // 2. Spawn threads (rayon or std::thread::scope) — one per host
    // 3. Each thread calls do_ssh_exec(host, cmd)
    // 4. Collect results, allocate as Starlark list of ExecResult
    let host_list: Vec<String> = /* unpack from Value */;
    let results: Vec<ExecResult> = std::thread::scope(|s| {
        let handles: Vec<_> = host_list.iter().map(|h| {
            s.spawn(|| do_ssh_exec(h, cmd))
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })?;
    // Allocate list on Starlark heap
    let starlark_results: Vec<Value> = results.into_iter()
        .map(|r| heap.alloc(r))
        .collect();
    Ok(heap.alloc(starlark_results))
}
```

**Thread safety note:** `ExecResult` values are created in worker threads but allocated on the Starlark `Heap` back on the main thread. The worker threads return plain Rust `ExecResult` structs; heap allocation happens after `join()`.

**Error strategy:** If SSH fails for one host, that host's `ExecResult` gets `exit_code = -1` and `stderr` contains the connection error. The function does NOT abort all hosts on single failure. The Starlark code decides how to handle partial failures:

```python
results = ssh_exec_all(hosts, "docker pull myapp:v2")
failed = [r for r in results if not r.ok]
if failed:
    print("Failed on: " + ", ".join([r.host for r in failed]))
    # decide: abort? retry? continue?
```

---

## Phase 2: Supporting Primitives

### 2.1 `http_get(url)` and `http_post(url, body)` — HTTP client

Needed for health checks. Use `ureq` (blocking, minimal dependencies) or `reqwest::blocking`.

```rust
#[derive(Debug, Clone, ProvidesStaticType, Allocative, NoSerialize)]
pub struct HttpResponse {
    pub status: i32,
    pub body: String,
    pub headers: Vec<(String, String)>,
}
```

Starlark usage:
```python
for attempt in range(10):
    r = http_get("http://{}:3000/up".format(host))
    if r.status == 200:
        break
    sleep(2)
else:
    fail("Health check failed for " + host)
```

**File:** `src/runtime/http.rs` (new)

**Dependency:** Add `ureq` to Cargo.toml (small, blocking HTTP client, ~200KB, no tokio dependency)

---

### 2.2 `upload(host, local_path, remote_path)` and `write_remote(host, content, remote_path)` — File transfer

Two variants:
- `upload` — SCP/SFTP a local file to remote host
- `write_remote` — write a string directly to a remote file (useful for templated configs)

`write_remote` can be implemented as `ssh_exec(host, "cat > {path} << 'NRGEOF'\n{content}\nNRGEOF")` internally, but having a clean API matters.

```python
# Template a config and push it
nginx_conf = """
upstream app {{
    server 127.0.0.1:{port};
}}
""".format(port=new_port)
write_remote(host, nginx_conf, "/etc/nginx/conf.d/app.conf")
ssh_exec(host, "nginx -s reload")
```

**File:** `src/runtime/transfer.rs` (new)

---

### 2.3 `secret(name)` — Secrets access from Starlark

Thin wrapper around existing `secrets/mod.rs`:

```rust
fn secret(name: &str) -> anyhow::Result<String> {
    // Call into existing secrets module
    secrets::get(name)
        .ok_or_else(|| anyhow::anyhow!("Secret '{}' not found", name))
}
```

Starlark usage:
```python
ssh_exec(host, "docker login -u {user} -p {pwd} registry.example.com".format(
    user=secret("REGISTRY_USER"),
    pwd=secret("REGISTRY_PASSWORD"),
))
```

---

### 2.4 `sleep(seconds)` — Blocking delay

Trivial but essential for health check polling loops.

```rust
fn sleep(seconds: f64) -> anyhow::Result<NoneType> {
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    Ok(NoneType)
}
```

---

### 2.5 `state_get(key)` / `state_set(key, value)` — Cross-run persistence

A simple key-value store backed by a JSON file (`.energize/state.json` in the project directory). Enables rollback workflows.

```python
# During deploy
state_set("previous_version", state_get("current_version"))
state_set("current_version", TAG)
state_set("deployed_at", local_exec("date -u +%Y-%m-%dT%H:%M:%SZ").stdout.strip())

# During rollback
previous = state_get("previous_version")
if not previous:
    fail("No previous version recorded")
# ... redeploy previous version
```

**File:** `src/runtime/state.rs` (new)

**Storage location:** `.energize/state.json` in the directory where `nrg` is invoked. Created on first `state_set` call.

---

### 2.6 `env(name)` / `env_or(name, default)` — Environment variable access

```rust
fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .map_err(|_| anyhow::anyhow!("Environment variable '{}' not set", name))
}

fn env_or(name: &str, default: &str) -> anyhow::Result<String> {
    Ok(std::env::var(name).unwrap_or_else(|_| default.to_string()))
}
```

---

## Phase 3: CLI Changes

### 3.1 New execution mode in `cli/run.rs`

Add a second invocation path. When `nrg run` is called **without a task name**, and the `.star` file contains top-level runtime primitive calls, it evaluates the file in "script mode" rather than looking for a named task.

Alternatively (cleaner): add `nrg exec` as a new subcommand that always runs in script mode. This avoids ambiguity.

```
nrg run deploy       # Classic mode — find task "deploy", run it via task_runner
nrg exec             # Script mode — evaluate Energize.star with runtime primitives
nrg exec deploy.star # Script mode — evaluate specific file
```

**Implementation:**
```rust
// cli/exec.rs (new)
pub fn exec_command(file: Option<&str>) -> Result<()> {
    let path = file.unwrap_or_else(|| find_energize_file());
    let content = std::fs::read_to_string(path)?;

    let globals = GlobalsBuilder::standard()
        .with(runtime_builtins)        // ssh_exec, local_exec, ssh_exec_all
        .with(http_builtins)           // http_get, http_post
        .with(transfer_builtins)       // upload, write_remote
        .with(state_builtins)          // state_get, state_set
        .with(util_builtins)           // sleep, secret, env, env_or, print
        .build();

    let module = Module::new();
    let mut eval = Evaluator::new(&module);

    // Optionally: attach SSH connection pool to evaluator extra data
    // eval.extra = Some(&ssh_pool);

    let ast = AstModule::parse("Energize.star", content, &Dialect::Standard)?;
    eval.eval_module(ast, &globals)?;

    Ok(())
}
```

### 3.2 Update `cli/mod.rs` — register `exec` subcommand

Add `Exec` variant to the CLI enum, wire it to `exec_command`.

---

## Phase 4: Refactoring Existing Code

### 4.1 `execution/ssh_command.rs` — Extract capture-mode function

**Before:** Single function that executes and streams output.

**After:** Two functions sharing core SSH logic:
```rust
/// Used by task_runner — streams output to terminal
pub fn ssh_exec_stream(host: &str, cmd: &str, ui: &Ui) -> Result<i32> { ... }

/// Used by Starlark runtime — captures output and returns it
pub fn ssh_exec_capture(host: &str, cmd: &str) -> Result<RawExecResult> { ... }

pub struct RawExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
```

Both call a shared `ssh_connect(host) -> Session` function.

### 4.2 `execution/executor.rs` — Same treatment for local execution

Extract `local_exec_capture(cmd: &str) -> Result<RawExecResult>`.

### 4.3 SSH Connection Pooling

Add connection reuse to `ssh/config.rs`:

```rust
use std::collections::HashMap;
use std::cell::RefCell;

thread_local! {
    static SSH_POOL: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
}

pub fn get_or_connect(host: &str) -> Result<&Session> {
    SSH_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if !pool.contains_key(host) {
            let session = connect(host)?;
            pool.insert(host.to_string(), session);
        }
        Ok(pool.get(host).unwrap())
    })
}
```

Exact implementation depends on which SSH crate is in use (likely `ssh2` or `russh`). Connection pooling is important for performance but can be deferred to a fast-follow — initial implementation can open a new connection per `ssh_exec` call and still work correctly.

---

## New File Structure

```
src/
├── main.rs
├── cli/
│   ├── mod.rs          (add Exec variant)
│   ├── exec.rs         (NEW — script mode entry point)
│   ├── run.rs          (unchanged — classic task mode)
│   ├── init.rs
│   ├── tasks.rs
│   ├── ssh.rs
│   ├── secrets.rs
│   ├── doctor.rs
│   └── ui.rs
├── runtime/            (NEW — all Starlark built-ins)
│   ├── mod.rs          (re-exports all modules)
│   ├── types.rs        (ExecResult, HttpResponse)
│   ├── exec.rs         (ssh_exec, local_exec, ssh_exec_all)
│   ├── http.rs         (http_get, http_post)
│   ├── transfer.rs     (upload, write_remote)
│   ├── state.rs        (state_get, state_set)
│   └── util.rs         (sleep, secret, env, env_or)
├── execution/          (existing — refactored)
│   ├── executor.rs     (add capture mode)
│   ├── ssh_command.rs  (add capture mode)
│   ├── task_runner.rs  (unchanged)
│   └── mod.rs
├── parsing/            (existing — minimal changes)
│   ├── starlark_parser.rs (unchanged for classic mode)
│   ├── bash_parser.rs
│   ├── models.rs       (add ExecResult-related types)
│   ├── env_parser.rs
│   └── mod.rs
├── secrets/            (existing — unchanged)
│   └── mod.rs
└── ssh/                (existing — add connection pooling)
    ├── config.rs
    └── mod.rs
```

---

## New Dependencies

```toml
[dependencies]
# existing deps unchanged, add:
ureq = "2"              # Blocking HTTP client (~200KB, no async runtime)
serde_json = "1"        # For state.json persistence (may already be present)
```

No new heavy dependencies. No tokio/async-std — everything is synchronous and blocking, which aligns with Starlark's execution model.

---

## Example: Kamal-Equivalent Deployment in Starlark

```python
# Energize.star — Docker deployment with zero-downtime proxy swap

# === Configuration ===
APP = "myapp"
IMAGE = "registry.example.com/" + APP
TAG = env_or("DEPLOY_TAG", "latest")

HOSTS = {
    "web": ["10.0.0.1", "10.0.0.2"],
    "worker": ["10.0.0.3"],
}
ALL_HOSTS = HOSTS["web"] + HOSTS["worker"]

HEALTH_CHECK_URL = "/up"
HEALTH_CHECK_RETRIES = 15
HEALTH_CHECK_INTERVAL = 2

# === Helpers ===
def docker(host, subcmd):
    """Run a docker command on a remote host, fail on error."""
    r = ssh_exec(host, "docker " + subcmd)
    if not r.ok:
        fail("docker {} failed on {}: {}".format(subcmd, host, r.stderr))
    return r

def health_check(host, port):
    """Poll HTTP health endpoint until healthy or give up."""
    url = "http://{}:{}{}" .format(host, port, HEALTH_CHECK_URL)
    for attempt in range(HEALTH_CHECK_RETRIES):
        r = http_get(url)
        if r.status == 200:
            return True
        sleep(HEALTH_CHECK_INTERVAL)
    return False

def swap_proxy(host, container_name, port):
    """Tell caddy/nginx/kamal-proxy to route traffic to new container."""
    # This is where the user plugs in their proxy of choice.
    # Example with nginx upstream swap:
    conf = "server 127.0.0.1:{};".format(port)
    write_remote(host, conf, "/etc/nginx/conf.d/{}-upstream.conf".format(APP))
    ssh_exec(host, "nginx -s reload")

# === Build & Push (local) ===
print("Building {}:{}".format(IMAGE, TAG))
r = local_exec("docker build -t {}:{} .".format(IMAGE, TAG))
if not r.ok:
    fail("Build failed: " + r.stderr)

local_exec("docker push {}:{}".format(IMAGE, TAG))

# === Pull on all hosts (parallel) ===
print("Pulling image on {} hosts".format(len(ALL_HOSTS)))
results = ssh_exec_all(ALL_HOSTS, "docker pull {}:{}".format(IMAGE, TAG))
failed = [r for r in results if not r.ok]
if failed:
    fail("Pull failed on: " + ", ".join([r.host for r in failed]))

# === Rolling deploy: web hosts ===
for host in HOSTS["web"]:
    print("Deploying to web host: " + host)
    NEW_PORT = 3001

    # Start new container on alternate port
    docker(host, "run -d --name {}-new -p {}:3000 {}:{}".format(APP, NEW_PORT, IMAGE, TAG))

    # Wait for health
    if not health_check(host, NEW_PORT):
        # Rollback this host
        docker(host, "rm -f {}-new".format(APP))
        fail("Health check failed on " + host)

    # Swap traffic
    swap_proxy(host, APP, NEW_PORT)

    # Stop old, rename new
    ssh_exec(host, "docker rm -f {app}-current 2>/dev/null; docker rename {app}-new {app}-current".format(app=APP))
    print("  {} done".format(host))

# === Workers: parallel restart (no health check / proxy) ===
print("Deploying workers")
ssh_exec_all(HOSTS["worker"], " && ".join([
    "docker rm -f {}-worker 2>/dev/null".format(APP),
    "docker run -d --name {app}-worker {img}:{tag} bin/worker".format(app=APP, img=IMAGE, tag=TAG),
]))

# === Record state ===
state_set("previous_version", state_get("current_version") or "none")
state_set("current_version", TAG)

print("Deployed {}:{} to {} hosts".format(APP, TAG, len(ALL_HOSTS)))
```

This is ~80 lines. It handles: build, push, parallel pull, rolling web deploy with health checks, proxy swap, worker restart, state tracking. No framework, no YAML, full control.

---

## Example: Rollback Script

```python
# rollback.star
APP = "myapp"
IMAGE = "registry.example.com/" + APP
PREVIOUS = state_get("previous_version")

if not PREVIOUS or PREVIOUS == "none":
    fail("No previous version recorded. Cannot rollback.")

print("Rolling back to " + PREVIOUS)

# The image is already on the hosts (was pulled during previous deploy)
# Just re-run the deploy sequence with the old tag
HOSTS_WEB = ["10.0.0.1", "10.0.0.2"]
for host in HOSTS_WEB:
    ssh_exec(host, "docker rm -f {app}-current; docker run -d --name {app}-current -p 3001:3000 {img}:{tag}".format(
        app=APP, img=IMAGE, tag=PREVIOUS))
    # ... health check + proxy swap same as deploy

state_set("current_version", PREVIOUS)
state_set("previous_version", "none")
print("Rolled back to " + PREVIOUS)
```

---

## Implementation Order

| Step | What | Est. LOC | Depends on |
|------|------|----------|------------|
| 1 | `src/runtime/types.rs` — ExecResult + HttpResponse types | ~120 | nothing |
| 2 | Refactor `ssh_command.rs` — add capture mode | ~80 | nothing |
| 3 | Refactor `executor.rs` — add capture mode | ~40 | nothing |
| 4 | `src/runtime/exec.rs` — ssh_exec, local_exec, ssh_exec_all | ~200 | 1, 2, 3 |
| 5 | `src/runtime/util.rs` — sleep, secret, env | ~60 | nothing |
| 6 | `src/runtime/state.rs` — state_get, state_set | ~80 | nothing |
| 7 | `src/runtime/http.rs` — http_get, http_post | ~100 | 1 |
| 8 | `src/runtime/transfer.rs` — upload, write_remote | ~100 | 2 |
| 9 | `src/runtime/mod.rs` — register all modules with GlobalsBuilder | ~40 | 4-8 |
| 10 | `src/cli/exec.rs` — new exec subcommand | ~60 | 9 |
| 11 | Wire exec into `cli/mod.rs` + `main.rs` | ~20 | 10 |
| 12 | SSH connection pooling | ~100 | 4 |

**Total new code: ~1000 LOC**
**Refactored code: ~120 LOC** (ssh_command.rs + executor.rs capture modes)

Steps 1-3 can be done in parallel.
Steps 4-8 can be done in parallel (after 1-3).
Steps 9-11 are sequential.
Step 12 is independent optimization.

---

## Open Questions

1. **Should `ssh_exec` also stream output to the terminal in addition to capturing it?** Useful for long-running commands where the user wants to see progress. Could be a flag: `ssh_exec(host, cmd, quiet=False)`.

2. **Should `ssh_exec_all` support batching?** E.g., `ssh_exec_all(hosts, cmd, batch_size=2)` to deploy to 2 hosts at a time. Easy to add, but the user can also do this in Starlark with list slicing + a for loop.

3. **Should `fail()` be a built-in or rely on Starlark's native `fail()`?** Starlark has `fail()` built in. It raises an error that stops evaluation. This should be sufficient — just verify it produces clean error output in the CLI.

4. **`print()` behavior:** Starlark has built-in `print()`. By default in the `starlark` crate, it writes to stderr. May want to redirect to the Energize UI module for consistent formatting.

5. **Timeout on `ssh_exec`:** Should there be an optional `timeout` parameter? Kamal has deploy timeouts. Could be `ssh_exec(host, cmd, timeout=300)`.

6. **Which SSH crate is currently in use?** Need to verify whether it's `ssh2` (libssh2 bindings, synchronous) or `russh` (pure Rust, async). This affects how `ssh_exec_capture` is implemented and whether connection pooling is straightforward.
