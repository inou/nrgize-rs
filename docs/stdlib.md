---
title: Standard library
parent: Reference
nav_order: 3
---

# Standard library

Every `std/` module and every public function in it, with the config keys each one reads.

The standard library is a set of Rhai modules built into the `nrg` binary. You import one at the top of your `Energize.rhai` and call its functions with `module::function(...)`:

```rhai
import "std/deploy" as deploy;
import "std/healthcheck" as health;

fn ship() {
    deploy::deploy(["web1", "web2"], "ghcr.io/acme/shop:v1", "shop", #{ domain: "shop.example.com" });
    health::wait_healthy("https://shop.example.com/up");
}
```

Run it with `nrg run ship`, or add `--dry-run` to see the plan first.

## The modules

| Module | What it is for | Guide |
| --- | --- | --- |
| `std/deploy` | Zero-downtime rolling deploys, rollbacks and accessory containers | [Deploying containers](deploy.md) |
| `std/release` | Versioned directories on a host, switched with a `current` symlink | [Directory releases](directory-releases.md) |
| `std/release_recipes` | Build defaults for Rails, Django, Next.js, Phoenix and Laravel | [Directory releases](directory-releases.md) |
| `std/recipe` | One call that logs in to a registry, starts accessories and deploys | [Recipes](examples.md) |
| `std/docker` | Build, push, pull, run, stop and inspect containers | none |
| `std/caddy` | Run Caddy and switch traffic to a new container | [Deploying containers](deploy.md) |
| `std/healthcheck` | Wait until a URL, a port or a container is healthy | [Recipes](examples.md) |
| `std/registry` | Log in to a container registry, including AWS ECR | [Recipes](examples.md) |
| `std/runtime` | Choose Docker, Podman or nerdctl | none |
| `std/mise` | Install Erlang, Elixir and Node for one app with mise | none |
| `std/notify` | Post to a webhook or to Slack | [Recipes](examples.md) |
| `std/contracts` | Declare the steps and checks for `nrg rehearse` | [Testing a deploy](rehearsal.md) |
| `std/bunny` | Deploy to Bunny Magic Containers over its API | none |

## Rules that apply everywhere

