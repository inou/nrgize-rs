#!/usr/bin/env python3
"""Disposable reference recipe, not a production supervisor or framework certification.

All managed files are under NRG_WORKSPACE. The only network listener binds loopback
on a kernel-assigned port. Faults affect only this fixture's own children/artifacts.
"""
import http.server
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(os.environ["NRG_WORKSPACE"])
os.chdir(ROOT)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        value = Path("current").read_text()
        self.send_response(503 if value == "broken" else 200)
        self.end_headers()
        self.wfile.write(value.encode())

    def log_message(self, *_args):
        pass


def request(port=None):
    # Explicitly disable inherited proxy configuration, including system proxies.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    port = Path("port").read_text() if port is None else port
    return opener.open("http://127.0.0.1:" + port, timeout=1).read().decode()


def stop(sig=signal.SIGTERM):
    port = Path("port").read_text() if Path("port").exists() else None
    if Path("pid").exists():
        pid = int(Path("pid").read_text())
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass
        Path("pid").unlink()
    if port is not None:
        deadline = time.monotonic() + 5
        while True:
            try:
                request(port)
            except urllib.error.HTTPError:
                pass  # An unhealthy HTTP response still proves the service is running.
            except OSError:
                break
            if time.monotonic() > deadline:
                raise RuntimeError("owned service did not stop")
            time.sleep(0.01)
    Path("port").unlink(missing_ok=True)


def start():
    stop()
    with open("server.log", "ab") as log:
        child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "serve"],
                                 stdout=log, stderr=log, stdin=subprocess.DEVNULL)
    Path("pid").write_text(str(child.pid))
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError("service exited: " + Path("server.log").read_text()[-2048:])
        try:
            request()
            return
        except (OSError, ValueError):
            time.sleep(0.02)
    stop()
    raise RuntimeError("service readiness deadline exceeded")


def deploy(version):
    previous = Path("current").read_bytes() if Path("current").exists() else None
    Path("candidate").write_text(version)
    os.replace("candidate", "current")
    if not Path("pid").exists():
        start()
    try:
        assert request() == version
    except urllib.error.HTTPError as error:
        assert error.code == 503, "only the injected unhealthy response is expected"
        if previous is None:
            Path("current").unlink()
            stop()
        else:
            Path("candidate").write_bytes(previous)
            os.replace("candidate", "current")
            assert request().encode() == previous
        print("candidate health failed; previous artifact restored", file=sys.stderr)
        sys.exit(42)


def interrupt_upload():
    # Signal a real writer after observing a partial write, not after a guessed delay.
    child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "upload"],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 5
        while not Path(".upload-partial").exists():
            if child.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("writer never reached its partial write")
            time.sleep(0.01)
        child.terminate()
        assert child.wait(timeout=5) == 43
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
    print("owned upload interrupted after partial write", file=sys.stderr)
    sys.exit(43)


if __name__ == "__main__":
    command = sys.argv[1]
    if command == "serve":
        with http.server.HTTPServer(("127.0.0.1", 0), Handler) as server:
            Path("port").write_text(str(server.server_port))
            server.serve_forever()
    elif command == "deploy":
        deploy(sys.argv[2])
    elif command in ("start", "restart"):
        start()
    elif command == "stop":
        stop()
    elif command == "kill":
        stop(signal.SIGKILL)
    elif command == "check":
        assert request() == "v1"
    elif command == "interrupt-upload":
        interrupt_upload()
    elif command == "upload":
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(43))
        try:
            Path(".upload-partial").write_bytes(b"incomplete artifact")
            time.sleep(30)
            os.replace(".upload-partial", "current")
        finally:
            Path(".upload-partial").unlink(missing_ok=True)
    else:
        raise ValueError("unknown demo operation")
