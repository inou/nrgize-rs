# Energize (`nrg`)

A deployment toolkit written in Rust. Define servers and tasks in simple config files, or write full deployment orchestration scripts in Starlark — from basic SSH task running to Kamal-style zero-downtime Docker deployments.

## Quick Start

```bash
# Scaffold a new config file
nrg init

# List available tasks and macros
nrg tasks

# Run a task
nrg run deploy

# Execute a deployment script
nrg exec

# Validate your setup
nrg doctor
```

## Installation

Build from source (requires Rust 1.70+):

```bash
cd nrgize-rs
cargo build --release
cp target/release/nrg ~/.local/bin/   # or anywhere on your PATH
```

### Optional Dependencies

| Tool      | Required for            | Install                            |
|-----------|-------------------------|------------------------------------|
| `age`     | Secret encryption       | `brew install age` / `apt install age` |
| `rsync`   | File upload (preferred) | Usually pre-installed              |
| `scp`     | File upload (fallback)  | Part of OpenSSH                    |
| `docker`  | Container deployments   | https://docs.docker.com/get-docker |
| `podman`  | Container deployments (alternative) | https://podman.io/getting-started |
| OrbStack  | Container deployments (macOS) | https://orbstack.dev |

---

## Two Modes

Energize has two modes that serve different needs:

### 1. Task Runner (`nrg run`)

Define named tasks and run them across servers. Great for simple deployments, maintenance scripts, and ad-hoc operations. Uses the Starlark DSL (`servers()`, `task()`, `define_macro()`) or Bash annotations.

```bash
nrg run deploy
nrg run restart --var branch=main
```

### 2. Orchestration Engine (`nrg exec`)

Evaluate a Starlark file as a deployment script with runtime primitives — SSH execution, HTTP requests, file transfers, state persistence. Starlark code runs top-to-bottom, and built-in functions have real side effects as they're evaluated. This is how you build Kamal-style zero-downtime Docker deployments.

```bash
nrg exec                    # Runs Energize.star
nrg exec deploy.star        # Runs a specific file
NRG_TRACE=1 nrg exec       # With debug tracing
```

---

## Exec Mode: Deployment Orchestration

### Runtime Primitives

These built-in functions are available in any `.star` file run via `nrg exec`:

#### Execution

| Function | Signature | Description |
|---|---|---|
| `ssh_exec` | `(host, cmd) -> ExecResult` | Run a command on a remote host via SSH |
| `local_exec` | `(cmd) -> ExecResult` | Run a command locally via `sh -c` |
| `ssh_exec_all` | `(hosts, cmd) -> [ExecResult]` | Run a command on multiple hosts in parallel |

**ExecResult** attributes: `stdout`, `stderr`, `exit_code`, `host`, `ok`

#### HTTP

| Function | Signature | Description |
|---|---|---|
| `http_get` | `(url) -> HttpResponse` | HTTP GET (30s timeout) |
| `http_post` | `(url, body) -> HttpResponse` | HTTP POST with JSON content type |

**HttpResponse** attributes: `status`, `body`, `ok`

#### File Transfer

| Function | Signature | Description |
|---|---|---|
| `upload` | `(host, local_path, remote_path)` | SCP a file to a remote host |
| `write_remote` | `(host, content, remote_path)` | Write a string to a remote file |

#### State Persistence

| Function | Signature | Description |
|---|---|---|
| `state_get` | `(key) -> string\|None` | Read from `.energize/state.json` |
| `state_set` | `(key, value)` | Write to state |
| `state_del` | `(key)` | Delete a state key |
| `state_all` | `() -> dict` | Read all state |

#### Utilities

| Function | Signature | Description |
|---|---|---|
| `sleep` | `(seconds)` | Blocking delay (integer seconds) |
| `nrg_env` | `(name) -> string` | Get env var (fails if unset) |
| `env_or` | `(name, default) -> string` | Get env var with fallback |
| `secret` | `(name) -> string` | Get secret from env/files |
| `print` | `(value)` | Print to stderr |

