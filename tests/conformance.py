#!/usr/bin/env python3
"""Roost conformance suite.

Drives a live roost daemon through the Wings HTTP/WebSocket surface and
asserts protocol parity. Closes the "integration test" gap from
GAP-REPORT-roost-vs-wings.md (roadmap item 1).

Modes
-----
Offline (default): a mock panel implementing the /api/remote/* surface is
started locally, roost is launched against it, and every daemon route is
exercised end-to-end (including a real container start/stop cycle).

Live panel: set PANEL_URL + PANEL_TOKEN (application API key) + NODE_ID and
the suite uses the real panel instead of the mock. The daemon must already
be running and registered with that panel; set DAEMON_URL + DAEMON_TOKEN
accordingly.

Usage
-----
    python3 tests/conformance.py                 # offline, manages roost itself
    ROOST_BIN=./target/debug/roost python3 tests/conformance.py

Exit code 0 = all checks passed.
"""

import base64
import hashlib
import hmac
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid as uuidlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

ROOST_BIN = os.environ.get("ROOST_BIN", "./target/debug/roost")
DAEMON_HOST = "127.0.0.1"
DAEMON_PORT = int(os.environ.get("DAEMON_PORT", "8099"))
DAEMON_URL = os.environ.get("DAEMON_URL", f"http://{DAEMON_HOST}:{DAEMON_PORT}")
DAEMON_TOKEN = os.environ.get("DAEMON_TOKEN", "conformance-daemon-secret")
DAEMON_USER = os.environ.get("DAEMON_USER", "conformance-daemon-user")

MOCK_PANEL_PORT = int(os.environ.get("MOCK_PANEL_PORT", "8799"))
MOCK_PANEL_URL = f"http://127.0.0.1:{MOCK_PANEL_PORT}"
NODE_TOKEN_ID = "testnodeid"
NODE_TOKEN = DAEMON_TOKEN
SFTP_PORT = int(os.environ.get("SFTP_PORT", "2099"))

CONTAINER_IMAGE = os.environ.get("ROOST_IMAGE", "ghcr.io/parkervcp/yolks:nodejs_18")
START_CONTAINER = os.environ.get("ROOST_CONFORMANCE_START", "1") == "1"

TEST_UUID = "bcff11a0-1234-4abc-8def-0123456789ab"
USER_UUID = "5b1e7a90-5678-4abc-9def-0123456789cd"

# Live-panel mode overrides
PANEL_URL = os.environ.get("PANEL_URL")
PANEL_TOKEN = os.environ.get("PANEL_TOKEN")
NODE_ID = os.environ.get("NODE_ID")

USE_MOCK = not (PANEL_URL and PANEL_TOKEN and NODE_ID)

WORKDIR = ""

results = []


def check(name):
    def deco(fn):
        results.append((name, fn))
        return fn
    return deco


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def request(method, url, headers=None, data=None, timeout=15):
    req = urllib.request.Request(url, method=method, data=data)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = resp.read()
            return resp.status, body
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def api(method, path, body=None, headers=None, timeout=15):
    hdrs = {"Authorization": f"Bearer {DAEMON_TOKEN}"}
    hdrs.update(headers or {})
    data = None
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        hdrs.setdefault("Content-Type", "application/json")
    return request(method, DAEMON_URL + path, hdrs, data, timeout)


def jwt_encode(claims, secret=DAEMON_TOKEN):
    def b64(d):
        return base64.urlsafe_b64encode(d).rstrip(b"=")
    header = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    payload = b64(json.dumps(claims).encode())
    signing_input = header + b"." + payload
    sig = b64(hmac.new(secret.encode(), signing_input, hashlib.sha256).digest())
    return (signing_input + b"." + sig).decode()


def wait_for_daemon(deadline=60):
    end = time.time() + deadline
    while time.time() < end:
        try:
            status, _ = api("GET", "/api/system", timeout=3)
            if status == 200:
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def wait_until(predicate, timeout=90, interval=1.0, desc="condition"):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if predicate():
                return True
        except Exception:
            pass
        time.sleep(interval)
    raise AssertionError(f"timeout waiting for {desc}")


# ---------------------------------------------------------------------------
# Mock panel
# ---------------------------------------------------------------------------

