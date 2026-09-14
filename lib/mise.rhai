// App-scoped toolchain; mise and OS build dependencies must already be installed.
private fn version(tool, value) {
    let parts = value.split(".");
    let required = if tool == "erlang" { 2 } else { 3 };
    if parts.len() < required { throw tool + " requires an explicit release version"; }
    for part in parts {
        let digits = part.split("-")[0];
        if digits == "" { throw "invalid version for " + tool; }
        for ch in digits.chars() {
            if ch < '0' || ch > '9' { throw "invalid version for " + tool; }
        }
    }
    tool + "@" + value
}

private fn executable(cfg) {
    if cfg.contains("mise") { sh_quote(cfg.mise) } else { `"$HOME/.local/bin/mise"` }
}

private fn prefix(app) {
    if !app.starts_with("/") || app == "/" { throw "mise app directory must be an absolute non-root path"; }
    "set -eu; export MISE_YES=1; cd " + sh_quote(app) + "; "
}

fn provision(host, app, cfg) {
    let erlang = version("erlang", cfg.erlang);
    let elixir = version("elixir", cfg.elixir);
    let node = if cfg.contains("node") { version("node", cfg.node) } else { "" };
    let base = prefix(app);
    let mise = executable(cfg);
    let config = sh_quote(app + "/mise.toml");
    ssh_step(host, "Prepare app toolchain directory", "mkdir -p " + sh_quote(app));
    // Separate calls are deliberate: Erlang must be in mise.toml before Elixir's installer
    // constructs its dependency environment. An inherited shell PATH is insufficient.
    ssh_step(host, "Register and install Erlang", base + mise + " use --pin --path " + config + " " + sh_quote(erlang));
    ssh_step(host, "Verify Erlang", base + "export MISE_INSTALL_MISSING=0 MISE_EXEC_AUTO_INSTALL=0; " + mise + " exec " + sh_quote(erlang) + " -- erl -noshell -eval " + sh_quote("halt()."));
    ssh_step(host, "Register and install Elixir", base + mise + " use --pin --path " + config + " " + sh_quote(elixir));
    if node != "" {
        ssh_step(host, "Register and install Node", base + mise + " use --pin --path " + config + " " + sh_quote(node));
    }
    verify(host, app, cfg);
}

fn command(app, cfg, shell_command) {
    let tools = sh_quote(version("erlang", cfg.erlang)) + " " + sh_quote(version("elixir", cfg.elixir));
    if cfg.contains("node") { tools += " " + sh_quote(version("node", cfg.node)); }
    // No activation, shell startup edits, global defaults, or implicit installs at execution.
    prefix(app) + "export MISE_INSTALL_MISSING=0 MISE_EXEC_AUTO_INSTALL=0; " + executable(cfg) + " exec " + tools + " -- sh -c " + sh_quote(shell_command)
}

fn verify(host, app, cfg) {
    let check = "erl -noshell -eval 'halt().' && elixir --version && mix --version";
    if cfg.contains("node") { check += " && node --version"; }
    ssh_step(host, "Verify app toolchain", command(app, cfg, check));
}

// Read-only declaration usable with preflight(..., false), before provisioning.
fn checks(host, cfg) {
    let mise = executable(cfg);
    [#{ name: "mise app-scoped use and exec", kind: "read-only", host: host,
        command: mise + " use --help | grep -q -- --path" + " && " +
            mise + " use --help | grep -q -- --pin" + " && " + mise + " exec --help >/dev/null" }]
}