### Module System (`load()`)

Starlark's `load()` statement lets you import functions from other `.star` files:

```python
load("lib/docker.star", "docker_build", "docker_run")
load("helpers.star", "notify_slack")
```

Paths are resolved relative to the directory of the file being executed. Loaded modules have access to all the same runtime primitives, so a function defined in `helpers.star` can call `ssh_exec()` just fine.

Modules are evaluated once and cached — loading the same file from multiple places doesn't re-execute it.

### Standard Library (`lib/`)

Energize ships with a standard library of reusable modules for common deployment patterns:

#### `lib/docker.star` — Container Lifecycle

```python
load("lib/docker.star", "docker_build", "docker_push", "docker_run", "docker_stop")
```

| Function | Description |
|---|---|
| `docker_build(tag, context, dockerfile, build_args)` | Build image locally |
| `docker_push(tag)` | Push image to registry |
| `docker_pull(host, tag)` | Pull image on a remote host |
| `docker_pull_all(hosts, tag)` | Pull on all hosts in parallel |
| `docker_run(host, tag, name, ports, envs, volumes, network)` | Start a container |
| `docker_stop(host, name, timeout)` | Stop a container |
| `docker_remove(host, name)` | Remove a container |
| `docker_rename(host, old_name, new_name)` | Rename a container |
| `docker_container_running(host, name) -> bool` | Check if container is running |
| `docker_image_id(host, tag) -> string` | Get image ID on remote |
| `docker_cleanup(host)` | Prune old containers and images |
| `docker_exec(host, name, cmd)` | Exec into a running container |
| `docker_logs(host, name, tail)` | Get container logs |

#### `lib/proxy.star` — kamal-proxy Management

```python
load("lib/proxy.star", "proxy_boot", "proxy_deploy")
```

| Function | Description |
|---|---|
| `proxy_boot(host, http_port, https_port)` | Ensure kamal-proxy is running |
| `proxy_boot_all(hosts)` | Boot proxy on all hosts |
| `proxy_deploy(host, service, target, health_path)` | Zero-downtime traffic switch |
| `proxy_remove(host, service)` | Remove a service from proxy |
| `proxy_set_tls(host, service, domain)` | Configure Let's Encrypt TLS |
| `proxy_list(host)` | List registered services |
| `proxy_stop(host)` | Stop the proxy |

#### `lib/healthcheck.star` — Health Verification

```python
load("lib/healthcheck.star", "wait_healthy", "wait_port")
```

| Function | Description |
|---|---|
| `wait_healthy(url, attempts, interval, expected_status)` | Poll HTTP endpoint |
| `wait_port(host, port, attempts, interval)` | Wait for TCP port to open |
| `wait_container_healthy(host, name, attempts, interval)` | Wait for Docker HEALTHCHECK |
| `wait_healthy_all(hosts, port, path)` | Health check across all hosts |

#### `lib/registry.star` — Container Registry Auth

```python
load("lib/registry.star", "registry_login", "registry_login_all")
```

| Function | Description |
|---|---|
| `registry_login(host, server, username, password)` | Docker registry login |
| `registry_login_all(hosts, server, username, password)` | Login on all hosts |
| `ecr_login(host, region, account_id)` | AWS ECR login via AWS CLI |

#### `lib/runtime.star` — Container Runtime Abstraction

```python
load("lib/runtime.star", "set_runtime", "container_cmd", "is_podman", "is_docker")
```

| Function | Description |
|---|---|
| `set_runtime(runtime)` | Set runtime: `"docker"`, `"podman"`, `"nerdctl"`, or `"auto"` (default) |
| `container_cmd() -> string` | Get the container CLI command (e.g. `"docker"` or `"podman"`) |
| `runtime_name() -> string` | Human-readable runtime name (e.g. `"orbstack"`) |
| `is_podman() -> bool` | True if Podman is the active runtime |
| `is_docker() -> bool` | True if Docker (or OrbStack) is the active runtime |
| `runtime_run_flags() -> string` | Extra flags for `run` on the current runtime |
| `runtime_login_cmd(server, user, pass)` | Build a registry login command |
| `runtime_exec_cmd(container, cmd)` | Build a container exec command |