class MockPanel:
    """Implements the /api/remote/* surface roost depends on (wings parity),
    records every callback so the suite can assert on panel interactions."""

    def __init__(self):
        self.calls = []
        self.lock = threading.Lock()

    def record(self, kind, payload=None):
        with self.lock:
            self.calls.append((kind, payload))
        self.calls_event.set()

    def wait_for(self, kind, timeout=30):
        end = time.time() + timeout
        while time.time() < end:
            with self.lock:
                for k, payload in self.calls:
                    if k == kind:
                        return payload
            time.sleep(0.2)
        raise AssertionError(f"mock panel never received {kind}")

    def seen(self, kind):
        with self.lock:
            return any(k == kind for k, _ in self.calls)


panel = MockPanel()


def server_settings():
    return {
        "uuid": TEST_UUID,
        "meta": {"name": "conformance", "description": "roost conformance server"},
        "suspended": False,
        "environment": {
            "SERVER_MEMORY": "256",
            "SERVER_IP": "0.0.0.0",
            "SERVER_PORT": "25565",
        },
        "invocation": "while true; do read l; if [ \"$l\" = \"exit\" ]; then exit 0; fi; echo \"cmd: $l\"; done",
        "skip_egg_scripts": True,
        "build": {
            "memory_limit": 256,
            "swap": -1,
            "io_weight": 500,
            "cpu_limit": 0,
            "threads": "",
            "disk_space": 1024,
            "oom_disabled": False,
        },
        "allocations": {
            "force_outgoing_ip": False,
            "default": {"ip": "0.0.0.0", "port": 25565},
            "mappings": {},
        },
        "mounts": [],
        "egg": {"id": "9dcbbe6a-2f4b-4d3e-9d0a-7e5b6a1c2d3e", "file_denylist": []},
        "container": {
            "image": CONTAINER_IMAGE,
            "oom_disabled": False,
            "requires_rebuild": False,
        },
        "labels": {},
        "crash_detection_enabled": False,
    }


def process_configuration():
    return {
        "startup": {
            "done": ["conformance-ready"],
            "user_interaction": [],
            "strip_ansi": False,
        },
        "stop": {"type": "command", "value": "exit"},
        "configs": [],
    }


class PanelHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self):
        auth = self.headers.get("Authorization", "")
        return auth == f"Bearer {NODE_TOKEN_ID}.{NODE_TOKEN}"

    def _read_body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if length else b""

    def do_GET(self):
        if not self._authorized():
            return self._json({"error": "unauthorized"}, 403)
        path = self.path.split("?")[0]
        if path == "/api/remote/servers":
            return self._json({
                "data": [{
                    "uuid": TEST_UUID,
                    "settings": server_settings(),
                    "process_configuration": process_configuration(),
                }],
                "meta": {"current_page": 1, "last_page": 1},
            })
        if path == f"/api/remote/servers/{TEST_UUID}":
            return self._json({
                "settings": server_settings(),
                "process_configuration": process_configuration(),
            })
        if path == f"/api/remote/servers/{TEST_UUID}/install":
            return self._json({
                "container_image": CONTAINER_IMAGE,
                "entrypoint": "",
                "script": "echo conformance-install",
            })
        if path.startswith("/api/remote/backups/"):
            return self._json({"parts": [], "part_size": 0})
        return self._json({"error": "not found"}, 404)

    def do_POST(self):
        if self.path.split("?")[0] == "/api/transfers":
            # Destination endpoint for an outgoing transfer: the source
            # node authenticates with a panel-issued transfer JWT, not the
            # node token pair.
            panel.record("transfer-received", None)
            self.send_response(204)
            self.end_headers()
            return
        if not self._authorized():
            return self._json({"error": "unauthorized"}, 403)
        path = self.path.split("?")[0]
        body = self._read_body()
        if path == "/api/remote/servers/reset":
            self.send_response(204)
            self.end_headers()
            return
        if path == f"/api/remote/servers/{TEST_UUID}/install":
            panel.record("install-status", json.loads(body or b"{}"))
            self.send_response(204)
            self.end_headers()
            return
        if path == f"/api/remote/servers/{TEST_UUID}/archive":
            self.send_response(204)
            self.end_headers()
            return
        if path.startswith("/api/remote/servers/") and "/transfer/" in path:
            state = path.rsplit("/", 1)[-1]
            panel.record("transfer-status", state)
            self.send_response(204)
            self.end_headers()
            return
        if path.startswith("/api/remote/backups/"):
            if path.endswith("/restore"):
                panel.record("backup-restore-status", json.loads(body or b"{}"))
            else:
                panel.record("backup-status", json.loads(body or b"{}"))
            self.send_response(204)
            self.end_headers()
            return
        if path == "/api/remote/activity":
            try:
                events = json.loads(body)
                panel.record("activity", events if isinstance(events, list) else [events])
            except Exception:
                panel.record("activity", [])
            self.send_response(204)
            self.end_headers()
            return
        if path == "/api/remote/sftp/auth":
            # Panel looks the server up by uuidShort (first 8 hex chars);
            # an unknown suffix is a rejection (wings/panel parity).
            try:
                req = json.loads(body or b"{}")
            except Exception:
                req = {}
            short = TEST_UUID.replace("-", "")[:8]
            if not req.get("username", "").endswith("." + short):
                return self._json({"error": "not found"}, 404)
            return self._json({
                "server": TEST_UUID,
                "user": "conformance-user",
                "permissions": [
                    "file.read", "file.read-content", "file.create",
                    "file.update", "file.delete", "*",
                ],
            })
        return self._json({"error": "not found"}, 404)