- Put `import` lines at the top of the script, so every function can see the module.
- Options are a map: `#{ key: value }`. There are no keyword arguments. Many functions also have a shorter form without `cfg`, which uses the defaults. The tables below list each form that exists.
- Most calls throw on failure. Wrap them in `try { ... } catch (e) { ... }` if you want to recover. Some calls return a result map instead, so check `.ok` on those. The tables say which.
- A `Secret` from `secret("NAME")` cannot be joined to a string. Use `reveal(secret)` where you need the text, as late as possible.
- `state_get(key)` returns `()` when the key is missing. Check with `has_state(key)` first.
- A module cannot be edited in place. To change one function, copy it from [`lib/` on GitHub](https://github.com/inou/nrgize-rs/tree/main/lib) into your script and call your copy.

Under `--dry-run`, functions that change something are only recorded in the plan. Functions that only read still talk to your hosts. See [Dry run](dry-run.md) for what a plan does and does not prove.

## std/deploy

Rolls a new image out to every host, one host at a time. If any host fails, the hosts already switched go back to the old container. This is the module most scripts need.

```rhai
import "std/deploy" as deploy;

const HOSTS = ["web1", "web2"];

fn ship(tag) {
    deploy::deploy(global::HOSTS, "ghcr.io/acme/shop:" + tag, "shop", #{
        domain: "shop.example.com",
        health_path: "/up",
    });
}
fn back() { deploy::rollback(global::HOSTS, "shop"); }
```

| Signature | What it does |
| --- | --- |
| `deploy(hosts, image, service, cfg)` | Build, push, pull, start the new container on each host, wait for it to be healthy, then switch Caddy to it. |
| `deploy(hosts, image, service)` | Same, with every option at its default. |
| `rollback(hosts, service, cfg)` | Deploy the previous image again, with the options it was deployed with. `cfg` overrides them. Set `cfg.image` to roll back to a specific image instead; the options of the last deploy are then used. Set `cfg.undo_pre_deploy: true` to also undo the migration of the release you leave, with the `pre_deploy_undo` saved for it (see [Undoing the migration](deploy.md#undoing-the-migration)). |
| `rollback(hosts, service)` | Same, with no overrides. |
| `accessory_run(host, name, image, cfg)` | Start a long-lived container such as Postgres. Does nothing if it is already running. |
| `accessory_run(host, name, image)` | Same, with no options. |
| `accessory_upgrade(host, name, image, cfg)` | Pull a new image and replace the container. Named volumes stay. |
| `accessory_upgrade(host, name, image)` | Same, with no options. |
| `accessory_restart(host, name)` | Restart the same container. It cannot change the image. |
| `accessory_stop(host, name)` | Stop the container but keep it and its volumes. Does nothing if it is already stopped. |
| `run_post_deploy_hook(hosts, cmd)` | Run `cmd` on every host and return a list of `"host: error"` strings for the hosts where it failed. `deploy()` calls this for `post_deploy_cmd`; you rarely call it yourself. |

`image` is the full reference, such as `ghcr.io/acme/shop:v1`. The tag (`v1`) is the version. An image with no tag has the version `latest`.

For every `deploy()` option (`container_port`, `envs`, `health_path`, `domain`, `pre_deploy`, `pre_deploy_version`, `pre_deploy_undo`, `keep_images`, and so on) see [Deploying containers: Options](deploy.md#options). `accessory_*` functions take `ports`, `envs`, `volumes`, `network` and `cmd`.

### Hooks you can define

If your script defines these functions, `deploy()` and `rollback()` call them. All three take `(service, image, hosts)`.

| Function | When it runs | If it throws |
| --- | --- | --- |
| `hook_pre_deploy` | Before anything changes | The deploy stops. |
| `hook_post_deploy` | After the deploy is committed | A warning is printed. |
| `hook_post_rollback` | After a rollback finishes | A warning is printed. |

```rhai
import "std/notify" as notify;

fn hook_post_deploy(service, image, hosts) {
    notify::slack(secret("SLACK_WEBHOOK_URL"), service + " " + image + " is live");
}
```

### State keys

After a successful deploy, `deploy()` saves these keys in `.energize/state.json`. `nrg status`, `nrg rollback` and later deploys read them.

| Key | Holds |
| --- | --- |
| `<service>.version` | The version just deployed, such as `v2`. |
| `<service>.image` | The full image reference just deployed. |
| `<service>.prev` | The image that ran before. This is the default rollback target. |
| `<service>.prev_config` | The options of the deploy before this one, used when you roll back to `.prev`. |
| `<service>.config` | The options of this deploy as JSON, so a rollback can reuse them. It can contain secret values, and the `pre_deploy_undo` command if the release ran `pre_deploy`. |
| `<service>.pre_deploy_probe` | JSON saved when a release with `pre_deploy_version` changed the schema version: the image, the undo, the version command and the versions before and after. A rollback with `undo_pre_deploy` checks the database is still at the version after. Kept out of `.config` on purpose, but like `.config` it can hold a revealed secret if the probe or undo command contains one. It is removed by any deploy that neither writes nor keeps it. |
| `<service>.release_hosts` | Every host the current release (image) was deployed to, as JSON. A repeat deploy of the same image adds hosts (a canary first, then the rest); a new image starts the list again. A rollback replaces it with the hosts it rolled back. A rollback with `undo_pre_deploy` needs all of them. |
| `<service>.undone_images` | Images whose migration a rollback with `undo_pre_deploy` undid, as JSON. A rollback to one of them is refused until `deploy()` runs it again. |
| `<service>.port.<host>` | The port on that host where the container is published. |
| `<service>.target.<host>` | Where Caddy sends traffic on that host, such as `localhost:13000`. |
| `<service>.deployed_at` | The UTC time of the deploy. It is empty in a dry run. |
| `<service>.runtime.cmd`, `<service>.runtime.name` | The container CLI this deploy used. |
| `nrg.runtime.cmd`, `nrg.runtime.name` | The same, kept for `nrg status`, `nrg logs` and `nrg app exec`. |
| `<service>.transition.<host>` | A note written while a host is being switched. If it is still there later, `nrg` could not finish the switch. Check that host by hand. |

Keep `.energize/state.json` out of git and backups. Only the encrypted secrets files are committed. See [what to commit](secrets.md#what-to-commit).

## std/release

Deploys a directory of any kind (a build, a binary, a tarball) to `releases/<version>` on a host, then points `current` at it. No containers needed.

```rhai
import "std/release" as release;

fn config() {
    #{
        prepare: "cp -R /srv/staged/shop/. .",
        activate: "sudo -n systemctl restart shop",
        health: "curl --fail --silent http://127.0.0.1:8080/up",
    }
}
fn deploy(version) { release::deploy("web1", "/srv/shop", version, config()); }
fn rollback(version) { release::rollback("web1", "/srv/shop", version, config()); }
```

| Signature | What it does |
| --- | --- |
| `deploy(host, root, version, cfg)` | Create `root/releases/<version>`, run your commands in it, switch `root/current` to it, run `activate`, then `health`. If `activate` or `health` fails, the previous release is switched back and re-checked. |
| `rollback(host, root, version, cfg)` | Switch `current` back to an existing release and run `activate` and `health` again. You name the version; nothing is guessed. |

`root` is an absolute path without a trailing slash, such as `/srv/shop`. `version` may contain letters, digits, `.`, `_` and `-`. A version that already exists cannot be deployed again.

| `cfg` key | Required | What it does |
| --- | --- | --- |
| `prepare` | deploy only | Shell command that fills the new release directory. Runs inside it. |
| `activate` | yes | Command that starts or reloads your service on the new release. |
| `health` | yes | Command that must succeed for the release to count as live. |
| `build` | no | Command run after `prepare`, inside the release directory. |
| `migrate` | no | Command run after `build`, inside the release directory. |
| `deactivate` | no | Command run if the very first release fails, to stop what `activate` started. |
| `keep` | no | Number of releases to keep, at least `2`. The active and the previous release are always kept. |
| `env` | no | Map of environment variables for every command. |
| `timeout_secs` | no | Time limit for each command. Default `600`. |

Do not name your own function `host` in a script that uses this module. It hides the `host` variable and the call fails with "Function not found: ssh_step".

Every command also sees `NRG_RELEASE_DIR`, `NRG_CURRENT_LINK` and `NRG_PREVIOUS_RELEASE`.

Only one deploy or rollback can run on a host at a time. If another one holds the lock, the second fails at once with `could not acquire deploy lock on web1`. It does not wait. See [Directory releases](directory-releases.md) for the full walkthrough.

## std/release_recipes

Starting values for `std/release`. Each function returns a map with an `env` and a `build` command for one framework. You add `prepare`, `activate` and `health`, and anything you pass replaces the default.

```rhai
import "std/release" as release;
import "std/release_recipes" as recipes;

fn config() {
    recipes::rails(#{
        prepare: "cp -R /srv/staged/shop/. .",
        activate: "sudo -n systemctl restart shop",
        health: "curl --fail http://127.0.0.1:3000/up",
    })
}
fn deploy(version) { release::deploy("web1", "/srv/shop", version, config()); }
```

| Signature | Default `env` | Default `build` |
| --- | --- | --- |
| `rails(cfg)` | `RAILS_ENV=production` | `bundle install && bundle exec rails assets:precompile` |
| `django(cfg)` | none | Create `.venv`, `pip install -r requirements.txt`, `manage.py collectstatic` |
| `nextjs(cfg)` | `NODE_ENV=production` | `npm ci --include=dev && npm run build` |
| `phoenix(cfg)` | `MIX_ENV=prod` | `mix deps.get`, `mix compile`, `mix assets.deploy`, `mix release` |
| `laravel(cfg)` | `APP_ENV=production` | `composer install`, then `artisan` config, route and view caches |

Keys you pass in `env` are added to the defaults. Any other key you pass replaces the default. The recipes do not install runtimes or databases.

## std/recipe

One call for the usual container deploy: log in to the registry, start accessories, deploy. The templates from `nrg init --template rails` (and `django`, `nextjs`, `phoenix`, `laravel`) use it.

```rhai
import "std/recipe" as recipe;

fn ship() {
    recipe::standard_deploy(#{
        service: "shop",
        image_repo: "ghcr.io/acme/shop",
        web_hosts: ["web1", "web2"],
        port: 3000,
        domain: "shop.example.com",
    });
}
```

| Signature | What it does |
| --- | --- |
| `standard_deploy(cfg)` | Choose the runtime, log in, start accessories on `db_host`, then call `deploy::deploy`. |
| `db_hostname(host)` | The host part of `"user@host"`. A name without `@` comes back unchanged. |

```rhai
recipe::db_hostname("deploy@db1");   // "db1"
recipe::db_hostname("db1");          // "db1"
```

| `cfg` key | Required | What it does |
| --- | --- | --- |
| `service` | yes | Service name. |
| `image_repo` | yes | Image without a tag, such as `ghcr.io/acme/shop`. |
| `web_hosts` | yes | List of hosts to deploy to. |
| `version` | no | Image tag. Default: the `DEPLOY_TAG` environment variable, then `latest`. |
| `port` | no | Port inside the container. Passed to `deploy()` as `container_port`. |
| `registry`, `registry_user`, `registry_password` | no | Log in to this registry on your machine and on every web host. All three go together. `registry_password` is a `Secret`. |
| `accessories` | no | List of `#{ name, image, ports, envs, volumes }`. Needs `db_host`. |
| `db_host` | with `accessories` | Host that runs the accessories. |
| `runtime` | no | Value for `set_runtime`. Default `"auto"`. |
| `network` | no | Network name. Passed to `deploy()` and to every accessory. |
| `build_host` | no | Build on this host. With `registry` set, the login also runs there. |

Every other key goes straight to `deploy()`, so all of its [options](deploy.md#options) work here.

## std/docker

Container commands on your machine and on remote hosts. Builds and pushes run locally unless you set `build_host`. Everything else runs over SSH. The command used (`docker`, `podman`, ...) comes from `std/runtime`.

```rhai
import "std/docker" as docker;

fn smoke() {
    docker::docker_pull("web1", "ghcr.io/acme/shop:v1");
    let r = docker::docker_run("web1", "ghcr.io/acme/shop:v1", "shop-test", #{
        ports: #{ "127.0.0.1:13000": "3000" },
        envs: #{ RAILS_ENV: "production" },
    });
    if !r.ok { throw "run failed: " + r.stderr; }
}
```

| Signature | What it does |
| --- | --- |
| `docker_build(tag, cfg)` | Build an image. Throws if the build fails. |
| `docker_build(tag)` | Build with the defaults. |
| `docker_push(host, tag)` | Push `tag` from `host`. Use `""` for your own machine. |
| `docker_push(tag)` | Push from your own machine. |
| `docker_pull(host, tag)` | Pull `tag` on one host. Throws on failure. |
| `docker_pull_all(hosts, tag)` | Pull on all hosts in parallel. Throws and names the hosts that failed. |
| `docker_run(host, tag, name, cfg)` | Start a detached container with `--restart unless-stopped`. Returns the result; check `.ok` yourself. |
| `docker_run(host, tag, name)` | Same, with no options. |
| `docker_run_once(host, tag, command, cfg)` | Run `command` in a throwaway container (`run --rm`) and return the result. `deploy()` uses it for `pre_deploy`. |
| `docker_stop(host, name, cfg)` | Stop a container. Does not fail if it is not there. |
| `docker_stop(host, name)` | Same, with the default timeout. |
| `docker_remove(host, name)` | Remove a container (`rm -f`). Does not fail if it is not there. |
| `docker_rename(host, old_name, new_name)` | Rename a container. A missing container counts as done. Any other error is returned as `ok: false`. |
| `docker_promote(host, new_name, canonical)` | Rename a fresh container to its final name. A missing container is not an error: the call returns `ok: false`, so check `.ok`. |
| `docker_restart(host, name)` | Restart a container in place. Check `.ok` yourself. |
| `docker_network_create(host, name)` | Create a network. "Already exists" counts as success. |
| `docker_network_create_all(hosts, name)` | Create a network on every host. Throws on the first real failure. |
| `docker_container_running(host, name)` | `true` if the container exists and is running. |
| `docker_image_id(host, tag)` | The image id for `tag`, or `""` if the host does not have it. |
| `docker_cleanup(host, cfg)` | Remove exited containers and dangling images. |
| `docker_cleanup(host)` | Same. |
| `docker_prune_old_images(host, repo, keep_n, protect_tags)` | Remove all but the newest `keep_n` tags of `repo`, except tags in `protect_tags`. Returns `#{ ok, removed, stderr }`. `deploy()` calls it for `keep_images`. |
| `docker_exec(host, name, command)` | Run `command` inside a running container. |
| `docker_logs(host, name, cfg)` | Return recent logs, with stderr merged in. |
| `docker_logs(host, name)` | Same, last 100 lines. |

### docker_build keys

| Key | Default | What it does |
| --- | --- | --- |
| `context` | `"."` | Build context directory. |
| `dockerfile` | `"Dockerfile"` | Dockerfile path. |
| `build_args` | `#{}` | Build arguments as `#{ KEY: "value" }`. |
| `platform` | `""` | One platform such as `"linux/amd64"`, or a comma-separated list for a multi-arch image. A list pushes during the build and uses `buildx`. |
| `build_host` | `""` | Build on this host over SSH instead of your machine. Push from the same host. |

A `build_host` build copies the context to the host first. It never sends `.nrg-key`, `.nrg-key.pub`, `.energize/` or `.env` from the context root. See [Multi-arch builds](deploy.md#multi-arch-builds).

### docker_run keys

| Key | Default | What it does |
| --- | --- | --- |
| `ports` | `#{}` | `#{ "host_port": "container_port" }`. A key like `"127.0.0.1:13000"` binds one address. |
| `envs` | `#{}` | Environment variables. They are written to a private file on the host, not to the command line. A newline in a key or value throws, and so does a key that contains `=`. |
| `volumes` | `#{}` | `#{ "/host/path": "/container/path" }`. |
| `network` | `""` | Network name. |
| `extra` | `""` | Raw arguments added as written. Never put secrets or untrusted input here. |

`docker_run_once` takes `envs` and `network`. Its `command` is used as written, so build it only from input you trust. `docker_stop` takes `timeout` (default `30`). `docker_logs` takes `tail` (default `100`). `docker_cleanup` ignores its `cfg`.

## std/caddy

Runs [Caddy](https://caddyserver.com/) in a container on each host and moves traffic between containers. Each service is one route. Switching it is one call that replaces the upstream.

```rhai
import "std/caddy" as caddy;

fn maintenance(mode) {
    caddy::proxy_maintenance("web1", "shop", mode == "on", #{ target: "localhost:13000" });
}
```

Run `nrg run maintenance on`, and later `nrg run maintenance off`.

| Signature | What it does |
| --- | --- |
| `proxy_boot(host, cfg)` | Start Caddy if it is not running. Caddy and its routes come back after a reboot. |
| `proxy_boot(host)` | Same, with Caddy 2. |
| `proxy_boot_all(hosts, cfg)` | `proxy_boot` on each host in turn. |
| `proxy_boot_all(hosts)` | Same, with no options. |
| `proxy_deploy(host, service, target, cfg)` | Send `service` to `target` (`host:port`). Updates the route, or creates it if it does not exist. |
| `proxy_deploy(host, service, target)` | Same, with no options. |
| `proxy_remove(host, service)` | Delete the service's route. A missing route counts as removed. |
| `proxy_set_tls(host, service, domain)` | Match `domain` on an existing route, so Caddy gets a certificate for it. |
| `proxy_maintenance(host, service, on_off, cfg)` | `true` answers every request with a fixed response. `false` sends traffic back to `cfg.target`. |
| `proxy_maintenance(host, service, true)` | Turn maintenance on with no options. Turning it off needs `cfg.target`, so use the form above. |
| `proxy_list(host)` | Return the registered routes. |
| `proxy_stop(host)` | Stop and remove the Caddy container. |

| `cfg` key | Used by | What it does |
| --- | --- | --- |
| `proxy_version` | `proxy_boot` | Caddy image tag. Default `"2"`. |
| `domain` | `proxy_deploy` | Match this host name. Caddy gets a Let's Encrypt certificate for it. |
| `health_path` | `proxy_deploy`, `proxy_maintenance` | Path Caddy checks on the upstream every 10 seconds. |
| `target` | `proxy_maintenance` | Required when turning maintenance off. Caddy does not remember the old upstream. |
| `status_code` | `proxy_maintenance` | Status for the maintenance response, `100` to `599`. Default `503`. |
| `message` | `proxy_maintenance` | Body of the maintenance response. |

## std/healthcheck

Waits until something is healthy. Each function throws if it never gets there.

```rhai
import "std/healthcheck" as health;

fn up() {
    health::wait_healthy_on_host("web1", 3000, #{ path: "/up", consecutive: 2 });
}
```

```console
$ nrg run up --dry-run
  health check passed on web1 (2/30, 2/2 consecutive)
...
```

In a dry run, `wait_healthy`, `wait_port` and `wait_container_healthy` do not poll. They print a `[dry-run] assuming ...` line and carry on. `wait_healthy_on_host` and `wait_healthy_all` treat every probe as a `200`, so they print the normal pass line, as above.

| Signature | What it does |
| --- | --- |
| `wait_healthy(url, cfg)` | `GET` the URL from your machine until it returns the expected status. Returns the last response. |
| `wait_healthy(url)` | Same, with the defaults. |
| `wait_healthy_on_host(host, port, cfg)` | Run `curl` on `host` against `http://localhost:<port><path>`. Use this for deploy targets: the host name may not be reachable by HTTP from your machine. Returns `#{ status }`. |
| `wait_healthy_on_host(host, port)` | Same, with the defaults. |
| `wait_healthy_all(hosts, port, cfg)` | `wait_healthy_on_host` on each host in turn. |
| `wait_healthy_all(hosts, port)` | Same, with the defaults. |
| `wait_port(host, port, cfg)` | Wait for a TCP port to accept connections. Returns `true`. |
| `wait_port(host, port)` | Same, with the defaults. |
| `wait_container_healthy(host, name, cfg)` | Wait for the image's `HEALTHCHECK` to report healthy. Returns `true`. The image must define one. |
| `wait_container_healthy(host, name)` | Same, with the defaults. |

| `cfg` key | Default | Used by | What it does |
| --- | --- | --- | --- |
| `attempts` | `30` | all | Most tries before giving up. |
| `interval` | `2` | all | Seconds between tries. |
| `expected_status` | `200` | the HTTP checks | Status that counts as healthy. |
| `consecutive` | `1` | the HTTP checks | Passes in a row required. A failure resets the count. |
| `timeout` | `30` | the HTTP checks | Seconds to wait for each request. Must be a positive number. |
| `path` | `"/up"` | `wait_healthy_on_host`, `wait_healthy_all` | Path to request. |

`attempts` is the hard limit. Raising `consecutive` does not give you more time, so raise `attempts` with it for an app that flaps.

## std/registry

Logs in to a container registry so hosts can pull private images.

Set the password first. It must be at least 6 characters. See [Secrets](secrets.md) for other places to keep it.

```console
$ export NRG_SECRET_REGISTRY_PASSWORD=change-me-123
```

```rhai
import "std/registry" as registry;

fn login() {
    registry::registry_login_all(["web1", "web2"], "ghcr.io", "acme", secret("REGISTRY_PASSWORD"));
}
```

```console
$ nrg run login --dry-run
...
  ssh-stdin web1   docker login 'ghcr.io' -u 'acme' --password-stdin
  ssh-stdin web2   docker login 'ghcr.io' -u 'acme' --password-stdin
...
```

The password goes in on standard input. It never appears on a command line or in the plan.

| Signature | What it does |
| --- | --- |
| `registry_login(host, server, username, password)` | Log in on one host. Use `"local"` for your own machine. `password` must be a `Secret`. |
| `registry_login_all(hosts, server, username, password)` | Log in on every host. |
| `ecr_login(host, cfg)` | Log in to AWS ECR with the `aws` CLI on `host`. The token never reaches your script. |
| `ecr_login(host)` | Same, with region `us-east-1` and the account found automatically. |

| `ecr_login` key | Default | What it does |
| --- | --- | --- |
| `region` | `"us-east-1"` | ECR region. |
| `account_id` | `""` | AWS account. When empty, `aws sts get-caller-identity` finds it on the host. |

## std/runtime

Chooses the container command that every other module uses. Call it first, before other `std/` functions.

```rhai
import "std/deploy" as deploy;
import "std/runtime" as rt;

fn ship(tag) {
    rt::set_runtime("podman");
    deploy::deploy(["web1"], "ghcr.io/acme/shop:" + tag, "shop", #{ container_port: 3000 });
}
```

| Signature | What it does |
| --- | --- |
| `set_runtime(runtime)` | Use `"docker"`, `"podman"`, `"nerdctl"` or `"auto"` for this run. Throws on any other value. |
| `set_runtime()` | Same as `"auto"`. |
| `container_cmd()` | The command in use, such as `"docker"`. Default `"docker"`. |
| `runtime_name()` | A readable name. It can be `"orbstack"`. |
| `is_docker()` | `true` when the name is exactly `"docker"`. |
| `is_podman()` | `true` when the name is `"podman"`. |
| `runtime_run_flags()` | Flags added to every `run`. Always `--restart unless-stopped`. |
| `runtime_exec_cmd(container_name, command)` | The text `<cmd> exec '<container_name>' <command>`, for example `docker exec 'shop-web' ls`. |
| `local_build_cmd()` | The command used for builds and pushes on your machine. On macOS it is Apple's `container` tool when that is running, otherwise `container_cmd()`. |
| `set_local_build_runtime(runtime)` | Pin the local build command to `"docker"`, `"podman"`, `"nerdctl"` or `"container"`. |

| Value | Meaning |
| --- | --- |
| `"docker"` | Docker, and anything with the same CLI (OrbStack, Rancher Desktop, colima). This is the default. |
| `"podman"` | Podman. |
| `"nerdctl"` | nerdctl with containerd. Experimental. |
| `"auto"` | Try docker, then podman, then nerdctl. |

The choice lasts for one run. It is also saved so `nrg status`, `nrg logs` and `nrg app exec` know which command to use.

A dry run cannot probe your machine, so `"auto"` always picks `docker`. Name the runtime if you want a different one in the plan.

## std/mise

Installs Erlang, Elixir and optionally Node for one app, using [mise](https://mise.jdx.dev/). Versions go in the app's own `mise.toml`. Nothing global changes.

```rhai
import "std/mise" as mise;

const HOST = "web1";
const APP = "/opt/shop";
const TOOLS = #{ erlang: "28.2", elixir: "1.19.4-otp-28", node: "22.14.0" };

fn provision() {
    preflight(mise::checks(global::HOST, global::TOOLS), false);
    mise::provision(global::HOST, global::APP, global::TOOLS);
}
fn compile() {
    ssh_step(global::HOST, "Compile", mise::command(global::APP, global::TOOLS, "cd source && MIX_ENV=prod mix compile"));
}
```

| Signature | What it does |
| --- | --- |
| `provision(host, app, cfg)` | Pin and install Erlang, then Elixir, then Node if given. Verify each one. Repeating it reuses what is installed. |
| `command(app, cfg, shell_command)` | Build a command that runs `shell_command` with those tool versions. It never installs anything. |
| `verify(host, app, cfg)` | Check that `erl`, `elixir`, `mix` and, if given, `node` work. |
| `checks(host, cfg)` | A read-only check for `preflight` that your mise supports `use --path`, `--pin` and `exec`. |

| `cfg` key | Required | What it does |
| --- | --- | --- |
| `erlang` | yes | Exact version, such as `"28.2"`. |
| `elixir` | yes | Exact version, such as `"1.19.4-otp-28"`. |
| `node` | no | Exact version, such as `"22.14.0"`. |
| `mise` | no | Absolute path to mise. Default `$HOME/.local/bin/mise`. |

`latest` and a bare major version like `22` are rejected. `app` must be an absolute path other than `/`, and your SSH user must be able to write to it. Install mise and your system build tools yourself.

## std/notify

Posts a message to a webhook. Both functions return the response and never throw, so a failed message does not fail your deploy. Check `.ok` if it matters.

```rhai
import "std/notify" as notify;

fn tell(msg) {
    notify::slack(secret("SLACK_WEBHOOK_URL"), msg);
}
```

| Signature | What it does |
| --- | --- |
| `webhook(url, payload)` | `POST` a JSON string to `url`. `url` can be a `Secret`, and stays hidden in the plan. |
| `slack(url, text)` | `POST {"text": ...}` to a Slack incoming webhook. Quotes and newlines in `text` are escaped for you. |

A dry run records the post and does not send it.

The example needs `SLACK_WEBHOOK_URL` as a secret, the same way as `REGISTRY_PASSWORD` above: `export NRG_SECRET_SLACK_WEBHOOK_URL=...`. The `hook_post_deploy` example in `std/deploy` needs it too.

## std/contracts

Writes down what `nrg rehearse` should run. These functions only build maps. They never run a command. See [Testing a deploy](rehearsal.md) for how to run a contract.

Save this as `Rehearsal.rhai`. It is a contract file, not an `Energize.rhai`.

```rhai
import "std/contracts" as c;

c::service(#{
    name: "shop",
    deploy: c::step("deploy", "sh ./rehearsal/deploy.sh"),
    restart: c::step("restart", "sh ./rehearsal/restart.sh"),
    checks: [c::step("responds", "sh ./rehearsal/check.sh")],
    cleanup: [c::step("stop", "sh ./rehearsal/cleanup.sh")],
})
```

```console
$ nrg rehearse Rehearsal.rhai
first install / deploy: planned; execution-unverified (expected 0, actual unverified)
first install / responds: planned; execution-unverified (expected 0, actual unverified)
...
Rehearsal 'shop': execution-unverified
No commands ran. Use --execute for trusted local commands; --faults enables declared fault scenarios.
```

| Signature | What it does |
| --- | --- |
| `step(name, command)` | One command that must exit with `0`. |
| `scenario(name, steps, checks)` | Actions, then checks that must hold afterwards. |
| `rejects(name, command, exit_code, checks)` | A fault: the command must fail with exactly `exit_code` (`1` to `125`), and then the checks must pass. |
| `fault(name, steps, checks)` | A fault with several steps, such as killing a process and starting it again. |
| `service(cfg)` | The usual lifecycle: first install, repeat install, restart, plus optional faults. |

`service` accepts these keys and throws on any other.

| Key | Required | What it does |
| --- | --- | --- |
| `name` | yes | Name of the contract. |
| `deploy` | yes | The step that installs the app. Runs for the first and the repeat install. |
| `restart` | yes | The step that restarts it. |
| `checks` | yes | Steps that must pass after each scenario. |
| `cleanup` | yes | Steps that remove what the contract created. |
| `setup` | no | Steps that run once before the scenarios. |
| `env` | no | Environment variables for every command. |
| `faults` | no | List of `rejects(...)` or `fault(...)` scenarios. They run only with `--faults`. |

A step is a map: `name`, `run`, and optionally `expect_exit` (default `0`), `timeout_secs` (default `60`, from `1` to `3600`) and `stdin`.

## std/bunny

A second deploy target: [Bunny Magic Containers](https://bunny.net/magic-containers/), a hosted container service with no SSH. These functions call Bunny's REST API with `http_get`, `http_post`, `http_patch`, `http_patch_all` and `http_delete`. Most functions take one `cfg` map. `find_container` and `deploy_fleet` take the arguments shown in the table.

```rhai
import "std/bunny" as bunny;

fn ship(tag) {
    let cfg = #{ app_id: "12345", api_key: secret("BUNNY_API_KEY"), container: "web", image_tag: tag };
    bunny::deploy_app(cfg);
    bunny::wait_for_image(cfg);
}
fn back() {
    bunny::rollback_app(#{ app_id: "12345", api_key: secret("BUNNY_API_KEY"), container: "web" });
}
```

| Signature | What it does |
| --- | --- |
| `deploy_app(cfg)` | Point one container at a new image tag. Saves the old tag as `bunny.<app_id>.<container>.prev`. |
| `rollback_app(cfg)` | Point the container back at the saved image: the tag, plus the name and digest when they were recorded. Throws if nothing was saved. |
| `wait_for_image(cfg)` | Poll until the container reports `image_tag`. Skipped in a dry run. |
| `deploy_fleet(targets, cfg)` | Deploy to many apps: canaries one at a time, then parallel batches. Returns `#{ succeeded, failed }`. |
| `current_image_tag(cfg)` | The tag the container runs now, or `()`. |
| `app_config(cfg)` | The app's full configuration as a map. |
| `find_container(app_config, container_name)` | The container with that name from an `app_config`. Throws if none or several match. |
| `create_app(cfg)` | Create an app with one container named `web`, one replica, one region. Returns the app, including its `id`. |
| `delete_app(cfg)` | Delete an app. It leaves DNS, CDN and storage alone. |

| `cfg` key | Used by | What it does |
| --- | --- | --- |
| `app_id` | all except `create_app`, `deploy_fleet` | The Bunny app id. |
| `api_key` | all | A `Secret` or a string. Sent as the `AccessKey` header. |
| `container` | `deploy_app`, `rollback_app`, `wait_for_image`, `current_image_tag` | The container's name. |
| `image_tag` | `deploy_app`, `wait_for_image` | The tag to run. |
| `image_name`, `image_digest` | `deploy_app` | Optional image name and digest to send with the tag. |
| `attempts`, `interval` | `wait_for_image` | Tries (default `30`) and seconds between them (default `2`). |
| `base_url` | all | API host. Default `https://api.bunny.net`. |

`deploy_fleet(targets, cfg)` takes a list of target maps. Each needs `app_id`, `container` and `image_tag`. It also accepts `image_name`, `image_digest`, `health_url` (checked after the deploy) and `base_url`. Its `cfg` takes `api_key`, plus these options:

| Fleet key | Default | What it does |
| --- | --- | --- |
| `canary_size` | `1` | Targets deployed one at a time first. |
| `batch_size` | `5` | Targets deployed in parallel after the canaries. |
| `max_failures` | `0` | The fleet stops and throws once more targets than this have failed. Targets that already succeeded are not undone. |
| `health_attempts`, `health_interval` | `30`, `2` | Tries and seconds between them. |
| `require_version_health` | `false` | Also require the health check to report the new version. Every target then needs `health_url`. |

`create_app(cfg)` requires `name`, `image_registry`, `image_namespace`, `image_name`, `image_tag` and `region_id`, and checks them before it calls the API. It also needs `api_key`. Optional keys are `env` (a list of `#{ name, value }`) and `volume` (`#{ name, size, path }`).

nrg refuses the keys `region`, `regions`, `replica`, `replicas`, `replica_count`, `scale` and `zone`. `deploy_app`, `rollback_app` and `create_app` check their `cfg`, and `deploy_fleet` checks its `cfg` and each target. A volume-backed app must stay on one replica in one region, so change those in Bunny's dashboard.

A dry run still sends real `GET` requests to `api.bunny.net` with your API key, because reading the app's configuration is a read. The plan shows them as `GET ... -> 200 (probed live)`. Writes are only recorded.

Under `--dry-run`, `create_app` returns an empty map, so `app.id` is `()`. Do not pass it to another call without checking `is_dry_run()`.

## Next

- [Deploying containers](deploy.md): `std/deploy` in a full walkthrough.
- [Builtins](builtins.md): the functions that are always available, with no import.
- [Dry run](dry-run.md): what a plan shows and what it cannot.