Auto-detection order: Docker → Podman → nerdctl. OrbStack is detected as a Docker variant (it provides the standard `docker` CLI).

All library modules (`docker.star`, `proxy.star`, `registry.star`) use `container_cmd()` internally, so configuring the runtime once at the top of your script makes everything work with your chosen container engine.

#### `lib/nginx.star` — Nginx Reverse Proxy

```python
load("lib/nginx.star", "nginx_boot", "nginx_configure", "nginx_enable_tls")
```

An alternative to kamal-proxy for teams that prefer nginx. Supports both containerized (recommended) and system-installed nginx.

| Function | Description |
|---|---|
| `nginx_boot(host)` | Start nginx in a container (host networking, config volumes) |
| `nginx_boot_all(hosts)` | Boot on all hosts |
| `nginx_install(host)` | Install nginx from system packages (apt/yum) |
| `nginx_configure(host, service, domain, upstream_port)` | Generate reverse proxy config for a service |
| `nginx_switch_upstream(host, service, new_port)` | Zero-downtime upstream port switch via `sed` + reload |
| `nginx_enable_tls(host, service, domain, email)` | Issue Let's Encrypt cert and configure HTTPS |
| `nginx_remove(host, service)` | Remove a site config |
| `nginx_reload(host)` | Graceful config reload (connections drain) |
| `nginx_restart(host)` | Hard restart |
| `nginx_stop(host)` | Stop and remove nginx |
| `nginx_status(host)` | Show running config |
| `nginx_logs(host, tail, error)` | Get access or error logs |

Generated configs include: gzip, WebSocket proxy, security headers, ACME challenge location, and `client_max_body_size 100m`. TLS configs add HSTS, OCSP stapling, and HTTP→HTTPS redirect.

#### `lib/tls.star` — Let's Encrypt / ACME Certificates

```python
load("lib/tls.star", "tls_proxy", "tls_certbot", "tls_certbot_dns", "tls_check_expiry")
```

Three strategies depending on your setup:

| Function | Strategy | Use when |
|---|---|---|
| `tls_proxy(host, service, domain)` | kamal-proxy built-in ACME | Using kamal-proxy (simplest) |
| `tls_certbot(host, domain, email)` | Standalone certbot (HTTP-01) | No kamal-proxy, port 80 reachable |
| `tls_certbot_dns(host, domain, email, plugin)` | Certbot DNS challenge | Wildcards, internal servers |

Management functions:

| Function | Description |
|---|---|
| `tls_list(host)` | List all certbot-managed certificates |
| `tls_renew(host, force)` | Manually trigger renewal |
| `tls_check_expiry(host, domain, warn_days)` | Check cert expiry, warn if close |
| `tls_check_expiry_all(hosts, domain, warn_days)` | Check across all hosts |
| `tls_set_renewal_hook(host, hook_cmd)` | Run a command after renewal (e.g. reload nginx) |

DNS plugins supported: `cloudflare`, `route53`, `digitalocean`, `google`.

#### `lib/provision.star` — Remote Server Provisioning

```python
load("lib/provision.star", "provision_docker", "provision_podman", "provision_base")
```

| Function | Description |
|---|---|
| `provision_docker(hosts, version)` | Install Docker CE via official repos (Debian/Ubuntu, RHEL/Fedora) |
| `provision_podman(hosts, rootless)` | Install Podman via distro repos |
| `provision_runtime(hosts, runtime)` | Install "docker" or "podman" by name |
| `provision_base(hosts)` | Install common tools (curl, git, netcat, fail2ban, ufw) |

All functions are idempotent — they skip hosts where the runtime is already working. Distro is auto-detected via `/etc/os-release`.