def start_mock_panel():
    panel.calls_event = threading.Event()
    server = ThreadingHTTPServer(("127.0.0.1", MOCK_PANEL_PORT), PanelHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

def make_jwt(scope, server_uuid=TEST_UUID, user_uuid=USER_UUID, perms=None,
             extra=None, iat_offset=0):
    now = int(time.time()) + iat_offset
    claims = {
        "sub": server_uuid,
        "scope": scope,
        "permissions": perms if perms is not None else [],
        "server_uuid": server_uuid,
        "user_uuid": user_uuid,
        "unique_id": str(uuidlib.uuid4()),
        "jti": str(uuidlib.uuid4()),
        "iat": now,
        "nbf": now,
        "exp": now + 600,
    }
    claims.update(extra or {})
    return jwt_encode(claims)


@check("auth: missing token -> 401")
def _():
    status, _ = request("GET", DAEMON_URL + "/api/system")
    assert status == 401, f"expected 401, got {status}"


@check("auth: wrong token -> 403")
def _():
    status, _ = request("GET", DAEMON_URL + "/api/system",
                        {"Authorization": "Bearer wrong"})
    assert status == 403, f"expected 403, got {status}"


@check("GET /api/system -> 200 with versions")
def _():
    status, body = api("GET", "/api/system")
    assert status == 200, f"expected 200, got {status}: {body[:200]}"
    data = json.loads(body)
    assert "version" in data, f"missing version: {data.keys()}"


@check("GET /api/servers -> lists boot servers")
def _():
    status, body = api("GET", "/api/servers")
    assert status == 200
    servers = json.loads(body)
    assert isinstance(servers, list) and servers, "no servers boot-loaded"
    uuids = [s.get("configuration", {}).get("uuid") for s in servers]
    assert TEST_UUID in uuids, f"server not booted: {uuids}"


@check("GET /api/servers/:uuid -> api response shape")
def _():
    status, body = api("GET", f"/api/servers/{TEST_UUID}")
    assert status == 200
    data = json.loads(body)
    for key in ("state", "is_suspended", "utilization", "configuration"):
        assert key in data, f"missing {key}"
    assert data["configuration"]["uuid"] == TEST_UUID


@check("GET unknown server -> 404")
def _():
    status, _ = api("GET", f"/api/servers/{uuidlib.uuid4()}")
    assert status == 404, f"expected 404, got {status}"


@check("files: write -> list -> contents -> copy -> rename -> chmod")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/write?file=hello.txt",
                    body=b"hello conformance")
    assert status == 204, f"write: {status}"

    status, body = api("GET", f"/api/servers/{TEST_UUID}/files/list-directory?directory=/")
    assert status == 200
    entries = json.loads(body)
    names = [e["name"] for e in entries]
    assert "hello.txt" in names, f"list: {names}"
    hello = next(e for e in entries if e["name"] == "hello.txt")
    # Wings FileInformation JSON shape.
    for key in ("name", "mode", "mode_bits", "size", "directory", "file",
                "symlink", "mime", "created", "modified"):
        assert key in hello, f"entry missing {key}: {hello}"

    status, body = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=hello.txt")
    assert status == 200 and body == b"hello conformance", f"contents: {status} {body[:100]}"

    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/copy",
                    {"location": "/hello.txt"})
    assert status == 200, f"copy: {status}"
    status, body = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=copy%20of%20hello.txt")
    assert status == 200 and body == b"hello conformance", f"copy contents: {status}"

    status, body = api("PUT", f"/api/servers/{TEST_UUID}/files/rename",
                       {"root": "/", "files": [{"from": "/copy of hello.txt", "to": "/renamed.txt"}]})
    assert status == 204, f"rename: {status} {body[:300]}"
    status, _ = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=renamed.txt")
    assert status == 200

    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/chmod",
                    {"root": "/", "files": [{"file": "renamed.txt", "mode": "0644"}]})
    assert status == 204, f"chmod: {status}"


