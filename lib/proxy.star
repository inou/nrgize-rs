# proxy.star — kamal-proxy management for zero-downtime deployments.
#
# kamal-proxy is a lightweight reverse proxy that supports zero-downtime
# deployments by draining connections to the old container before switching
# traffic to the new one. This module wraps its CLI.
#
# If you're not using kamal-proxy, you can write a similar module for
# traefik, caddy, nginx, or any other reverse proxy.
#
# Works with Docker, Podman, OrbStack, or nerdctl via the runtime
# abstraction layer.
#
# Usage:
#   load("lib/proxy.star", "proxy_deploy", "proxy_boot")

load("lib/runtime.star", "container_cmd", "runtime_run_flags")

KAMAL_PROXY_IMAGE = "basecamp/kamal-proxy:latest"
KAMAL_PROXY_CONTAINER = "kamal-proxy"

# ---------------------------------------------------------------------------
# Boot / Install
# ---------------------------------------------------------------------------

def proxy_boot(host, http_port = 80, https_port = 443):
    """Ensure kamal-proxy is running on a host.

    Pulls the latest image and starts the proxy if not already running.
    If already running, this is a no-op.

    Args:
        host:       SSH host.
        http_port:  Host port for HTTP traffic.
        https_port: Host port for HTTPS traffic.
    """
    _cmd = container_cmd()
    r = ssh_exec(host, _cmd + " inspect -f '{{.State.Running}}' " + KAMAL_PROXY_CONTAINER + " 2>/dev/null")
    if r.ok and r.stdout.strip() == "true":
        return  # Already running

    # Pull latest proxy image
    ssh_exec(host, _cmd + " pull " + KAMAL_PROXY_IMAGE)

    # Remove any stopped proxy container
    ssh_exec(host, _cmd + " rm -f " + KAMAL_PROXY_CONTAINER + " 2>/dev/null || true")

    # Start proxy with host networking for port 80/443
    cmd = " ".join([
        _cmd + " run -d",
        "--name " + KAMAL_PROXY_CONTAINER,
        runtime_run_flags(),
        "--network host",
        "-v kamal-proxy-config:/home/kamal-proxy/.config/kamal-proxy",
        KAMAL_PROXY_IMAGE,
    ])
    r = ssh_exec(host, cmd)
    if not r.ok:
        fail("Failed to start kamal-proxy on " + host + ":\n" + r.stderr)

def proxy_boot_all(hosts, http_port = 80, https_port = 443):
    """Boot kamal-proxy on all hosts."""
    for host in hosts:
        proxy_boot(host, http_port, https_port)

# ---------------------------------------------------------------------------
# Deploy (zero-downtime traffic switch)
# ---------------------------------------------------------------------------

def proxy_deploy(host, service, target, health_path = "/up", buffer_requests = True, buffer_timeout = 30):
    """Deploy a new target through kamal-proxy with zero-downtime.

    This tells kamal-proxy to route traffic for `service` to `target`,
    draining old connections gracefully.

    Args:
        host:            SSH host where kamal-proxy runs.
        service:         Service name (used as the virtual host identifier).
        target:          Target in host:port format, e.g. "localhost:3000".
        health_path:     Health check endpoint path.
        buffer_requests: If True, buffer incoming requests during switchover.
        buffer_timeout:  Max seconds to buffer requests during deploy.

    Returns:
        ExecResult from kamal-proxy deploy command.
    """
    parts = [
        container_cmd() + " exec " + KAMAL_PROXY_CONTAINER,
        "kamal-proxy deploy",
        service,
        "--target " + target,
    ]
    if health_path:
        parts.append("--health-check-path " + health_path)
    if buffer_requests:
        parts.append("--buffer-requests")
        parts.append("--buffer-timeout " + str(buffer_timeout) + "s")

    cmd = " ".join(parts)
    r = ssh_exec(host, cmd)
    if not r.ok:
        fail("kamal-proxy deploy failed on " + host + " for " + service + ":\n" + r.stderr)
    return r

# ---------------------------------------------------------------------------
# Remove service
# ---------------------------------------------------------------------------

def proxy_remove(host, service):
    """Remove a service from kamal-proxy routing.

    Args:
        host:    SSH host.
        service: Service name to remove.

    Returns:
        ExecResult.
    """
    cmd = container_cmd() + " exec " + KAMAL_PROXY_CONTAINER + " kamal-proxy remove " + service
    return ssh_exec(host, cmd)

# ---------------------------------------------------------------------------
# TLS / Let's Encrypt
# ---------------------------------------------------------------------------

def proxy_set_tls(host, service, domain):
    """Configure automatic TLS for a service via Let's Encrypt.

    Args:
        host:    SSH host.
        service: Service name.
        domain:  Domain to get a certificate for.

    Returns:
        ExecResult.
    """
    cmd = " ".join([
        container_cmd() + " exec " + KAMAL_PROXY_CONTAINER,
        "kamal-proxy deploy",
        service,
        "--host " + domain,
        "--tls",
    ])
    return ssh_exec(host, cmd)

# ---------------------------------------------------------------------------
# Status / Info
# ---------------------------------------------------------------------------

def proxy_list(host):
    """List services registered with kamal-proxy.

    Args:
        host: SSH host.

    Returns:
        ExecResult with stdout containing service listing.
    """
    return ssh_exec(host, container_cmd() + " exec " + KAMAL_PROXY_CONTAINER + " kamal-proxy list")

def proxy_stop(host):
    """Stop and remove the kamal-proxy container.

    Args:
        host: SSH host.
    """
    _cmd = container_cmd()
    ssh_exec(host, _cmd + " stop " + KAMAL_PROXY_CONTAINER + " 2>/dev/null || true")
    ssh_exec(host, _cmd + " rm " + KAMAL_PROXY_CONTAINER + " 2>/dev/null || true")