#### `lib/deploy.star` — Full Deployment Orchestration

```python
load("lib/deploy.star", "deploy", "rollback", "accessory_run")
```

| Function | Description |
|---|---|
| `deploy(hosts, image, service, ...)` | Full Kamal-style zero-downtime deploy |
| `deploy_to_host(host, image, service, ...)` | Deploy to a single host |
| `rollback(hosts, service, image)` | Roll back to a previous version |
| `accessory_run(host, name, image, ports, envs, volumes)` | Start a long-lived service (DB, Redis, etc.) |

The `deploy()` function runs the full pipeline: build -> push -> pull -> rolling per-host deploy with health checks -> kamal-proxy traffic switch -> old container cleanup -> state persistence.

---

## Container Runtimes

Energize supports multiple container runtimes through the `lib/runtime.star` abstraction layer. You don't need to change any library code — just call `set_runtime()` at the top of your deployment script.

### Docker (default)

Works out of the box with Docker CE/EE, Docker Desktop, and any Docker-compatible runtime.

```python
load("lib/runtime.star", "set_runtime")
set_runtime("docker")  # or just omit — docker is the default
```

### OrbStack (macOS)

OrbStack is a fast Docker Desktop replacement for macOS. It provides the standard `docker` CLI, so Energize auto-detects it and uses it transparently.

```python
load("lib/runtime.star", "set_runtime")
set_runtime("auto")  # auto-detects OrbStack via `docker info`
```

When OrbStack is detected, `runtime_name()` returns `"orbstack"` while `container_cmd()` still returns `"docker"` (since OrbStack uses the Docker CLI).

### Podman

Podman is a daemonless container engine compatible with Docker CLI commands. Works rootless or rootful.

```python
load("lib/runtime.star", "set_runtime")
set_runtime("podman")
```

Podman notes: both `podman` and `docker` CLIs share the same command structure for `build`, `push`, `pull`, `run`, `stop`, `rm`, `exec`, `inspect`, and `login`. The runtime layer handles any minor differences (restart policy flags, rootless considerations).

### nerdctl (experimental)

nerdctl is a Docker-compatible CLI for containerd. Experimental support.

```python
load("lib/runtime.star", "set_runtime")
set_runtime("nerdctl")
```

### Auto-detection

The recommended approach — probes the local system and picks the first available runtime:

```python
load("lib/runtime.star", "set_runtime")
set_runtime("auto")  # tries docker → podman → nerdctl
```

---

## Framework Tutorials

Complete deployment configurations for popular frameworks. Each tutorial is a production-ready `Energize.star` you can copy and customize.

All tutorials follow the same pattern: build image locally, push to registry, pull on all hosts, rolling zero-downtime deploy via kamal-proxy, run migrations, verify health checks, enable HTTPS via Let's Encrypt.

For first-time server setup (provisioning + deploy + TLS in one script), see `lib/examples/setup.star`.

See `lib/examples/` for the full files:

| Framework | File | App Port | Health Path |
|---|---|---|---|
| Ruby on Rails | `lib/examples/rails.star` | 3000 | `/up` |
| Django | `lib/examples/django.star` | 8000 | `/health/` |
| Next.js | `lib/examples/nextjs.star` | 3000 | `/api/health` |
| Phoenix | `lib/examples/phoenix.star` | 4000 | `/health` |
| Laravel | `lib/examples/laravel.star` | 8000 | `/up` |
| Full Setup | `lib/examples/setup.star` | 3000 | `/up` |

### Usage

```bash
# Copy a tutorial to your project root
cp lib/examples/rails.star Energize.star

# Edit configuration (hosts, image, secrets)
$EDITOR Energize.star

# Set up secrets
export NRG_SECRET_REGISTRY_PASSWORD="ghp_xxxx"
export NRG_SECRET_DATABASE_URL="postgres://..."
export NRG_SECRET_SECRET_KEY_BASE="abc123..."

# Deploy
DEPLOY_TAG=v1.0.0 nrg exec

# Deploy with TLS (first time — issues Let's Encrypt cert)
DEPLOY_TAG=v1.0.0 DOMAIN=myapp.example.com nrg exec

# Full setup on fresh servers (provision + deploy + TLS)
DEPLOY_TAG=v1.0.0 DOMAIN=myapp.example.com nrg exec lib/examples/setup.star
```