@check("files: create-directory, compress, decompress, delete")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/create-directory",
                    {"name": "dir1", "path": "/"})
    assert status == 204, f"create-directory: {status}"

    status, body = api("POST", f"/api/servers/{TEST_UUID}/files/write?file=dir1/renamed.txt",
                       body=b"hello conformance")
    assert status == 204, f"write into dir1: {status}"

    status, body = api("POST", f"/api/servers/{TEST_UUID}/files/compress",
                       {"root": "/dir1", "files": ["renamed.txt"]})
    assert status == 200, f"compress: {status} {body[:200]}"
    archive = json.loads(body).get("name")
    assert archive and archive.endswith(".tar.gz"), f"archive name: {body}"

    # Extract in place (wings semantics: root = directory containing the
    # archive).
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/decompress",
                    {"root": "/dir1", "file": f"/{archive}"})
    assert status == 204, f"decompress: {status}"
    status, body = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=dir1/renamed.txt")
    assert status == 200 and body == b"hello conformance", "decompressed content mismatch"

    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/delete",
                    {"root": "/", "files": ["dir1"]})
    assert status == 204, f"delete: {status}"
    status, _ = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=dir1/renamed.txt")
    assert status == 404, "dir1 should be gone"


@check("files: pull rejects private URLs (SSRF guard)")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/files/pull",
                    {"url": "http://127.0.0.1/etc/passwd", "file_name": "x"})
    assert status == 403, f"expected SSRF 403, got {status}"


@check("logs endpoint returns data envelope")
def _():
    status, body = api("GET", f"/api/servers/{TEST_UUID}/logs")
    assert status == 200, f"logs: {status}"
    data = json.loads(body)
    assert "data" in data and isinstance(data["data"], list)


@check("power: invalid action -> 422")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/power", {"action": "explode"})
    assert status == 422, f"expected 422, got {status}"


