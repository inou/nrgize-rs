# deploy.star — Zero-downtime deployment orchestration for Energize.
#
# This module implements the full Kamal-style deploy lifecycle:
#
#   1. Build image locally
#   2. Push to registry
#   3. Pull on all hosts
#   4. For each host (rolling):
#      a. Start new container with a unique name
#      b. Wait for health check
#      c. Tell proxy to switch traffic (zero-downtime)
#      d. Stop old container
#      e. Clean up
#
# Usage:
#   load("lib/deploy.star", "deploy", "rollback")
#
# Or use individual phases:
#   load("lib/deploy.star", "deploy_to_host", "build_and_push")

load("lib/docker.star",
    "docker_build", "docker_push", "docker_pull_all",
    "docker_run", "docker_stop", "docker_remove",
    "docker_rename", "docker_container_running",
    "docker_cleanup", "docker_logs")
load("lib/proxy.star",
    "proxy_boot", "proxy_deploy", "proxy_remove")
load("lib/healthcheck.star",
    "wait_healthy", "wait_port")

# ---------------------------------------------------------------------------
# Full deploy pipeline
# ---------------------------------------------------------------------------

def deploy(
    hosts,
    image,
    service,
    container_port = 3000,
    envs = {},
    volumes = {},
    health_path = "/up",
    health_attempts = 30,
    health_interval = 2,
    build_context = ".",
    dockerfile = "Dockerfile",
    build_args = {},
    skip_build = False,
    skip_push = False,
    network = "",
    pre_deploy_cmd = "",
    post_deploy_cmd = "",
):
    """Full zero-downtime deploy to one or more hosts.

    This is the main entry point — the Kamal-equivalent workflow.

    Args:
        hosts:            List of SSH hosts to deploy to.
        image:            Full image tag, e.g. "ghcr.io/org/app:v42".
        service:          Service name for proxy routing.
        container_port:   Port the app listens on inside the container.
        envs:             Dict of environment variables for the container.
        volumes:          Dict of host_path:container_path mounts.
        health_path:      HTTP path for health checks (relative to container_port).
        health_attempts:  Max health check attempts.
        health_interval:  Seconds between health check attempts.
        build_context:    Docker build context path.
        dockerfile:       Dockerfile path.
        build_args:       Dict of Docker build args.
        skip_build:       Skip build step (use pre-built image).
        skip_push:        Skip push step (image already in registry).
        network:          Docker network for the container.
        pre_deploy_cmd:   Command to run on each host before deploy.
        post_deploy_cmd:  Command to run on each host after deploy.
    """
    version = _extract_version(image)

    print("==> Deploying " + service + " " + version + " to " + str(len(hosts)) + " host(s)")

    # Phase 1: Build & push
    if not skip_build:
        print("\n--- Build ---")
        docker_build(image, build_context, dockerfile, build_args)

    if not skip_push:
        print("\n--- Push ---")
        docker_push(image)

    # Phase 2: Pull on all hosts in parallel
    print("\n--- Pull ---")
    docker_pull_all(hosts, image)

    # Phase 3: Ensure proxy is running on all hosts
    print("\n--- Proxy ---")
    for host in hosts:
        proxy_boot(host)

    # Phase 4: Rolling deploy
    print("\n--- Deploy ---")
    for host in hosts:
        print("\n  [" + host + "]")
        deploy_to_host(
            host = host,
            image = image,
            service = service,
            version = version,
            container_port = container_port,
            envs = envs,
            volumes = volumes,
            health_path = health_path,
            health_attempts = health_attempts,
            health_interval = health_interval,
            network = network,
            pre_deploy_cmd = pre_deploy_cmd,
            post_deploy_cmd = post_deploy_cmd,
        )

    # Phase 5: Save deploy state
    state_set(service + ".version", version)
    state_set(service + ".image", image)
    state_set(service + ".deployed_at", _timestamp())

    print("\n==> Deploy complete: " + service + " " + version)

# ---------------------------------------------------------------------------
# Single-host deploy (used by rolling deploy, also callable directly)
# ---------------------------------------------------------------------------