---

## Task Runner Mode Reference

### Config File Formats

Energize supports two config formats for task runner mode: **Starlark** (recommended) and **Bash**.

On startup, `nrg` looks for config files in this order:

1. `Energize.star`
2. `energize.star`
3. `Energize.sh`
4. `energize.sh`

You can also point to a specific file with `--path` or `--conf`.

### Starlark Format (`.star`)

Starlark is a Python-like configuration language by Google/Meta. It gives you variables, conditionals, loops, and string operations — all deterministic and sandboxed.

```python
# Energize.star

servers(
    staging = "deploy@staging.example.com",
    production = ["deploy@web1.example.com", "deploy@web2.example.com"],
)

BRANCH = var("branch", default = "main")

task(
    name = "deploy",
    on = ["production"],
    confirm = "Deploy to production?",
    script = """
        cd /var/www/app
        git pull origin """ + BRANCH + """
        composer install --no-dev
        php artisan migrate --force
    """,
)

task(
    name = "restart",
    on = ["production"],
    parallel = True,
    script = "sudo systemctl restart php-fpm",
)

define_macro(name = "full-deploy", tasks = ["deploy", "restart"])
```

#### DSL Reference

| Function | Description |
|---|---|
| `servers(**kwargs)` | Define servers (name = host or [hosts]) |
| `task(name, on, script, ...)` | Define a named task |
| `define_macro(name, tasks)` | Group tasks into a sequence |
| `var(name, default)` | Reference a CLI variable |
| `env_file(path, encrypted)` | Load .env file |
| `upload(name, src, dest, on)` | File upload task |
| `docker_deploy(name, image, on)` | Registryless Docker pipeline |
| `before()` / `after()` / `error()` / `success()` / `finished()` | Lifecycle hooks |

### Bash Format (`.sh`)

A simpler format using shell functions with comment annotations:

```bash
# @servers production=deploy@example.com
# @task on:production confirm="Deploy to production?"
deploy() {
    cd /var/www/app && git pull origin main
}
```

### Commands

| Command | Description |
|---|---|
| `nrg run <target>` | Execute a task or macro |
| `nrg exec [file]` | Evaluate a .star deployment script |
| `nrg tasks` | List available tasks and macros |
| `nrg ssh [server]` | Open interactive SSH session |
| `nrg init` | Scaffold a new config file |
| `nrg doctor` | Validate config and test connectivity |
| `nrg secrets <cmd>` | Manage encrypted secrets (age-based) |

### `nrg run` Options

| Flag | Description |
|---|---|
| `--var KEY=VALUE` | Pass a variable (repeatable) |
| `--env <FILE>` | Load environment variables from .env file |
| `--pretend` | Dry-run — print commands, don't execute |
| `--continue` | Continue executing if a task fails |
| `--summary` | Only show the result table |
| `--path <PATH>` | Explicit path to config file |

---

## Secrets

Manage encrypted secrets using [age](https://github.com/FiloSottile/age) encryption.

```bash
nrg secrets init                   # Generate key pair
nrg secrets encrypt "my-secret"    # Encrypt a value
nrg secrets seal .env.prod         # Encrypt entire .env file
nrg secrets unseal .env.prod.enc   # Decrypt .env file for editing
```

In exec mode, use the `secret()` function:

```python
db_pass = secret("DB_PASSWORD")
# Checks: $NRG_SECRET_DB_PASSWORD, $DB_PASSWORD, .energize/secrets, .env
```

## SSH Config Integration

Energize reads `~/.ssh/config` and resolves host aliases automatically.

## License

MIT