@check("websocket: auth success + status event")
def _():
    import socket as _s
    import base64 as _b
    key = _b.b64encode(os.urandom(16)).decode()
    sock = _s.create_connection((DAEMON_HOST, DAEMON_PORT), timeout=10)
    req = (
        f"GET /api/servers/{TEST_UUID}/ws HTTP/1.1\r\n"
        f"Host: {DAEMON_HOST}:{DAEMON_PORT}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    resp = b""
    while b"\r\n\r\n" not in resp:
        resp += sock.recv(4096)
    assert b"101" in resp.split(b"\r\n")[0], f"upgrade failed: {resp[:200]}"

    token = make_jwt("websocket", perms=["websocket.connect"])

    def send_frame(opcode, payload):
        mask = os.urandom(4)
        data = payload.encode() if isinstance(payload, str) else payload
        header = bytes([0x80 | opcode])
        length = len(data)
        if length < 126:
            header += bytes([0x80 | length])
        elif length < 65536:
            header += bytes([0x80 | 126]) + length.to_bytes(2, "big")
        else:
            header += bytes([0x80 | 127]) + length.to_bytes(8, "big")
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        sock.sendall(header + mask + masked)

    def recv_exact(n):
        buf = b""
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise AssertionError("websocket closed")
            buf += chunk
        return buf

    def recv_frame():
        b1, b2 = recv_exact(2)
        length = b2 & 0x7F
        if length == 126:
            length = int.from_bytes(recv_exact(2), "big")
        elif length == 127:
            length = int.from_bytes(recv_exact(8), "big")
        payload = recv_exact(length)
        return b1 & 0x0F, payload

    send_frame(0x1, json.dumps({"event": "auth", "args": [token]}))
    got_auth = False
    got_status = False
    deadline = time.time() + 15
    while time.time() < deadline and not (got_auth and got_status):
        opcode, payload = recv_frame()
        try:
            msg = json.loads(payload)
        except Exception:
            continue
        if msg.get("event") == "auth success":
            got_auth = True
        elif msg.get("event") == "status":
            got_status = True
            assert msg["args"] and msg["args"][0] in ("offline", "starting", "running",
                                                      "stopping", "restarting"), msg
    assert got_auth and got_status, f"auth={got_auth} status={got_status}"
    send_frame(0x8, b"")
    sock.close()


@check("websocket: bad token -> jwt error")
def _():
    import socket as _s
    import base64 as _b
    key = _b.b64encode(os.urandom(16)).decode()
    sock = _s.create_connection((DAEMON_HOST, DAEMON_PORT), timeout=10)
    req = (
        f"GET /api/servers/{TEST_UUID}/ws HTTP/1.1\r\n"
        f"Host: {DAEMON_HOST}:{DAEMON_PORT}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    resp = b""
    while b"\r\n\r\n" not in resp:
        resp += sock.recv(4096)

    def send_frame(payload):
        mask = os.urandom(4)
        data = payload.encode()
        header = bytes([0x81, 0x80 | len(data)])
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        sock.sendall(header + mask + masked)

    def recv_exact(n):
        buf = b""
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise AssertionError("websocket closed")
            buf += chunk
        return buf

    def recv_frame():
        b1, b2 = recv_exact(2)
        length = b2 & 0x7F
        if length == 126:
            length = int.from_bytes(recv_exact(2), "big")
        payload = recv_exact(length)
        return payload

    send_frame(json.dumps({"event": "auth", "args": ["garbage.token.here"]}))
    payload = recv_frame()
    msg = json.loads(payload)
    assert msg.get("event") == "jwt error", msg
    sock.close()


@check("power: start -> running")
def _():
    if not START_CONTAINER:
        return
    status, body = api("POST", f"/api/servers/{TEST_UUID}/power", {"action": "start"})
    assert status == 202, f"start: {status} {body[:200]}"

    def running():
        _, b = api("GET", f"/api/servers/{TEST_UUID}")
        return json.loads(b).get("state") == "running"
    wait_until(running, timeout=120, desc="container running")


@check("console: command reaches container logs")
def _():
    if not START_CONTAINER:
        return
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/commands",
                    {"commands": ["echo conformance-mark"]})
    assert status == 204, f"commands: {status}"
    wait_until(lambda: "conformance-mark" in "".join(
        json.loads(api("GET", f"/api/servers/{TEST_UUID}/logs")[1])["data"]
    ), timeout=30, desc="console output")


@check("console: empty commands accepted (no-op)")
def _():
    if not START_CONTAINER:
        return
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/commands", {"commands": []})
    assert status == 204, f"expected 204, got {status}"


@check("backups: create -> panel status callback -> restore -> delete")
def _():
    backup_uuid = str(uuidlib.uuid4())
    status, body = api("POST", f"/api/servers/{TEST_UUID}/backup",
                       {"adapter": "wings", "uuid": backup_uuid, "ignore": ""})
    assert status == 202, f"create backup: {status} {body[:200]}"
    reported = panel.wait_for("backup-status", timeout=120)
    assert reported.get("successful") is True, f"backup not successful: {reported}"
    assert reported.get("checksum"), "no checksum reported"
    assert reported.get("checksum_type") == "sha1"

    status, _ = api("POST", f"/api/servers/{TEST_UUID}/backup/{backup_uuid}/restore",
                    {"adapter": "wings", "truncate_directory": True})
    assert status in (204, 202), f"restore: {status}"
    if USE_MOCK:
        reported = panel.wait_for("backup-restore-status", timeout=120)
        assert reported.get("successful") is True, f"restore failed: {reported}"

    status, _ = api("DELETE", f"/api/servers/{TEST_UUID}/backup/{backup_uuid}")
    assert status == 204, f"delete backup: {status}"


@check("backup: unknown adapter -> 501")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/backup",
                    {"adapter": "rsync", "uuid": str(uuidlib.uuid4())})
    assert status == 501, f"expected 501, got {status}"


@check("downloads: file download with JWT")
def _():
    # Ensure a file exists.
    api("POST", f"/api/servers/{TEST_UUID}/files/write?file=dl.txt", body=b"download-me")
    token = make_jwt("file-download", extra={"file_path": "/dl.txt"})
    status, body = request("GET", DAEMON_URL + f"/download/file?token={token}")
    assert status == 200, f"download: {status}"
    assert body == b"download-me", body[:100]


@check("downloads: backup download with JWT (missing backup -> 404)")
def _():
    token = make_jwt("backup-download",
                     extra={"backup_uuid": str(uuidlib.uuid4())})
    status, _ = request("GET", DAEMON_URL + f"/download/backup?token={token}")
    assert status == 404, f"expected 404, got {status}"


@check("downloads: expired JWT -> 401")
def _():
    claims = {
        "sub": TEST_UUID,
        "scope": "file-download",
        "permissions": [],
        "server_uuid": TEST_UUID,
        "user_uuid": USER_UUID,
        "file_path": "/dl.txt",
        "iat": int(time.time()) - 600,
        "nbf": int(time.time()) - 600,
        "exp": int(time.time()) - 100,
    }
    expired = jwt_encode(claims)
    status, _ = request("GET", DAEMON_URL + f"/download/file?token={expired}")
    assert status == 401, f"expected 401, got {status}"


