---
title: Builtins
parent: Reference
nav_order: 2
---

# Builtins

This page lists every function your script can call without importing anything: run commands, copy files, call HTTP APIs, keep state, read secrets and undo changes.

Functions you get by importing `std/...` modules are on [Standard library](stdlib.md). Rhai language tips are on [Writing scripts](authoring.md).

## How to read the tables

Each group below has one table with one row per way of calling a function.

| Column | Meaning |
| --- | --- |
| Signature | The call and its argument names. Extra rows for the same name are optional arguments. |
| Returns | What you get back. `()` means nothing useful (Rhai's "unit" value). |
| Dry run | What the function does with `--dry-run`. See [Dry run](dry-run.md) for the full picture. |

Four words are used in the Dry run column:

- **Runs** means the function does its real work, even in a dry run. It only reads.
- **Plans** means it writes a line into the plan and returns a made-up success. Nothing happens on the host.
- **Same** means a dry run makes no difference.
- **Skipped** means the call does nothing in a dry run.

The Notes column says what the call does and what makes it throw.

A dry run still reads from your hosts. It does not change anything on them.

Here is a first script that uses a few builtins. Save it as `Energize.rhai` and run `nrg exec --dry-run`:

```rhai
let results = ssh_exec_all(["web1", "web2"], "docker pull ghcr.io/acme/shop:v2");
for r in results {
    if !r.ok { throw "pull failed on " + r.host + ": " + r.stderr; }
}
write_remote("web1", "PORT=3000\n", "/etc/shop/shop.env");
```

```text
PLAN (dry run — no changes made):
  ssh-all web1                   docker pull ghcr.io/acme/shop:v2 [planned; execution-unverified]
  ssh-all web2                   docker pull ghcr.io/acme/shop:v2 [planned; execution-unverified]
  write   web1                   write 10 bytes -> /etc/shop/shop.env [planned; execution-unverified]
3 action(s), 2 host(s). 0 executed.
...
```

## Result types

### ExecResult

Every command builtin returns an `ExecResult`. Its fields are read-only.

| Field | Type | Meaning |
| --- | --- | --- |
| `.stdout` | string | What the command printed. |
| `.stderr` | string | What the command printed as errors. |
| `.exit_code` | int | `0` means success. |
| `.host` | string | The host it ran on. Empty for local commands. |
| `.ok` | bool | `true` when `exit_code` is `0`. |

```rhai
let r = ssh_exec("web1", "uptime");
if !r.ok { throw "uptime failed on " + r.host + ": " + r.stderr; }
print(r.stdout);
```

In a dry run you get an `ExecResult` with empty output, `exit_code` 0 and `.host` set to the target.

### HttpResponse

Every HTTP builtin returns an `HttpResponse`. Its fields are read-only.

| Field | Type | Meaning |
| --- | --- | --- |
| `.status` | int | The HTTP status code. `0` means the request itself failed (no connection, DNS error, timeout). |
| `.body` | string | The response body. When `.status` is `0`, it reads `request failed: ...`. |
| `.ok` | bool | `true` for status 200 to 299 only. Redirects, 4xx and 5xx are not ok. |

```rhai
let resp = http_get("https://shop.example.com/up");
if resp.ok { print("healthy"); }
```

HTTP calls are made from the machine that runs nrg, not from a host. `shop.example.com` has to be reachable from your laptop or CI runner.

A 404 or 409 comes back with its real status and body, so you can read an API's error message.

### Secret

A `Secret` holds a value that must not leak into logs. You get one from [`secret`](#secrets) and unwrap it with `reveal` or `sh_quote`.

| What you do with it | What happens |
| --- | --- |
| `debug(s)` | Shows `***`. |
| `"text " + s` | Stops with an error. |
| `` `${s}` `` or `to_string(s)` | Gives a placeholder text. Any command that contains the placeholder is refused before it runs. |
| `reveal(s)` | Gives the real value as a string. |
| `sh_quote(s)` | Gives the real value, quoted for a shell. |

## Commands

Run commands on a host over SSH, or on this machine. Every command is checked first: a command that contains a `Secret` placeholder is refused.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `ssh_exec(host, cmd)` | ExecResult | Plans | Runs `cmd` on `host`. A failing command does not throw: check `.ok`. |
| `ssh_probe(host, cmd)` | ExecResult | Runs | Same as `ssh_exec`, meant for read-only commands. `nrg` does not enforce that: it runs whatever you give it, even in a dry run. Use it to look at a host and branch on the answer. |
| `local_exec(cmd)` | ExecResult | Plans | Runs `cmd` on this machine. `.host` is empty. |
| `ssh_exec_all(hosts, cmd)` | array of ExecResult | Plans, one line per host | Runs `cmd` on every host at the same time (16 at a time). One result per host, in the order you gave. |
| `ssh_exec_stdin(host, cmd, stdin)` | ExecResult | Plans (the plan shows the command, not the input) | Sends `stdin` to the command's standard input. The input never appears on a command line or in logs. |
| `local_exec_stdin(cmd, stdin)` | ExecResult | Plans | Same as `ssh_exec_stdin`, on this machine. |
| `is_dry_run()` | bool | Same | `true` during `--dry-run`. |

`ssh_exec_all` never stops at the first failure. You get every result and decide what to do. A host name that is not a string throws an error, so a typo in the list cannot turn into `ssh ""`.

```rhai
let results = ssh_exec_all(["web1", "web2"], "docker pull ghcr.io/acme/shop:v2");
let failed = [];
for r in results {
    if !r.ok { failed.push(r.host); }
}
if failed.len() > 0 { throw "pull failed on: " + join(failed, ", "); }
```

Use `ssh_exec_stdin` to pass a password without putting it in a command:

```rhai
let pw = secret("REGISTRY_PASSWORD");
ssh_exec_stdin("web1", "docker login -u robot --password-stdin ghcr.io", reveal(pw));
```

`is_dry_run()` lets you skip a call that cannot work in a dry run. `pick_port` has no dry-run fallback: it scans the host for real and throws if it cannot. Guard it like this:

```rhai
let port = if is_dry_run() { "<auto>" } else { "" + pick_port("web1", 3000) };
```

### Named steps

A step is a command with a name. It stops the script when the command fails, streams its output while it runs and is listed by name in the audit trail (`nrg audit`).

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `ssh_step(host, name, cmd)` | ExecResult | Plans | Throws if `cmd` exits non-zero. |
| `ssh_step(host, name, cmd, options)` | ExecResult | Plans | `options` is a map, see below. |
| `local_step(name, cmd)` | ExecResult | Plans | Same, on this machine. |
| `local_step(name, cmd, options)` | ExecResult | Plans | Same, with options. |

The returned `.stdout` and `.stderr` hold only the last 8 KiB of output. Use `ssh_exec` when you need to parse the whole output.

| Option | Default | Meaning |
| --- | --- | --- |
| `cwd` | `local_step`: the directory you ran nrg from. `ssh_step`: the SSH user's login directory | Working directory for the command. |
| `env` | none | Map of environment variables. They are kept off the command line and hidden from logs. |
| `stdin` | none | Text sent to the command's standard input. |
| `timeout_secs` | 600 | Give up after this many seconds. The default can be changed with the `NRG_COMMAND_TIMEOUT_SECS` environment variable. |
| `stream` | `true` | Show output while the command runs. |
| `retries` | `0` | Extra attempts after a failure, at most 10. Needs `idempotent: true`. |
| `retry_delay_ms` | `1000` | Wait between attempts, at most 60000. |
| `idempotent` | `false` | Your promise that running the command twice is safe. |

An unknown option name throws before that step runs. Earlier statements have already run.

```rhai
ssh_step("web1", "pull image", "docker pull ghcr.io/acme/shop:v2",
         #{ timeout_secs: 120, retries: 2, idempotent: true });
```

### Remote locks

`remote_lock_acquire` and `remote_lock_release` guard a directory on a host so two deploys cannot run at once. `deploy()` in `std/deploy` uses them for you, so you rarely call them yourself. See [Safety and limits](safety.md).

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `remote_lock_acquire(host, directory)` | `()` | Plans (`mkdir <directory>`) | Creates `directory` on `host`. Throws `could not acquire deploy lock` if it already exists. |
| `remote_lock_release(host, directory)` | `()` | Plans (`rmdir <directory>`) | Removes the lock only if this run owns it. Never throws. A lock still held when the run ends is released automatically. |

## Files

Copy files to and from hosts, and read local files.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `write_remote(host, content, remote_path)` | ExecResult | Plans (`write N bytes -> path`) | Writes the text to a file that only its owner can read (mode `0600`). |
| `write_remote(host, content, remote_path, permissions)` | ExecResult | Plans (adds `(mode 0644)`) | Same with a mode you choose, such as `"0644"`. |
| `read_file(path)` | string | Runs | Reads a local UTF-8 text file. Throws if it is missing, not a regular file, over 16 MiB or not UTF-8. |
| `upload_file(host, local_path, remote_path, permissions)` | ExecResult | Plans, and does not open the file | Copies a local file to the host. |
| `download_file(host, remote_path, local_path, permissions)` | ExecResult | Plans, and does not connect | Copies a file from the host to this machine. |

`permissions` is an octal string such as `"0600"` or `"0644"`. Special bits and letters like `u+x` are refused.

`write_remote` sends the content over standard input, so it never appears on a command line or in logs. It writes a temporary file next to the target, sets the mode, then renames it into place. A reader never sees a half-written file. It also fixes the mode of a file that already exists. A symlink or directory as the target is refused.

```rhai
let token = secret("APP_TOKEN");
let env = "APP_TOKEN=" + reveal(token) + "\nPORT=3000\n";
write_remote("web1", env, "/etc/shop/shop.env");                                   // mode 0600
write_remote("web1", read_file("deploy/Caddyfile"), "/etc/caddy/Caddyfile", "0644"); // readable by the proxy
```

`upload_file` and `download_file` move bytes without loading them into the script. Use them for archives and other files you do not need to read. They use plain SSH and need no `rsync`. The file is written under a temporary name and renamed when complete. The owner of an uploaded file is the SSH user. A downloaded file is owned by you. The parent directory must exist and be writable.

`upload_file` and `download_file` throw when the transfer fails. `write_remote` does not throw: it returns an ExecResult whose `.ok` is `false` and whose stderr says `write_remote failed (payload output suppressed)`. Check `.ok` when the write matters.

`read_file` does load the content into the script. Use it for small config files you render or pass to `write_remote`.

## Preflight

### `preflight(checks, allow_temporary)`

Runs a list of checks before you change anything. It returns `()` and throws on the first failed check.

`checks` is an array of maps. Each map has a `name`, a `kind` and, for a remote check, a `host`. Leave `host` out to check this machine.

| Kind | Other keys | What it does |
| --- | --- | --- |
| `read-only` | `command` | Runs your command. You promise it only reads. It also runs in a dry run. |
| `syntax` | `command` | Checks the shell text with `bash -n` without running it. |
| `file-permissions` | `directory` (default `.`) | Writes a test file twice in `directory` and checks that it ends up with mode `0600`. Removes the test files afterwards. |
| `rsync-permissions` | `directory`, `tool` (default `rsync`) | Same as `file-permissions`, using `rsync`. |

The two `...-permissions` kinds create files, so you must pass `true` as `allow_temporary`. A dry run skips them and says so in the plan.

```rhai
preflight([
    #{ name: "docker is installed", kind: "read-only", host: "web1", command: "docker --version" },
    #{ name: "release script parses", kind: "syntax", command: read_file("deploy/release.sh") },
    #{ name: "can write env files", kind: "file-permissions", host: "web1", directory: "/etc/shop" },
], true);
```

Output of a real run (`nrg exec`):

```text
[preflight:read-only] docker is installed: passed
[preflight:syntax] release script parses: passed
[preflight:temporary-write] can write env files: passed
```

With `false`, in a real run, the same script stops with `can write env files is a temporary-write check; explicitly enable temporary checks`.

nrg does not guess what a command does. A `read-only` check is only as honest as the command you write. Test something meaningful, such as `docker --version`, not just `command -v docker`. A passing local check does not prove a remote host works. You can also run the same checks from a JSON file with `nrg doctor --checks FILE`; see [CLI](cli.md).

## HTTP

Call web APIs. Requests time out after 30 seconds.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `http_get(url)` | HttpResponse | Runs | A real request, so your plan can depend on how production looks now. |
| `http_get(url, headers)` | HttpResponse | Runs | `headers` is a map of strings. `nrg` never follows redirects, so a 3xx comes back as it is. |
| `http_get(url, headers, timeout_secs)` | HttpResponse | Runs | Own timeout in seconds. |
| `http_post(url, body)` | HttpResponse | Plans, returns status 200 | Sends `body` as `application/json`. |
| `http_post(url, body, headers)` | HttpResponse | Plans, returns status 200 | |
| `http_put(url, body)` | HttpResponse | Plans, returns status 200 | Same as `http_post` with PUT. |
| `http_put(url, body, headers)` | HttpResponse | Plans, returns status 200 | |
| `http_patch(url, body)` | HttpResponse | Plans, returns status 200 | Same as `http_post` with PATCH. |
| `http_patch(url, body, headers)` | HttpResponse | Plans, returns status 200 | |
| `http_delete(url)` | HttpResponse | Plans, returns status 200 | No body. |
| `http_delete(url, headers)` | HttpResponse | Plans, returns status 200 | |
| `http_patch_all(requests)` | array of HttpResponse | Plans, one line per request, each returns status 200 | Many PATCH requests at once. |
| `synthetic_http_response(status)` | HttpResponse | Same | Builds a response with that status and an empty body. `wait_healthy` returns one in a dry run. |

Headers are a map whose values are strings. To send a secret, unwrap it with `reveal`:

```rhai
let auth = #{ "Authorization": "Bearer " + reveal(secret("API_TOKEN")) };
http_put("https://api.example.com/apps/1/image", to_json(#{ image: "ghcr.io/acme/shop:v2" }), auth);
http_delete("https://api.example.com/apps/1/old-release", auth);
```

The plan shows how many headers a request has, never their values. If you pass your own `Content-Type`, it replaces the default `application/json`.

A write request in a dry run is not sent. The plan line reads `[assumed ok] PATCH https://...`, and the script gets a 200.

Poll an address until it answers:

```rhai
let healthy = false;
for attempt in 0..30 {
    if http_get("https://shop.example.com/up").ok { healthy = true; break; }
    sleep(2);
}
if !healthy { throw "shop never became healthy"; }
```

Under `--dry-run` this script still throws: `http_get` really runs, but `sleep` is skipped, so the loop ends at once if the site is not up yet. For a polling check that passes in a dry run, use `wait_healthy` from `std/healthcheck`. See [Standard library](stdlib.md).

### `http_patch_all`

`http_patch_all` sends many PATCH requests at the same time (16 at a time) and returns the answers in the order you gave. One failing request does not stop the others. `requests` is an array of maps with `url`, `body` and an optional `headers`. A map with a missing or wrong-typed `url` or `body` throws an error that names its position in the array.

```rhai
let auth = #{ "Authorization": "Bearer " + reveal(secret("API_TOKEN")) };
let requests = [
    #{ url: "https://api.example.com/apps/1", body: to_json(#{ tag: "v2" }), headers: auth },
    #{ url: "https://api.example.com/apps/2", body: to_json(#{ tag: "v2" }), headers: auth },
];
let responses = http_patch_all(requests);
```

Dry run output for the two requests above (set `NRG_SECRET_API_TOKEN` first, see [Secrets](#secrets)):

```text
  check   -                      [assumed ok] PATCH https://api.example.com/apps/1 (1 header(s)) [planned; execution-unverified]
  check   -                      [assumed ok] PATCH https://api.example.com/apps/2 (1 header(s)) [planned; execution-unverified]
```

## Containers and ports

Typed reads of what is running on a host. Use these instead of `docker inspect` through `ssh_exec`, because a dry run plans an `ssh_exec` but runs these.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `container_running(host, name)` | bool | Runs | `true` if the container is running. |
| `container_healthy(host, name)` | bool | Runs | `true` if its health status is `healthy`. One check. Loop yourself to wait. |
| `container_image_id(host, tag)` | string | Runs | The image id for `tag`, or `""` if the host does not have the image. |
| `port_open(host, port)` | bool | Runs | `true` if something listens on `localhost:port` on the host. |
| `pick_port(host, base)` | int | Runs | The first free port starting at `base + 10000`. Throws if none of 100 ports are free. |

When a read cannot run (SSH is down, Docker is missing), a normal run throws. A dry run records `[probe failed — assuming ... absent]` in the plan and answers `false` or `""`, so you can still preview a plan from a laptop that cannot reach the hosts. `pick_port` is the exception: it has no fallback and throws in a dry run too, for example `no free host port found on web1 in 13000..13100 (all 100 candidates are in use)`.

`port_open` and `pick_port` need `nc` on the host. The container reads use the runtime set by `set_runtime` in `std/runtime` (default `docker`).

A read in a dry run shows the state from before the run. It does not see what the plan would have changed.

```rhai
if !container_running("web1", "shop") {
    print("shop is not running on web1");
}
```

## State

State is a small store of text values that is kept between runs, in `.energize/state.json`. Use it to remember what you deployed. Keys and values are strings. A key cannot contain `/`.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `state_get(key)` | string, or `()` if missing | Same | Read the value. |
| `has_state(key)` | bool | Same | `true` if the key is set. |
| `state_set(key, value)` | `()` | Plans (`key = <N bytes>`), nothing saved | Saves one value. |
| `state_update(values)` | `()` | Plans one line per key, nothing saved | Saves several values together or none. `values` is a map of strings. |
| `state_del(key)` | `()` | Plans, nothing saved | Deletes a key. A missing key is fine. |
| `state_all()` | map | Same | Every key and value. |
| `nrg_dest()` | string | Same | The `--dest` you passed, or `"default"`. |

In a dry run, later reads see the values you "set" earlier in the same run, so the script behaves as it would for real. Nothing is written to disk. The plan shows the size of a value, never the value.

`state_get` gives `()` for a missing key, and `()` is not a bool. This fails: `if state_get(k) { ... }`. Write one of these instead:

```rhai
if has_state("shop.version") { print("last deployed " + state_get("shop.version")); }
if state_get("shop.version") != () { print("seen before"); }
```

`state_update` writes several keys in one step:

```rhai
state_update(#{ "shop.version": "v2", "shop.port.web1": "3001" });
```

`nrg_dest()` lets one script serve several environments:

```rhai
let domain = if nrg_dest() == "staging" { "staging.shop.example.com" } else { "shop.example.com" };
```

## Session

Session values live in memory for one run. They are shared by every module the script imports and gone when `nrg` exits. They never touch the disk, in a real run or a dry run.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `session_set(key, value)` | `()` | Same | Stores a string. |
| `session_get(key)` | string, or `()` if missing | Same | Test with `has_session` or `!= ()`. |
| `has_session(key)` | bool | Same | `true` if the key is set. |

## Secrets

Read secrets from the environment or from encrypted files. See [Secrets](secrets.md) for how to create them.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `secret(name)` | Secret | Runs | Looks up `name`. Throws if it is missing or shorter than 6 characters. |
| `reveal(secret)` | string | Same | The real value. Use it for stdin, a file or an HTTP header. |
| `sh_quote(text)` | string | Same | Quotes plain text for a shell. |
| `sh_quote(secret)` | string | Same | Quotes the real value for a shell. The safe way to put a secret on a command line. |
| `to_string(secret)` | string | Same | Gives a placeholder, never the value. A command containing it is refused. Interpolation uses this. |
| `to_debug(secret)` | string | Same | Always `"***"`. `debug()` uses it. |

`secret(name)` checks two places, in this order:

1. The environment variable `NRG_SECRET_<NAME>` (the name in capitals). It holds the plain value, as CI sets it.
2. The files `.energize/secrets.<dest>` (only with `--dest`) and then `.energize/secrets`. Each line is `KEY=ENC[...]`, made by `nrg secrets encrypt KEY`.

A `.env` file is never read. Decryption also happens in a dry run. A value that is too short to hide reliably (under 6 characters) is refused.

Every secret you read is hidden (shown as `***`) in logs and plans.

```rhai
let pw = secret("REGISTRY_PASSWORD");
print(sh_quote("a b$c"));        // 'a b$c'
ssh_exec("web1", "myctl --token=" + sh_quote(secret("API_TOKEN")));
```

Adding a `Secret` to a string stops the script with an error. Without that, you would build a broken command with `***` in it:

```text
Error: Runtime error: refusing to concatenate a Secret into a string; use sh_quote(secret) for a shell argument or reveal(secret) for explicit plaintext
```

Interpolation fails later, when the command runs:

```text
Error: Runtime error: a Secret was string-converted into a command (e.g. via interpolation `${secret}` or to_string()); use sh_quote(secret) for a shell argument or reveal(secret) for explicit plaintext
```

## Transactions

A transaction lets you register undo steps as you go. If the body throws, the undo steps run, newest first, and then the original error is raised again.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `transaction(body)` | `()` | Runs the body. Undo steps are listed in the plan, never run | `body` is a closure. On success the undo steps are dropped. |
| `on_rollback(cb)` | `()` | Plans `register compensation`, never registers or calls `cb` | Registers an undo closure. It runs only if the enclosing `transaction` fails. |
| `in_transaction()` | bool | Same | `true` while any `transaction` is active, including nested ones. |

```rhai
transaction(|| {
    ssh_exec("web1", "docker run -d --name shop-new ghcr.io/acme/shop:v2");
    on_rollback(|| { ssh_exec("web1", "docker rm -f shop-new"); });

    let healthy = false;
    for attempt in 0..30 {
        if is_dry_run() || container_healthy("web1", "shop-new") { healthy = true; break; }
        sleep(2);
    }
    if !healthy {
        throw "new container is not healthy";   // runs the undo step above
    }
});
```

- An undo step that fails is logged (`rollback step failed (continuing)`) and the others still run.
- A nested transaction that succeeds keeps its undo steps. If the outer one fails later, they run too.
- `on_rollback` outside a `transaction` does nothing.
- In a dry run, `on_rollback` only lists the undo step in the plan. It never registers it. If the body throws, the error is raised again and no undo step runs. `container_healthy` reads the host as it is now, and the plan has not started `shop-new`, so guard such checks with `is_dry_run()` as above.
- `container_healthy` is one check, and a container that has just started usually reports `starting`. Loop as above to wait for it.

`deploy()` and `rollback()` from `std/deploy` use `in_transaction()` to refuse to run inside another transaction. See [Standard library](stdlib.md).

## Utilities

Small helpers. None of them touch a host, and they behave the same in a dry run, except `sleep`.

| Signature | Returns | Dry run | Notes |
| --- | --- | --- | --- |
| `join(array, sep)` | string | Same | Joins items with `sep`. Numbers and bools become text. Rhai has no `join` of its own. |
| `sleep(seconds)` | `()` | Skipped | Waits. Zero or negative does nothing. |
| `nrg_env(name)` | string | Same | Reads a required environment variable. Throws if it is not set. |
| `env_or(name, default)` | string | Same | Reads an environment variable, or gives `default`. |
| `json_string(s)` | string | Same | `s` as a JSON string, with quotes and escapes. |
| `to_json(value)` | string | Same | Turns a map, array, string, number or bool into JSON. |
| `from_json(text)` | any | Same | Parses JSON. Throws `from_json: invalid JSON: ...` on bad input. |
| `url_encode(s)` | string | Same | Percent-encodes everything except `A-Z a-z 0-9 - _ . ~`. |

```rhai
join(["-p", "80:80", "-p", "443:443"], " ");   // "-p 80:80 -p 443:443"
join([1, 2, 3], "-");                          // "1-2-3"
url_encode("p@ss:w/rd#1");                     // "p%40ss%3Aw%2Frd%231"
env_or("APP_TAG", "latest");                   // "latest" when APP_TAG is not set
print(from_json(to_json(#{ port: 3000 })).port);   // 3000
```

`url_encode` is for a value inside a URL, such as a password in `postgres://user:<password>@host`. If the text holds a known secret, the encoded form is hidden in logs too.

`json_string` is for building JSON by hand without letting a value break out of its quotes:

```rhai
let msg = "say \"hi\"";
let body = "{\"text\":" + json_string(msg) + "}";   // {"text":"say \"hi\""}
```

## Next

- [Standard library](stdlib.md) for `deploy()` and the other ready-made modules built on these builtins.
- [Dry run](dry-run.md) for what a plan means and what it cannot tell you.
- [Writing scripts](authoring.md) for Rhai tips, such as how `trim()` works and where `import` goes.
