# runtime.star — Container runtime abstraction for Energize.
#
# Supports Docker, Podman, and OrbStack (which uses the docker CLI).
# All other library modules (docker.star, proxy.star, deploy.star) use
# this module to get the container CLI command, so you only configure
# the runtime in one place.
#
# Usage in your Energize.star:
#
#   load("lib/runtime.star", "set_runtime", "CONTAINER_CMD")
#
#   # Use Podman everywhere:
#   set_runtime("podman")
#
#   # Or auto-detect:
#   set_runtime("auto")
#
#   # Then proceed normally — all lib/ functions use the configured runtime.
#   load("lib/deploy.star", "deploy")
#   deploy(...)
#
# Supported runtimes:
#   "docker"  — Docker CE/EE (default). Also works with OrbStack,
#               Rancher Desktop, colima, or any Docker-compatible runtime.
#   "podman"  — Podman (rootful or rootless).
#   "nerdctl" — nerdctl (containerd). Experimental.
#   "auto"    — Auto-detect: tries docker, then podman, then nerdctl.

# ---------------------------------------------------------------------------
# Runtime state — mutable via set_runtime()
# ---------------------------------------------------------------------------

# The container CLI command. Default is "docker".
# All library modules should call container_cmd() to get this value.
_RUNTIME = {"cmd": "docker", "name": "docker"}

def set_runtime(runtime = "auto"):
    """Set the container runtime for all Energize operations.

    Call this at the top of your Energize.star, BEFORE loading other
    library modules that use the runtime.

    Args:
        runtime: One of "docker", "podman", "nerdctl", "auto".
                 "auto" probes the local system for available runtimes.
    """
    if runtime == "auto":
        _auto_detect()
    elif runtime in ("docker", "podman", "nerdctl"):
        _RUNTIME["cmd"] = runtime
        _RUNTIME["name"] = runtime
    else:
        fail("Unknown container runtime: " + runtime + ". Use docker, podman, nerdctl, or auto.")

    print("[nrg] container runtime: " + _RUNTIME["name"] + " (" + _RUNTIME["cmd"] + ")")

def container_cmd():
    """Return the container CLI command (e.g. 'docker' or 'podman').

    All library modules should use this instead of hardcoding 'docker'.
    """
    return _RUNTIME["cmd"]

def runtime_name():
    """Return the human-readable runtime name."""
    return _RUNTIME["name"]

def is_podman():
    """Return True if the runtime is Podman."""
    return _RUNTIME["name"] == "podman"

def is_docker():
    """Return True if the runtime is Docker (or Docker-compatible like OrbStack)."""
    return _RUNTIME["name"] == "docker"

# ---------------------------------------------------------------------------
# Auto-detection
# ---------------------------------------------------------------------------

def _auto_detect():
    """Probe the local system for available container runtimes."""
    # Try docker first (covers Docker CE, OrbStack, colima, Rancher Desktop)
    r = local_exec("docker info --format '{{.ID}}' 2>/dev/null")
    if r.ok:
        _RUNTIME["cmd"] = "docker"
        _RUNTIME["name"] = "docker"
        # Check if it's actually OrbStack
        r2 = local_exec("docker info --format '{{.OperatingSystem}}' 2>/dev/null")
        if r2.ok and "orbstack" in r2.stdout.strip().lower():
            _RUNTIME["name"] = "orbstack"
        return

    # Try podman
    r = local_exec("podman info --format '{{.Host.OCIRuntime.Name}}' 2>/dev/null")
    if r.ok:
        _RUNTIME["cmd"] = "podman"
        _RUNTIME["name"] = "podman"
        return

    # Try nerdctl
    r = local_exec("nerdctl info 2>/dev/null")
    if r.ok:
        _RUNTIME["cmd"] = "nerdctl"
        _RUNTIME["name"] = "nerdctl"
        return

    fail("No container runtime found. Install Docker, Podman, or nerdctl.")

# ---------------------------------------------------------------------------
# Runtime-specific helpers
# ---------------------------------------------------------------------------

def runtime_run_flags():
    """Return extra flags needed for `run` on the current runtime.

    Podman rootless needs --userns=keep-id for bind mounts, and doesn't
    support --restart unless-stopped by default (uses quadlet/systemd).
    """
    if is_podman():
        return "--restart unless-stopped"
    return "--restart unless-stopped"

def runtime_login_cmd(server, username, password):
    """Build a registry login command for the current runtime.

    Podman and Docker both support --password-stdin.
    """
    cmd = container_cmd()
    return "echo '" + password + "' | " + cmd + " login " + server + " -u " + username + " --password-stdin"

def runtime_exec_cmd(container_name, command):
    """Build a container exec command for the current runtime."""
    return container_cmd() + " exec " + container_name + " " + command