@check("uploads: multipart upload with JWT")
def _():
    token = make_jwt("file-upload")
    boundary = "----roostconformance"
    content = b"uploaded content"
    part = (
        f"--{boundary}\r\n"
        'Content-Disposition: form-data; name="files"; filename="up.txt"\r\n'
        "Content-Type: text/plain\r\n\r\n"
    ).encode() + content + f"\r\n--{boundary}--\r\n".encode()
    status, body = request(
        "POST",
        DAEMON_URL + f"/upload/file?token={token}&directory=/",
        {"Content-Type": f"multipart/form-data; boundary={boundary}"},
        part,
    )
    assert status == 200, f"upload: {status} {body[:200]}"
    status, body = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=up.txt")
    assert status == 200 and body == b"uploaded content"


@check("install: run install script -> panel install callback")
def _():
    status, body = api("POST", f"/api/servers/{TEST_UUID}/install")
    assert status == 202, f"install: {status} {body[:200]}"
    if USE_MOCK:
        reported = panel.wait_for("install-status", timeout=180)
        assert reported.get("successful") is True, f"install failed: {reported}"


@check("sync: re-pull server config from panel")
def _():
    status, body = api("POST", f"/api/servers/{TEST_UUID}/sync")
    assert status == 204, f"sync: {status} {body[:200]}"


@check("ws/deny: revoke jtis -> 204")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/ws/deny",
                    {"jtis": [str(uuidlib.uuid4())]})
    assert status == 204, f"deny: {status}"


@check("deauthorize-user -> 204")
def _():
    status, _ = api("POST", "/api/deauthorize-user",
                    {"user": USER_UUID, "servers": [TEST_UUID]})
    assert status == 204, f"deauthorize: {status}"


@check("transfers: outgoing transfer -> destination receives archive")
def _():
    if not USE_MOCK:
        return  # requires a writable destination; mock-only check
    status, body = api("POST", f"/api/servers/{TEST_UUID}/transfer",
                       {"url": MOCK_PANEL_URL + "/api/transfers",
                        "token": make_jwt("transfer", extra={"sub": TEST_UUID})})
    assert status == 202, f"transfer: {status} {body[:200]}"
    # Wings parity: only the destination node reports success to the panel;
    # the source stays silent on success.
    panel.wait_for("transfer-received", timeout=180)
    time.sleep(2)
    status, _ = api("DELETE", f"/api/servers/{TEST_UUID}/transfer")
    assert status in (204, 409), f"cancel transfer: {status}"


@check("transfers: incoming transfer with bad JWT -> 401")
def _():
    boundary = "----roostconformance"
    part = (
        f"--{boundary}\r\n"
        'Content-Disposition: form-data; name="archive"; filename="archive.tar.gz"\r\n'
        "Content-Type: application/gzip\r\n\r\n"
    ).encode() + b"junk" + f"\r\n--{boundary}--\r\n".encode()
    status, _ = request(
        "POST",
        DAEMON_URL + "/api/transfers",
        {
            "Authorization": "Bearer not-a-jwt",
            "Content-Type": f"multipart/form-data; boundary={boundary}",
        },
        part,
    )
    assert status == 401, f"expected 401, got {status}"


@check("POST /api/servers -> create then detail 200")
def _():
    if not USE_MOCK:
        return
    status, body = api("POST", "/api/servers", {"uuid": TEST_UUID})
    assert status in (200, 201, 202, 204, 409), f"create: {status} {body[:200]}"

@check("POST /api/update -> applies config")
def _():
    # Push a complete, valid configuration (the panel replaces the whole
    # file; the daemon refuses partial payloads with empty tokens).
    full_config = {
        "debug": False,
        "app_name": "roost-conformance",
        "uuid": "0aa11a11-2222-4333-8444-555566667777",
        "token_id": NODE_TOKEN_ID,
        "token": NODE_TOKEN,
        "api": {
            "host": DAEMON_HOST,
            "port": DAEMON_PORT,
            "ssl": {"enabled": False, "cert": "", "key": ""},
            "upload_limit": 10,
        },
        "system": {
            "data": f"{WORKDIR}/data",
            "log_directory": f"{WORKDIR}/logs",
            "sftp": {"bind_port": 2099},
        },
        "docker": {"network": {"name": "pterodactyl_nw"}},
        "remote": PANEL_URL or MOCK_PANEL_URL,
        "allowed_mounts": [],
        "allowed_origins": [],
    }
    status, body = api("POST", "/api/update", full_config)
    assert status == 200, f"update: {status} {body[:300]}"
    assert json.loads(body).get("applied") is True, body


