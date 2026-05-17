# registry.star — Container registry authentication helpers.
#
# Handles logging into container registries on local and remote hosts.
# Supports Docker Hub, GitHub Container Registry, AWS ECR, and
# generic registries. Works with Docker, Podman, OrbStack, or nerdctl
# via the runtime abstraction layer.
#
# Usage:
#   load("lib/registry.star", "registry_login", "registry_login_all")

load("lib/runtime.star", "container_cmd")

# ---------------------------------------------------------------------------
# Login
# ---------------------------------------------------------------------------

def registry_login(host, server, username, password):
    """Log into a container registry on a remote host.

    Args:
        host:     SSH host (use "local" for the build machine).
        server:   Registry server URL, e.g. "ghcr.io", "registry.example.com".
        username: Registry username.
        password: Registry password or token.

    Returns:
        ExecResult from the login command.
    """
    cmd = "echo '" + password + "' | " + container_cmd() + " login " + server + " -u " + username + " --password-stdin"
    if host == "local":
        r = local_exec(cmd)
    else:
        r = ssh_exec(host, cmd)
    if not r.ok:
        fail("Registry login failed on " + host + " for " + server + ":\n" + r.stderr)
    return r

def registry_login_all(hosts, server, username, password):
    """Log into a container registry on all remote hosts.

    Args:
        hosts:    List of SSH hosts.
        server:   Registry server URL.
        username: Registry username.
        password: Registry password or token.
    """
    for host in hosts:
        registry_login(host, server, username, password)

# ---------------------------------------------------------------------------
# AWS ECR helpers
# ---------------------------------------------------------------------------

def ecr_login(host, region = "us-east-1", account_id = ""):
    """Log into AWS ECR on a remote host using the AWS CLI.

    Requires `aws` CLI to be available on the target host.

    Args:
        host:       SSH host (use "local" for build machine).
        region:     AWS region.
        account_id: AWS account ID. If empty, auto-detected from caller identity.

    Returns:
        ExecResult.
    """
    _cmd = container_cmd()
    if account_id:
        server = account_id + ".dkr.ecr." + region + ".amazonaws.com"
    else:
        server = ""  # Let aws ecr get-login-password handle it

    login_cmd = "aws ecr get-login-password --region " + region + " | " + _cmd + " login --username AWS --password-stdin "
    if server:
        login_cmd = login_cmd + server
    else:
        # Auto-detect account
        login_cmd = login_cmd + "$(aws sts get-caller-identity --query Account --output text).dkr.ecr." + region + ".amazonaws.com"

    if host == "local":
        r = local_exec(login_cmd)
    else:
        r = ssh_exec(host, login_cmd)
    if not r.ok:
        fail("ECR login failed on " + host + ":\n" + r.stderr)
    return r
