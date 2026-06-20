#!/usr/bin/env python3
"""agent.py — per-node Faasm Faaslet launcher (track D of EXPERIMENT_PLAN §5).

Runs on EVERY compute node. A tiny stdlib HTTP server (no Flask dep) — the
baseline-side analogue of WasMem's node-agent — that the coordinator calls to
launch/manage Faaslets on this node. Faasm is clean-slate, so this is the
per-node placement layer the other baselines get from k8s/Knative.

Endpoints (all JSON):
  GET  /health                      → {ok, node, faaslets}
  POST /launch  {cmd[], env{}, cwd, tag}
                                    → {handle}        start a Faaslet (wasmtime) process
  GET  /status/<handle>             → {running, exit_code, ms, stdout_tail, stderr_tail}
  POST /stop/<handle>               → {stopped}
  POST /stage   {path, b64}         → {bytes}         write a file (a .cwasm or input)
  GET  /metrics                     → {cpu_pct, rss_mb, per_faaslet[…]}

Faaslets move state through the shared Redis (REDIS_HOST) — the agent doesn't
touch state, it only places + supervises processes (one transfer/peer is not a
concern here: every node talks to the same neutral Redis box).

Usage:  AGENT_PORT=9600 STAGE_DIR=/tmp/faasm_stage ./agent.py
"""
import base64
import json
import os
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("AGENT_PORT", "9600"))
STAGE_DIR = os.environ.get("STAGE_DIR", "/tmp/faasm_stage")
NODE = socket.gethostname()
os.makedirs(STAGE_DIR, exist_ok=True)

# handle → {proc, tag, t0, t1, exit}
_FAASLETS = {}
_LOCK = threading.Lock()
_SEQ = 0


def _new_handle(tag):
    global _SEQ
    with _LOCK:
        _SEQ += 1
        return f"{tag}-{_SEQ}"


def _rss_mb(pid):
    """Private-ish RSS in MB from /proc (best-effort)."""
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return 0.0


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n) or b"{}")

    # ── GET ──────────────────────────────────────────────────────────────────
    def do_GET(self):
        if self.path == "/health":
            return self._send(200, {"ok": True, "node": NODE, "faaslets": len(_FAASLETS)})
        if self.path.startswith("/status/"):
            h = self.path[len("/status/"):]
            with _LOCK:
                e = _FAASLETS.get(h)
            if not e:
                return self._send(404, {"error": "no such handle"})
            rc = e["proc"].poll()
            if rc is not None and e["t1"] is None:
                e["t1"] = time.time(); e["exit"] = rc
            ms = int(((e["t1"] or time.time()) - e["t0"]) * 1000)
            return self._send(200, {
                "running": rc is None, "exit_code": e["exit"], "ms": ms,
                "stdout_tail": e["out"][-2000:], "stderr_tail": e["err"][-2000:],
            })
        if self.path == "/metrics":
            per = []
            tot_rss = 0.0
            with _LOCK:
                items = list(_FAASLETS.items())
            for h, e in items:
                if e["proc"].poll() is None:
                    r = _rss_mb(e["proc"].pid); tot_rss += r
                    per.append({"handle": h, "pid": e["proc"].pid, "rss_mb": round(r, 1)})
            return self._send(200, {"node": NODE, "rss_mb": round(tot_rss, 1), "per_faaslet": per})
        return self._send(404, {"error": "unknown GET"})

    # ── POST ─────────────────────────────────────────────────────────────────
    def do_POST(self):
        try:
            b = self._body()
        except Exception as ex:
            return self._send(400, {"error": f"bad json: {ex}"})

        if self.path == "/launch":
            cmd = b.get("cmd")
            if not cmd:
                return self._send(400, {"error": "cmd[] required"})
            env = dict(os.environ); env.update(b.get("env", {}))
            cwd = b.get("cwd", STAGE_DIR)
            h = _new_handle(b.get("tag", "faaslet"))
            try:
                p = subprocess.Popen(cmd, cwd=cwd, env=env,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            except Exception as ex:
                return self._send(500, {"error": f"spawn failed: {ex}"})
            e = {"proc": p, "tag": b.get("tag"), "t0": time.time(), "t1": None,
                 "exit": None, "out": "", "err": ""}
            with _LOCK:
                _FAASLETS[h] = e
            # Drain pipes in the background so the Faaslet never blocks on a full pipe.
            threading.Thread(target=self._drain, args=(e,), daemon=True).start()
            return self._send(200, {"handle": h, "pid": p.pid})

        if self.path.startswith("/stop/"):
            h = self.path[len("/stop/"):]
            with _LOCK:
                e = _FAASLETS.get(h)
            if not e:
                return self._send(404, {"error": "no such handle"})
            e["proc"].kill()
            return self._send(200, {"stopped": h})

        if self.path == "/stage":
            path = b.get("path"); data = b.get("b64")
            if not path or data is None:
                return self._send(400, {"error": "path + b64 required"})
            dst = path if os.path.isabs(path) else os.path.join(STAGE_DIR, path)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            raw = base64.b64decode(data)
            with open(dst, "wb") as f:
                f.write(raw)
            return self._send(200, {"path": dst, "bytes": len(raw)})

        return self._send(404, {"error": "unknown POST"})

    @staticmethod
    def _drain(e):
        out, err = e["proc"].communicate()
        e["out"] += out or ""; e["err"] += err or ""
        if e["t1"] is None:
            e["t1"] = time.time(); e["exit"] = e["proc"].returncode


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    print(f"[faasm-agent {NODE}] listening on :{PORT}  stage={STAGE_DIR}", flush=True)
    srv.serve_forever()