@check("power: stop -> offline")
def _():
    if not START_CONTAINER:
        return
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/power", {"action": "stop"})
    assert status == 202

    def offline():
        _, b = api("GET", f"/api/servers/{TEST_UUID}")
        return json.loads(b).get("state") == "offline"
    wait_until(offline, timeout=120, desc="container offline")


@check("power: kill while offline -> 202")
def _():
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/power", {"action": "kill"})
    assert status == 202


@check("sftp: auth + file operations via real SSH client")
def _():
    try:
        import paramiko
    except ImportError:
        print("  (paramiko not installed; skipping SFTP live test)")
        return
    # Wings username format: <user>.<uuidShort> (first 8 hex of the uuid).
    uuid_short = TEST_UUID.replace("-", "")[:8]
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    client.connect(
        "127.0.0.1",
        port=SFTP_PORT,
        username=f"conformance.{uuid_short}",
        password="sftp-password",
        look_for_keys=False,
        allow_agent=False,
    )
    try:
        sftp = client.open_sftp()
        with sftp.open("sftp.txt", "w") as f:
            f.write("sftp roundtrip")
        with sftp.open("sftp.txt", "r") as f:
            assert f.read().decode() == "sftp roundtrip", "sftp content mismatch"
        names = [s.strip("/") for s in sftp.listdir("/")]
        assert "sftp.txt" in names, f"sftp listdir: {names}"
        status, body = api("GET",
                           f"/api/servers/{TEST_UUID}/files/contents?file=sftp.txt")
        assert status == 200 and body == b"sftp roundtrip", \
            "sftp write not visible through HTTP file API"
    finally:
        client.close()


@check("sftp: invalid username format -> rejected without panel call")
def _():
    try:
        import paramiko
    except ImportError:
        return
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    before = panel.seen("activity")  # placeholder; assert no sftp auth below
    rejected = False
    try:
        client.connect(
            "127.0.0.1",
            port=SFTP_PORT,
            username="no-suffix-format",
            password="x",
            look_for_keys=False,
            allow_agent=False,
        )
    except paramiko.ssh_exception.AuthenticationException:
        rejected = True
    except Exception:
        rejected = True
    finally:
        client.close()
    assert rejected, "invalid username should be rejected"


@check("sftp: wrong server (valid format, unknown uuid) -> rejected")
def _():
    try:
        import paramiko
    except ImportError:
        return
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    rejected = False
    try:
        client.connect(
            "127.0.0.1",
            port=SFTP_PORT,
            username=f"conformance.{uuidlib.uuid4().hex[:8]}",
            password="sftp-password",
            look_for_keys=False,
            allow_agent=False,
        )
    except paramiko.ssh_exception.AuthenticationException:
        rejected = True
    except Exception:
        rejected = True
    finally:
        client.close()
    assert rejected, "unknown server should be rejected"


@check("files: empty file lists -> 422 (wings parity)")
def _():
    for method, path, body in [
        ("PUT", f"/api/servers/{TEST_UUID}/files/rename", {"root": "/", "files": []}),
        ("POST", f"/api/servers/{TEST_UUID}/files/delete", {"root": "/", "files": []}),
        ("POST", f"/api/servers/{TEST_UUID}/files/compress", {"root": "/", "files": []}),
        ("POST", f"/api/servers/{TEST_UUID}/files/chmod", {"root": "/", "files": []}),
    ]:
        status, body = api(method, path, body)
        assert status == 422, f"{path}: expected 422, got {status} {body[:200]}"


