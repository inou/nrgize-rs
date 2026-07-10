// registry.rhai — Container registry authentication helpers.
//
// Handles logging into container registries on local and remote hosts.
// Supports Docker Hub, GitHub Container Registry, AWS ECR, and generic
// registries. Works with Docker, Podman, OrbStack, or nerdctl via
// lib/runtime.rhai.
//
// Usage:
//   import "lib/registry" as registry;
//   let pw = secret("REGISTRY_PASSWORD");
//   registry::registry_login(host, "ghcr.io", "myuser", pw);
//
// SECRET-HANDLING CONTRACT: the password is delivered to `--password-stdin`
// OFF-ARGV via ssh_exec_stdin / local_exec_stdin. The plaintext NEVER appears
// on argv, in the command string, or in the dry-run plan. `password` should be
// a Secret obtained from secret("..."); we reveal() it only at the very last
// moment, as the stdin argument. NEVER concatenate the Secret into a string
// (that throws) and NEVER `echo '<pw>' | ...` it onto the command line.

import "lib/runtime" as rt;

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

// registry_login(host, server, username, password) — log into a registry.
//   host:     SSH host (use "local" for the build machine).
//   server:   registry server URL, e.g. "ghcr.io".
//   username: registry username.
//   password: a Secret (from secret("...")) — revealed only onto stdin.
// Returns the ExecResult; throws on failure.
fn registry_login(host, server, username, password) {
    // The password goes to --password-stdin; it is NOT on the command line. server/username flow
    // from user input — sh_quote them (issue #10).
    let cmd = rt::container_cmd() + " login " + sh_quote(server) + " -u " + sh_quote(username) + " --password-stdin";
    let r = if host == "local" {
        local_exec_stdin(cmd, reveal(password))
    } else {
        ssh_exec_stdin(host, cmd, reveal(password))
    };
    if !r.ok {
        throw "Registry login failed on " + host + " for " + server + ":\n" + r.stderr;
    }
    r
}

// registry_login_all(hosts, server, username, password) — log into a registry
// on all hosts. Passes the Secret through unchanged (never stringified).
fn registry_login_all(hosts, server, username, password) {
    for host in hosts {
        registry_login(host, server, username, password);
    }
}

// ---------------------------------------------------------------------------
// AWS ECR helpers
// ---------------------------------------------------------------------------

// ecr_login(host, cfg) — log into AWS ECR using the AWS CLI.  Requires `aws`
// on the target host.  cfg: #{region: "us-east-1", account_id: ""}
//
// The ECR token is produced and consumed entirely inside the remote shell
// pipeline (`aws ecr get-login-password ... | <cmd> login --password-stdin`),
// so no Rhai-side Secret object is involved and the token never enters the
// script — keep it as a single command string.
// Returns the ExecResult; throws on failure.
fn ecr_login(host, cfg) {
    let region = if cfg.contains("region") { cfg.region } else { "us-east-1" };
    let account_id = if cfg.contains("account_id") { cfg.account_id } else { "" };

    let _cmd = rt::container_cmd();
    let server = if account_id != "" {
        account_id + ".dkr.ecr." + region + ".amazonaws.com"
    } else {
        "" // Let the subshell below resolve the account.
    };

    // region / server flow from cfg — sh_quote them (issue #10). The account-auto-detect branch
    // embeds a `$(...)` subshell followed by literal text; `region` is spliced in as its OWN
    // sh_quote()'d (single-quoted) segment, adjacent to the double-quoted segments either side —
    // shell concatenates adjacent quoted strings with no separator, and single quotes keep
    // region's contents fully literal (previously it was pasted raw inside the double-quoted
    // segment, where `$`, backticks, and `\` stay live — robustness review R1).
    let login_cmd = "aws ecr get-login-password --region " + sh_quote(region) + " | " + _cmd + " login --username AWS --password-stdin ";
    if server != "" {
        login_cmd += sh_quote(server);
    } else {
        // Auto-detect account from the caller identity.
        login_cmd += "\"$(aws sts get-caller-identity --query Account --output text).dkr.ecr.\"" + sh_quote(region) + "\".amazonaws.com\"";
    }

    let r = if host == "local" {
        local_exec(login_cmd)
    } else {
        ssh_exec(host, login_cmd)
    };
    if !r.ok {
        throw "ECR login failed on " + host + ":\n" + r.stderr;
    }
    r
}

// ecr_login(host) — 1-arg overload (defaults).
fn ecr_login(host) {
    ecr_login(host, #{})
}
