// Pure declarations: these helpers never execute a command or inspect a host.
fn step(name, command) { #{ name: name, run: command } }

fn scenario(name, steps, checks) {
    #{ name: name, steps: steps, checks: checks }
}

// A failure must be the declared exit code, followed by successful recovery checks.
// nrg enables fault scenarios only when --faults is explicitly selected.
fn rejects(name, command, exit_code, checks) {
    if exit_code < 1 || exit_code > 125 { throw "rejects requires an expected exit code in 1..125"; }
    #{ name: name, fault: true,
       steps: [#{ name: name, run: command, expect_exit: exit_code }], checks: checks }
}

fn fault(name, steps, checks) {
    #{ name: name, fault: true, steps: steps, checks: checks }
}

// Common service lifecycle contract. Commands and observable checks remain app-owned.
// Scenarios run in order in one disposable workspace; repeat uses the exact same deploy.
fn service(cfg) {
    for key in cfg.keys() {
        if !["name", "setup", "deploy", "restart", "checks", "cleanup", "faults", "env"].contains(key) {
            throw "unknown service contract key: " + key;
        }
    }
    let result = #{ name: cfg.name,
        setup: if cfg.contains("setup") { cfg.setup } else { [] },
        env: if cfg.contains("env") { cfg.env } else { #{} },
        cleanup: cfg.cleanup,
        scenarios: [
            scenario("first install", [cfg.deploy], cfg.checks),
            scenario("repeat install", [cfg.deploy], cfg.checks),
            scenario("restart", [cfg.restart], cfg.checks)
        ]
    };
    if cfg.contains("faults") {
        for item in cfg.faults {
            item.fault = true;
            result.scenarios.push(item);
        }
    }
    result
}
