#!/usr/bin/env python3
"""rt-agent.py — dedicated RTSFaaS per-node role launcher (NOT the faasm agent).

RTSFaaS ships an scp+ssh deploy (scripts/FaaS/run-application.sh). We replace that
with this tiny HTTP agent — one per node — modeled on the faasm track-D agent but
RTSFaaS-specific: it knows the four RTSFaaS roles (driver/database/worker/client)
and launches each as the same privileged, host-network, infiniband docker
container the single-node baseline uses (../single_node/run-single-node.sh), just
on whichever node the coordinator targets.

Endpoints:
  GET  /health                      -> {ok, host, docker}
  POST /role  {role, worker_id, app, rtdir, image, docker}
                                    -> {name}  (starts container detached)
  GET  /status/<name>               -> {status, exit_code, running}
  GET  /logs/<name>                 -> {log}   (raw; coordinator greps)
  POST /rm    {names?}              -> {removed}  (default: all rt-* on this node)

The role container invocation mirrors single_node exactly:
  $DOCKER run -d --name rt-<role>[wid] --network host \
     --env-file Env/Cluster.env --env-file Env/System.env \
     --env-file Env/<App>.env  --env-file Env/Database.env \
     -v <rtdir>/DockerFiles/<role>.sh:/rtfaas/<role>.sh \
     -v <rtdir>/DockerFiles/entrypoint.sh:/rtfaas/entrypoint.sh \
     --privileged --device=/dev/infiniband/ <image> <role> [worker_id]
cwd = rtdir so the relative --env-file paths resolve (same as single-node).
"""
import json
import os
import shlex
import socket
import subprocess
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("AGENT_PORT", "9700"))
ROLES = ("driver", "database", "worker", "client")


def sh(cmd, cwd=None):
    p = subprocess.run(cmd, cwd=cwd, stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT, text=True)
    return p.returncode, p.stdout


def container_name(role, worker_id):
    return f"rt-{role}{worker_id}" if role == "worker" else f"rt-{role}"


def launch_role(b):
    role = b["role"]
    if role not in ROLES:
        return None, f"unknown role {role!r}"
    wid = int(b.get("worker_id", 0))
    app = b["app"]                                  # MediaReview | SocialNetwork
    rtdir = b["rtdir"]                              # abs path to .../cluster on this node
    image = b.get("image", "rtfaas:1.0")
    docker = shlex.split(b.get("docker", "sudo docker"))
    name = container_name(role, wid)
    sh(docker + ["rm", "-f", name])                 # idempotent
    envs = []
    for e in ("Cluster", "System", app, "Database"):
        envs += ["--env-file", f"Env/{e}.env"]
    mounts = ["-v", f"{rtdir}/DockerFiles/{role}.sh:/rtfaas/{role}.sh",
              "-v", f"{rtdir}/DockerFiles/entrypoint.sh:/rtfaas/entrypoint.sh"]
    dev = ["--privileged", "--device=/dev/infiniband/"]
    cmd = (docker + ["run", "-d", "--name", name, "--network", "host"]
           + envs + mounts + dev + [image, role])
    if role == "worker":
        cmd.append(str(wid))
    rc, out = sh(cmd, cwd=rtdir)
    if rc != 0:
        return None, f"docker run failed (rc={rc}): {out.strip()[-500:]}"
    return name, None


def inspect(name, docker):
    rc, out = sh(docker + ["inspect", "-f",
                           "{{.State.Status}} {{.State.ExitCode}}", name])
    if rc != 0:
        return {"status": "absent", "exit_code": None, "running": False}
    status, code = (out.split() + ["", ""])[:2]
    return {"status": status, "exit_code": int(code) if code.isdigit() else None,
            "running": status == "running"}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _docker(self):
        return shlex.split(os.environ.get("DOCKER", "sudo docker"))

    def do_GET(self):
        if self.path == "/health":
            rc, out = sh(self._docker() + ["version", "--format", "{{.Server.Version}}"])
            return self._send(200, {"ok": rc == 0, "host": socket.gethostname(),
                                    "docker": out.strip()})
        if self.path.startswith("/status/"):
            return self._send(200, inspect(self.path[len("/status/"):], self._docker()))
        if self.path.startswith("/logs/"):
            rc, out = sh(self._docker() + ["logs", self.path[len("/logs/"):]])
            return self._send(200, {"log": out})
        self._send(404, {"error": "not found"})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        b = json.loads(self.rfile.read(n) or b"{}")
        if self.path == "/role":
            name, err = launch_role({**b, "docker": b.get("docker", os.environ.get("DOCKER", "sudo docker"))})
            return self._send(200 if name else 500,
                              {"name": name} if name else {"error": err})
        if self.path == "/rm":
            docker = self._docker()
            names = b.get("names")
            if not names:
                rc, out = sh(docker + ["ps", "-aq", "--filter", "name=rt-"])
                names = out.split()
            for x in names:
                sh(docker + ["rm", "-f", x])
            return self._send(200, {"removed": names})
        self._send(404, {"error": "not found"})


if __name__ == "__main__":
    print(f"[rt-agent] {socket.gethostname()} listening on :{PORT}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
