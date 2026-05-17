# healthcheck.star — Health check polling for Energize deployments.
#
# Provides configurable retry loops for verifying that a service is up
# after deployment. Supports both HTTP health endpoints and TCP port checks.
#
# Usage:
#   load("lib/healthcheck.star", "wait_healthy", "wait_port")

load("lib/runtime.star", "container_cmd")

# ---------------------------------------------------------------------------
# HTTP health check
# ---------------------------------------------------------------------------

def wait_healthy(url, attempts = 30, interval = 2, expected_status = 200):
    """Poll an HTTP endpoint until it returns the expected status code.

    Args:
        url:             Full URL to poll, e.g. "http://10.0.0.1:3000/up"
        attempts:        Maximum number of attempts before failing.
        interval:        Seconds between attempts.
        expected_status: HTTP status code that means "healthy" (default 200).

    Returns:
        The successful HttpResponse.

    Raises:
        fail() if all attempts are exhausted.
    """
    for i in range(attempts):
        r = http_get(url)
        if r.status == expected_status:
            print("  health check passed (" + str(i + 1) + "/" + str(attempts) + ")")
            return r
        if i < attempts - 1:
            sleep(interval)

    fail("Health check failed after " + str(attempts) + " attempts: " + url +
         " (last status: " + str(r.status) + ")")

# ---------------------------------------------------------------------------
# TCP port check (via SSH)
# ---------------------------------------------------------------------------

def wait_port(host, port, attempts = 30, interval = 2):
    """Wait for a TCP port to be open on a remote host.

    Uses `nc -z` via SSH to check port availability. Useful when you
    need to verify a service is listening before routing traffic to it.

    Args:
        host:     SSH host.
        port:     Port number to check.
        attempts: Maximum number of attempts.
        interval: Seconds between attempts.

    Returns:
        True when port is open.

    Raises:
        fail() if all attempts are exhausted.
    """
    for i in range(attempts):
        r = ssh_exec(host, "nc -z localhost " + str(port) + " 2>/dev/null")
        if r.ok:
            print("  port " + str(port) + " open on " + host + " (" + str(i + 1) + "/" + str(attempts) + ")")
            return True
        if i < attempts - 1:
            sleep(interval)

    fail("Port " + str(port) + " not open on " + host + " after " + str(attempts) + " attempts")

# ---------------------------------------------------------------------------
# Container health check (via docker inspect)
# ---------------------------------------------------------------------------

def wait_container_healthy(host, name, attempts = 30, interval = 2):
    """Wait for a container's HEALTHCHECK to pass.

    Requires the container to have a HEALTHCHECK instruction in its Dockerfile/Containerfile.

    Args:
        host:     SSH host.
        name:     Container name.
        attempts: Maximum number of attempts.
        interval: Seconds between attempts.

    Returns:
        True when container reports "healthy".

    Raises:
        fail() if all attempts are exhausted.
    """
    for i in range(attempts):
        r = ssh_exec(host, container_cmd() + " inspect -f '{{.State.Health.Status}}' " + name + " 2>/dev/null")
        if r.ok and r.stdout.strip() == "healthy":
            print("  container " + name + " healthy on " + host + " (" + str(i + 1) + "/" + str(attempts) + ")")
            return True
        if i < attempts - 1:
            sleep(interval)

    fail("Container " + name + " not healthy on " + host + " after " + str(attempts) + " attempts")

# ---------------------------------------------------------------------------
# Multi-host health check
# ---------------------------------------------------------------------------

def wait_healthy_all(hosts, port, path = "/up", attempts = 30, interval = 2):
    """Wait for a service to be healthy on all hosts.

    Checks each host sequentially. Constructs URL as http://host:port/path.

    Args:
        hosts:    List of SSH hosts.
        port:     Service port.
        path:     Health endpoint path.
        attempts: Max attempts per host.
        interval: Seconds between attempts.
    """
    for host in hosts:
        url = "http://" + host + ":" + str(port) + path
        print("  checking " + host + "...")
        wait_healthy(url, attempts, interval)
