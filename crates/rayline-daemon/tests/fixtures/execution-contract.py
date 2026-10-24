#!/usr/bin/env python3
"""External executable contract fixture. No provider calls or real credentials."""
import argparse
import http.server
import json
import os
import pathlib
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--config", required=True)
parser.add_argument("--listen", required=True)
args = parser.parse_args()
config = json.loads(pathlib.Path(args.config).read_text())
events = pathlib.Path(config["events"])
token = os.environ["RAYLINE_EXEC_AUTH_TOKEN"]
credential_env = [entry["auth_env"] for entry in
                  [config.get("decisions", {})] + config.get("deployments", []) if entry.get("auth_env")]
pathlib.Path(args.config).with_suffix(".pid").write_text(str(os.getpid()))


def parent_watchdog():
    sys.stdin.buffer.read()
    os._exit(0)


if os.environ.get("RAYLINE_EXEC_PARENT_PIPE") == "1":
    threading.Thread(target=parent_watchdog, daemon=True).start()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    ready = True

    def log_message(self, *_):
        pass

    def authorized(self):
        if self.headers.get("Authorization") == "Bearer " + token:
            return True
        self.send_error(401)
        return False

    def do_GET(self):
        if not self.authorized():
            return
        body = json.dumps({"ready": Handler.ready, "contract_version": 1, "instance_id": f"fixture-{os.getpid()}",
                           "capabilities": config.get("capabilities", {"context_window": 16384, "max_output_tokens": 8192,
                                            "input_modalities": ["text"]})}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if not self.authorized():
            return
        if self.headers.get("Transfer-Encoding") == "chunked":
            chunks = []
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)
            body = b"".join(chunks)
        else:
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        with events.open("a") as output:
            output.write(json.dumps({"path": self.path, "body": body.decode(),
                                     "session": self.headers.get("x-rayline-session-id"),
                                     "branch": self.headers.get("x-rayline-branch-id"),
                                     "credential_env_present": [name for name in credential_env if os.environ.get(name)],
                                     "core_authenticated": True}) + "\n")
        if self.path == "/crash":
            os._exit(23)
        if self.path == "/unready":
            Handler.ready = False
        if self.path == "/pending":
            if not self.rfile.read(1):
                pathlib.Path(config["cancelled"] + "-pending").write_text("cancelled before headers")
            return
        if self.path == "/stream":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            try:
                for _ in range(2000):
                    self.wfile.write(b"data: " + b"x" * 8192 + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.01)
            except (BrokenPipeError, ConnectionResetError):
                pathlib.Path(config["cancelled"]).write_text("upstream observed disconnect")
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("x-rayline-selected-model", "fixture-model")
        self.end_headers()
        self.wfile.write(body)


class Server(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # A disconnected client is the expected cancellation fixture.
        pass


server = Server(("127.0.0.1", 0), Handler)
time.sleep(config.get("handshake_delay", 0))
print(json.dumps({"contract_version": config.get("contract_version", 1),
                  "listen_url": "http://127.0.0.1:" + str(server.server_port),
                  "pid": os.getpid()}), flush=True)
server.serve_forever()