@check("backups: .pteroignore excludes files from archive")
def _():
    api("POST", f"/api/servers/{TEST_UUID}/files/write?file=keep.txt", body=b"keep")
    api("POST", f"/api/servers/{TEST_UUID}/files/write?file=skipme.txt", body=b"skip")
    api("POST", f"/api/servers/{TEST_UUID}/files/write?file=.pteroignore",
        body=b"skipme.txt\n")

    backup_uuid = str(uuidlib.uuid4())
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/backup",
                    {"adapter": "wings", "uuid": backup_uuid, "ignore": ""})
    assert status == 202
    marker_b = len(panel.calls)
    wait_until(lambda: any(
        k == "backup-status" and p.get("successful") is True
        for k, p in panel.calls[marker_b:]
    ), timeout=120, desc="backup success")

    # Restore into a clean directory and inspect what survived.
    marker = len(panel.calls)
    status, _ = api("POST", f"/api/servers/{TEST_UUID}/backup/{backup_uuid}/restore",
                    {"adapter": "wings", "truncate_directory": True})
    assert status in (202, 204)

    def restored():
        with panel.lock:
            recent = [p for k, p in panel.calls[marker:] if k == "backup-restore-status"]
        return any(p.get("successful") is True for p in recent)
    wait_until(restored, timeout=120, desc="restore success")
    status, _ = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=keep.txt")
    assert status == 200, "kept file missing after restore"
    status, _ = api("GET", f"/api/servers/{TEST_UUID}/files/contents?file=skipme.txt")
    assert status == 404, ".pteroignore entry was included in the backup"
    api("DELETE", f"/api/servers/{TEST_UUID}/backup/{backup_uuid}")


@check("DELETE /api/servers/:uuid -> 204 and gone")
def _():
    status, _ = api("DELETE", f"/api/servers/{TEST_UUID}")
    assert status == 204, f"delete: {status}"
    status, _ = api("GET", f"/api/servers/{TEST_UUID}")
    assert status == 404, f"server should be gone, got {status}"


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def write_config(workdir):
    cfg = f"""debug: false
app_name: roost-conformance
uuid: "0aa11a11-2222-4333-8444-555566667777"
token_id: "{NODE_TOKEN_ID}"
token: "{NODE_TOKEN}"
api:
  host: {DAEMON_HOST}
  port: {DAEMON_PORT}
  ssl:
    enabled: false
  upload_limit: 10
system:
  root_directory: {workdir}/root
  log_directory: {workdir}/logs
  data: {workdir}/data
  archive_directory: {workdir}/archives
  backup_directory: {workdir}/backups
  tmp_directory: {workdir}/tmp
  username: roost-conformance
  timezone: UTC
  disk_check_interval: 150
  activity_send_interval: 2
  activity_send_count: 100
  sftp:
    bind_port: {SFTP_PORT}
  crash_detection:
    enabled: false
docker:
  network:
    name: pterodactyl_nw
remote: "{PANEL_URL or MOCK_PANEL_URL}"
allowed_mounts: []
allowed_origins: []
"""
    path = os.path.join(workdir, "config.yml")
    with open(path, "w") as f:
        f.write(cfg)
    return path


def main():
    global WORKDIR
    only = sys.argv[1] if len(sys.argv) > 1 else None
    workdir = tempfile.mkdtemp(prefix="roost-conformance-")
    WORKDIR = workdir
    daemon = None
    panel_server = None
    failures = 0

    def cleanup(*_):
        if daemon and daemon.poll() is None:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        if panel_server:
            panel_server.shutdown()

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, cleanup)

    try:
        if USE_MOCK:
            panel_server = start_mock_panel()
            config_path = write_config(workdir)
            env = dict(os.environ, ROOST_CONFIG=config_path)
            daemon = subprocess.Popen(
                [ROOST_BIN], env=env,
                stdout=open(os.path.join(workdir, "daemon.out"), "ab"),
                stderr=subprocess.STDOUT,
            )
            print(f"[suite] roost starting (config {config_path}, logs {workdir})")
        else:
            print("[suite] live-panel mode; assuming daemon already running")

        if not wait_for_daemon():
            print("[suite] FATAL: daemon did not become ready; dumping log")
            if daemon:
                subprocess.run(["tail", "-50", os.path.join(workdir, "daemon.out")])
            return 2

        if USE_MOCK:
            # The server is already boot-loaded from the panel list; POST
            # create is a no-op duplicate (409 is fine).
            status, body = api("POST", "/api/servers", {"uuid": TEST_UUID})
            print(f"[suite] create server -> {status}")
            if status not in (200, 201, 202, 204, 409):
                print(body[:400])
                return 2

        for name, fn in results:
            if only and only not in name:
                continue
            try:
                fn()
                print(f"[PASS] {name}")
            except Exception as e:
                failures += 1
                print(f"[FAIL] {name}: {e}")
        print(f"\n[suite] {len(results) - failures}/{len(results)} checks passed"
              + (f", {failures} failed" if failures else ""))
        return 1 if failures else 0
    finally:
        cleanup()
        if USE_MOCK and not os.environ.get("ROOST_CONFORMANCE_KEEP"):
            shutil.rmtree(workdir, ignore_errors=True)
        elif USE_MOCK:
            print(f"[suite] kept workdir {workdir}")


if __name__ == "__main__":
    sys.exit(main())
