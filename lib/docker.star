# docker.star — Container lifecycle helpers for Energize deployments.
#
# Provides build, push, pull, run, stop, remove, cleanup, and container
# inspection primitives. Works with Docker, Podman, OrbStack, or nerdctl
# via the runtime abstraction layer.
#
# Usage:
#   load("lib/docker.star", "docker_build", "docker_run", "docker_stop")
#
# All remote operations use ssh_exec under the hood.
# The container CLI command is resolved via lib/runtime.star.

load("lib/runtime.star", "container_cmd", "is_podman", "runtime_run_flags")

# ---------------------------------------------------------------------------
# Build & Push (local)
# ---------------------------------------------------------------------------

def docker_build(tag, context = ".", dockerfile = "Dockerfile", build_args = {}):
    """Build a container image locally.

    Args:
        tag:        Full image tag, e.g. "registry.example.com/app:v42"
        context:    Build context path (default ".")
        dockerfile: Dockerfile path relative to context
        build_args: Dict of --build-arg key=value pairs

    Returns:
        ExecResult from the build command.
    """
    args = " ".join(["--build-arg " + k + "=" + v for k, v in build_args.items()])
    cmd = container_cmd() + " build -t " + tag + " -f " + dockerfile + " " + args + " " + context
    r = local_exec(cmd)
    if not r.ok:
        fail(container_cmd() + " build failed:\n" + r.stderr)
    return r

def docker_push(tag):
    """Push an image to the registry from the local machine.

    Args:
        tag: Full image tag to push.

    Returns:
        ExecResult from the push command.
    """
    r = local_exec(container_cmd() + " push " + tag)
    if not r.ok:
        fail(container_cmd() + " push failed:\n" + r.stderr)
    return r

# ---------------------------------------------------------------------------
# Pull (remote)
# ---------------------------------------------------------------------------

def docker_pull(host, tag):
    """Pull an image on a remote host.

    Args:
        host: SSH host.
        tag:  Full image tag to pull.

    Returns:
        ExecResult from the pull command.
    """
    r = ssh_exec(host, container_cmd() + " pull " + tag)
    if not r.ok:
        fail(container_cmd() + " pull on " + host + " failed:\n" + r.stderr)
    return r

def docker_pull_all(hosts, tag):
    """Pull an image on all hosts in parallel.

    Args:
        hosts: List of SSH hosts.
        tag:   Full image tag to pull.

    Returns:
        List of ExecResult.
    """
    results = ssh_exec_all(hosts, container_cmd() + " pull " + tag)
    failed = [r for r in results if not r.ok]
    if failed:
        fail(container_cmd() + " pull failed on: " + ", ".join([r.host for r in failed]))
    return results

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

def docker_run(host, tag, name, ports = {}, envs = {}, volumes = {}, network = "", extra = ""):
    """Run a container on a remote host.

    Args:
        host:    SSH host.
        tag:     Image tag.
        name:    Container name.
        ports:   Dict of host_port: container_port mappings.
        envs:    Dict of environment variables.
        volumes: Dict of host_path: container_path bind mounts.
        network: Container network to connect to.
        extra:   Additional run flags or override command.

    Returns:
        ExecResult from the run command.
    """
    parts = [container_cmd() + " run -d " + runtime_run_flags()]
    parts.append("--name " + name)

    for hp, cp in ports.items():
        parts.append("-p " + str(hp) + ":" + str(cp))
    for k, v in envs.items():
        parts.append("-e " + k + "=" + v)
    for hp, cp in volumes.items():
        parts.append("-v " + hp + ":" + cp)
    if network:
        parts.append("--network " + network)
    if extra:
        parts.append(extra)

    parts.append(tag)
    cmd = " ".join(parts)
    return ssh_exec(host, cmd)

# ---------------------------------------------------------------------------
# Stop / Remove / Rename
# ---------------------------------------------------------------------------

def docker_stop(host, name, timeout = 30):
    """Stop a running container on a remote host.

    Args:
        host:    SSH host.
        name:    Container name.
        timeout: Seconds to wait before SIGKILL.

    Returns:
        ExecResult (may have non-zero exit if container doesn't exist).
    """
    return ssh_exec(host, container_cmd() + " stop -t " + str(timeout) + " " + name + " 2>/dev/null || true")

def docker_remove(host, name):
    """Remove a container on a remote host.

    Args:
        host: SSH host.
        name: Container name.

    Returns:
        ExecResult.
    """
    return ssh_exec(host, container_cmd() + " rm -f " + name + " 2>/dev/null || true")

def docker_rename(host, old_name, new_name):
    """Rename a container on a remote host.

    Args:
        host:     SSH host.
        old_name: Current container name.
        new_name: New container name.

    Returns:
        ExecResult.
    """
    return ssh_exec(host, container_cmd() + " rename " + old_name + " " + new_name + " 2>/dev/null || true")

# ---------------------------------------------------------------------------
# Inspection
# ---------------------------------------------------------------------------

def docker_container_running(host, name):
    """Check if a container is running on a remote host.

    Args:
        host: SSH host.
        name: Container name.

    Returns:
        True if the container exists and is running.
    """
    r = ssh_exec(host, container_cmd() + " inspect -f '{{.State.Running}}' " + name + " 2>/dev/null")
    return r.ok and r.stdout.strip() == "true"

def docker_image_id(host, tag):
    """Get the image ID for a tag on a remote host.

    Returns:
        Image ID string, or empty string if not found.
    """
    r = ssh_exec(host, container_cmd() + " image inspect -f '{{.Id}}' " + tag + " 2>/dev/null")
    if r.ok:
        return r.stdout.strip()
    return ""

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

def docker_cleanup(host, keep_images = 3):
    """Clean up old containers and dangling images on a remote host.

    Args:
        host:        SSH host.
        keep_images: Number of recent images to keep (not used yet — prunes dangling).

    Returns:
        ExecResult from system prune.
    """
    _cmd = container_cmd()
    # Remove exited containers
    ssh_exec(host, _cmd + " container prune -f 2>/dev/null || true")
    # Remove dangling images
    return ssh_exec(host, _cmd + " image prune -f 2>/dev/null || true")

# ---------------------------------------------------------------------------
# Exec into container
# ---------------------------------------------------------------------------

def docker_exec(host, name, cmd):
    """Execute a command inside a running container on a remote host.

    Args:
        host: SSH host.
        name: Container name.
        cmd:  Command to run inside the container.

    Returns:
        ExecResult.
    """
    return ssh_exec(host, container_cmd() + " exec " + name + " " + cmd)

# ---------------------------------------------------------------------------
# Logs
# ---------------------------------------------------------------------------

def docker_logs(host, name, tail = 100):
    """Get recent logs from a container on a remote host.

    Args:
        host: SSH host.
        name: Container name.
        tail: Number of lines to return.

    Returns:
        ExecResult with stdout containing the logs.
    """
    return ssh_exec(host, container_cmd() + " logs --tail " + str(tail) + " " + name + " 2>&1")