def deploy_to_host(
    host, image, service, version = "",
    container_port = 3000, envs = {}, volumes = {},
    health_path = "/up", health_attempts = 30, health_interval = 2,
    network = "", pre_deploy_cmd = "", post_deploy_cmd = "",
):
    """Deploy to a single host with zero-downtime container swap.

    Steps:
      1. Run pre-deploy command (if any)
      2. Start new container as service-version
      3. Wait for health check on the new container
      4. Switch proxy traffic to the new container
      5. Stop the old container
      6. Rename new container to the canonical name
      7. Run post-deploy command (if any)
      8. Clean up old images/containers

    Args:
        (same as deploy() for the relevant subset)
    """
    if not version:
        version = _extract_version(image)

    canonical_name = service + "-web"
    new_name = service + "-web-" + version
    old_name = service + "-web-old"

    # Pre-deploy hook
    if pre_deploy_cmd:
        print("    pre-deploy: " + pre_deploy_cmd)
        r = ssh_exec(host, pre_deploy_cmd)
        if not r.ok:
            fail("Pre-deploy command failed on " + host + ":\n" + r.stderr)

    # Find a free host port for the new container.
    # We use a deterministic offset so we don't collide with the running instance.
    new_host_port = _pick_port(host, container_port)

    # Start new container
    print("    starting " + new_name + " on port " + str(new_host_port))
    r = docker_run(
        host = host,
        tag = image,
        name = new_name,
        ports = {str(new_host_port): str(container_port)},
        envs = envs,
        volumes = volumes,
        network = network,
    )
    if not r.ok:
        fail("Failed to start " + new_name + " on " + host + ":\n" + r.stderr)

    # Health check on the new container
    print("    waiting for health check...")
    health_url = "http://" + host + ":" + str(new_host_port) + health_path
    wait_healthy(health_url, health_attempts, health_interval)

    # Switch proxy traffic
    print("    switching traffic via proxy...")
    target = "localhost:" + str(new_host_port)
    proxy_deploy(host, service, target, health_path)

    # Stop old container
    print("    stopping old container...")
    docker_stop(host, canonical_name)
    docker_rename(host, canonical_name, old_name)
    docker_rename(host, new_name, canonical_name)
    docker_remove(host, old_name)

    # Post-deploy hook
    if post_deploy_cmd:
        print("    post-deploy: " + post_deploy_cmd)
        ssh_exec(host, post_deploy_cmd)

    # Cleanup
    docker_cleanup(host)
    print("    done.")

# ---------------------------------------------------------------------------
# Rollback
# ---------------------------------------------------------------------------

def rollback(hosts, service, image = ""):
    """Roll back to the previous version.

    If no image is specified, looks up the previous version from state.

    Args:
        hosts:   List of SSH hosts.
        service: Service name.
        image:   Image to roll back to. If empty, uses last known version.
    """
    if not image:
        image = state_get(service + ".rollback_image")
        if not image:
            fail("No rollback image found for " + service + ". Specify one explicitly.")

    print("==> Rolling back " + service + " to " + image)

    # Save current as rollback target before we overwrite
    current = state_get(service + ".image")
    if current:
        state_set(service + ".rollback_image", current)

    deploy(hosts = hosts, image = image, service = service, skip_build = True, skip_push = True)

# ---------------------------------------------------------------------------
# Accessories (databases, redis, etc. — long-lived containers)
# ---------------------------------------------------------------------------

def accessory_run(host, name, image, ports = {}, envs = {}, volumes = {}, network = "", cmd = ""):
    """Start an accessory container (database, cache, etc.).

    Accessories are long-running containers that aren't part of the
    rolling deploy cycle. They're started once and left running.

    Args:
        host:    SSH host.
        name:    Container name, e.g. "app-db" or "app-redis".
        image:   Image tag.
        ports:   Port mappings.
        envs:    Environment variables.
        volumes: Volume mounts.
        network: Docker network.
        cmd:     Override command.
    """
    # Check if already running
    if docker_container_running(host, name):
        print("  accessory " + name + " already running on " + host)
        return

    print("  starting accessory " + name + " on " + host)
    extra = ""
    if cmd:
        extra = cmd
    r = docker_run(host, image, name, ports, envs, volumes, network, extra)
    if not r.ok:
        fail("Failed to start accessory " + name + " on " + host + ":\n" + r.stderr)

# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

def _extract_version(image):
    """Extract version tag from an image string. Falls back to 'latest'."""
    if ":" in image:
        parts = image.split(":")
        return parts[len(parts) - 1]
    return "latest"

def _timestamp():
    """Get current UTC timestamp string."""
    r = local_exec("date -u '+%Y-%m-%d %H:%M:%S UTC'")
    return r.stdout.strip()

def _pick_port(host, base_port):
    """Pick an available host port near the base port.

    Tries base_port + 10000, then increments until finding a free one.
    This avoids collision with the currently-running container.
    """
    candidate = base_port + 10000
    for _ in range(100):
        r = ssh_exec(host, "nc -z localhost " + str(candidate) + " 2>/dev/null; echo $?")
        # nc -z returns 1 if port is NOT in use (connection refused)
        if r.ok and r.stdout.strip() == "1":
            return candidate
        candidate = candidate + 1
    # Fallback — just use the offset port and hope for the best
    return base_port + 10000
