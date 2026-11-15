#!/usr/bin/env python3
"""Mock of the local NIBE REST API for development and testing.

Serves a real `/points` dump (testdata/points.json.gz) over HTTPS with basic auth and makes
a few points move, so the collector and MCP server have something to record.

    python3 tools/mock_nibe.py --port 8443 --user nibe --password secret
"""
import argparse, base64, gzip, hashlib, json, math, os, ssl, subprocess, tempfile, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ap = argparse.ArgumentParser()
ap.add_argument("--port", type=int, default=8443)
ap.add_argument("--user", default="nibe")
ap.add_argument("--password", default="secret")
ap.add_argument("--data", default=os.path.join(os.path.dirname(__file__), "..", "testdata", "points.json.gz"))
ap.add_argument("--speed", type=float, default=60.0, help="simulated seconds per real second")
args = ap.parse_args()

with gzip.open(args.data, "rt") as f:
    BASE = json.load(f)

START = time.time()
state = {"prod": 12144.7, "cons": 4043.0, "heat": 8740.0, "hw": 1239.1, "last": START}


def sim_time():
    return (time.time() - START) * args.speed


def current():
    """Returns a copy of the dump with moving values."""
    now = time.time()
    dt = (now - state["last"]) * args.speed / 3600.0  # simulated hours since the previous request
    state["last"] = now
    t = sim_time()
    on = 1 if math.sin(t / 5400.0) > -0.3 else 0
    comp_w = int(250 + 150 * (math.sin(t / 900.0) + 1)) if on else 0
    unit_w = comp_w + 36
    # 1 kWh of electricity yields ~3.2 kWh of heat; counters only advance while the compressor runs
    state["cons"] += unit_w / 1000.0 * dt
    state["prod"] += unit_w / 1000.0 * dt * 3.2
    state["heat"] += unit_w / 1000.0 * dt * 3.0
    state["hw"] += unit_w / 1000.0 * dt * 0.2
    out = json.loads(json.dumps(BASE))

    def setv(pid, raw):
        out[str(pid)]["value"]["integerValue"] = int(round(raw))

    setv(4, 80 + 40 * math.sin(t / 43200.0))            # outdoor temperature, /10
    setv(22, 205 + 5 * math.sin(t / 7200.0))            # exhaust air
    setv(23, 28 + 10 * on)
    setv(1801, comp_w)
    setv(1932, comp_w)
    setv(22130, unit_w)
    setv(1973, on)
    setv(1799, (190 if on else 0))
    setv(28392, state["prod"] * 10)
    setv(28393, state["cons"] * 10)
    setv(27378, state["heat"] * 10)
    setv(27379, state["hw"] * 10)
    return out


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _auth(self):
        want = "Basic " + base64.b64encode(f"{args.user}:{args.password}".encode()).decode()
        if self.headers.get("Authorization") != want:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="nibe"')
            self.end_headers()
            return False
        return True

    def _send(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if not self._auth():
            return
        p = self.path.split("?")[0].rstrip("/")
        if p == "/api/v1/devices/0/points":
            return self._send(current())
        if p.startswith("/api/v1/devices/0/points/"):
            pid = p.rsplit("/", 1)[1]
            cur = current()
            if pid in cur:
                return self._send(cur[pid])
        if p == "/api/v1/devices":
            return self._send([{"deviceId": 0}])
        self._send({"error": "not found"}, 404)


d = tempfile.mkdtemp()
cert, key = os.path.join(d, "c.pem"), os.path.join(d, "k.pem")
subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", cert,
                "-days", "2", "-subj", "/CN=mock-nibe"], check=True, capture_output=True)
srv = ThreadingHTTPServer(("0.0.0.0", args.port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
with open(cert) as f:
    fingerprint = hashlib.sha256(ssl.PEM_cert_to_DER_cert(f.read())).hexdigest().upper()
print(f"mock NIBE on https://0.0.0.0:{args.port} (user {args.user})", flush=True)
print(f"NIBE_TLS_FINGERPRINT={':'.join(fingerprint[i:i + 2] for i in range(0, 64, 2))}", flush=True)
srv.serve_forever()
