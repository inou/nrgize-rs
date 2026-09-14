// Versioned directories for any artifact/toolchain. Hooks are caller-owned shell commands.
// The root must be an application-owned directory on a POSIX host.
private fn path(root, version) {
    if !root.starts_with("/") || root == "/" || root.ends_with("/") {
        throw "release root must be an absolute non-root path without a trailing slash";
    }
    for part in root.split("/") {
        if part == "." || part == ".." { throw "release root cannot contain dot components"; }
    }
    if version == "" || version == "." || version == ".." { throw "invalid release version"; }
    for ch in version.chars() {
        if !(ch >= 'a' && ch <= 'z') && !(ch >= 'A' && ch <= 'Z') &&
           !(ch >= '0' && ch <= '9') && ch != '.' && ch != '_' && ch != '-' {
            throw "release version may contain only letters, digits, dot, underscore and dash";
        }
    }
    root + "/releases/" + version
}

private fn validate(cfg) {
    if in_transaction() { throw "release lifecycle cannot be nested in a transaction"; }
    for key in ["activate", "health"] {
        if !cfg.contains(key) || type_of(cfg[key]) != "string" || cfg[key] == "" {
            throw "release requires a nonempty " + key + " command";
        }
    }
}

private fn hook(host, name, cmd, cwd, root, directory, previous, cfg) {
    let vars = if cfg.contains("env") { cfg.env } else { #{} };
    vars.NRG_RELEASE_DIR = directory;
    vars.NRG_CURRENT_LINK = root + "/current";
    vars.NRG_PREVIOUS_RELEASE = previous;
    ssh_step(host, name, cmd, #{ cwd: cwd, env: vars,
        timeout_secs: if cfg.contains("timeout_secs") { cfg.timeout_secs } else { 600 } });
}

// mv's explicit no-dereference options differ across GNU and BSD; never use plain mv on a
// destination symlink to a directory. The temporary symlink lives on the same filesystem.
private fn switch_link(host, root, directory) {
    let command = "set -eu; d=$(mktemp -d " + sh_quote(root + "/.nrg-link.XXXXXXXXXX") +
        "); trap 'rm -rf \"$d\"' 0; trap 'exit 130' HUP INT TERM; ln -s " + sh_quote(directory) +
        " \"$d/current\"; mv -fT \"$d/current\" " + sh_quote(root + "/current") +
        " 2>/dev/null || mv -fh \"$d/current\" " + sh_quote(root + "/current");
    ssh_step(host, "Switch current release", command);
}

private fn previous(host, root) {
    if is_dry_run() { return "<execution-unverified previous release>"; }
    let link = sh_quote(root + "/current");
    let r = ssh_probe(host, "if [ -L " + link + " ]; then readlink " + link +
        "; elif [ -e " + link + " ]; then echo 'current must be a symlink' >&2; exit 1; fi");
    if !r.ok { throw "Cannot inspect current release: " + r.stderr; }
    let value = r.stdout;
    value.trim();
    if value != "" && !value.starts_with(root + "/releases/") {
        throw "current must reference an absolute release path beneath this root";
    }
    if value != "" {
        let components = value.split("/");
        if path(root, components[components.len() - 1]) != value {
            throw "current must reference a single release version beneath this root";
        }
    }
    value
}

private fn activate(host, root, directory, old, cfg) {
    transaction(|| {
        on_rollback(|| {
            if old == "" {
                ssh_step(host, "Remove failed first release link", "rm -f " + sh_quote(root + "/current"));
                if cfg.contains("deactivate") && cfg.deactivate != "" {
                    hook(host, "Deactivate failed first release", cfg.deactivate, root, root, directory, old, cfg);
                }
            } else {
                switch_link(host, root, old);
                hook(host, "Reactivate previous release", cfg.activate, root, root, old, directory, cfg);
                hook(host, "Check previous release", cfg.health, root, root, old, directory, cfg);
            }
        });
        switch_link(host, root, directory);
        hook(host, "Activate release", cfg.activate, root, root, directory, old, cfg);
        hook(host, "Check release", cfg.health, root, root, directory, old, cfg);
    });
}

fn deploy(host, root, version, cfg) {
    let directory = path(root, version);
    validate(cfg);
    if !cfg.contains("prepare") || type_of(cfg.prepare) != "string" || cfg.prepare == "" {
        throw "release requires a nonempty prepare command";
    }
    ssh_step(host, "Prepare release root", "mkdir -p " + sh_quote(root + "/releases"));
    let lock = root + "/.nrg-release-lock";
    remote_lock_acquire(host, lock);
    try {
        let old = previous(host, root);
        // Exclusive mkdir prevents accidental rebuilding of a previously activated version.
        ssh_step(host, "Create immutable release directory", "mkdir " + sh_quote(directory));
        hook(host, "Prepare release", cfg.prepare, directory, root, directory, old, cfg);
        for key in ["build", "migrate"] {
            if cfg.contains(key) && cfg[key] != "" {
                hook(host, "Release " + key, cfg[key], directory, root, directory, old, cfg);
            }
        }
        activate(host, root, directory, old, cfg);
    } catch (e) {
        remote_lock_release(host, lock);
        throw e;
    }
    remote_lock_release(host, lock);
}

// Explicit version: never infer which release is safe to restore from a partial run.
fn rollback(host, root, version, cfg) {
    let directory = path(root, version);
    validate(cfg);
    let lock = root + "/.nrg-release-lock";
    remote_lock_acquire(host, lock);
    try {
        let old = previous(host, root);
        ssh_step(host, "Verify rollback directory", "test -d " + sh_quote(directory));
        activate(host, root, directory, old, cfg);
    } catch (e) {
        remote_lock_release(host, lock);
        throw e;
    }
    remote_lock_release(host, lock);
}
