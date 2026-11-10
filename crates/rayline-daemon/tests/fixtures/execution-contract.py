#!/usr/bin/env python3
"""External executable contract fixture. No provider calls or real credentials."""
import argparse
import gzip
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
# Windows runs this script through a .cmd launcher, the process rld spawned.
core_pid = os.getppid() if config.get("launcher") else os.getpid()


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
        if self.path == "/pending" or (self.path == "/v1/messages" and json.loads(body).get("fixture_pending")):
            try:
                more = self.rfile.read(1)
            except (ConnectionResetError, ConnectionAbortedError):
                more = b""
            if not more:
                pathlib.Path(config["cancelled"] + "-pending").write_text("cancelled before headers")
            return
        if self.path == "/v1/messages" and json.loads(body).get("fixture_empty_status"):
            self.send_response(json.loads(body)["fixture_empty_status"])
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/v1/messages" and json.loads(body).get("fixture_deep"):
            # Valid JSON nested past serde_json's default recursion limit (128).
            deep = ('{"type":"message","content":[{"type":"tool_use","input":' + "[" * 200 + "0"
                    + "]" * 200 + "}]}").encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(deep)))
            self.end_headers()
            self.wfile.write(deep)
            return
        if self.path == "/v1/messages" and json.loads(body).get("fixture_gzip"):
            # Fixed mtime makes the encoded wire bytes independently assertable.
            encoded = bytearray(gzip.compress(b'{"usage":{"input_tokens":7,"output_tokens":3}}', mtime=0))
            encoded[9] = 255  # Portable gzip OS marker.
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Encoding", "gzip")
            if json.loads(body).get("fixture_eof"):
                self.send_header("Connection", "close")
                self.close_connection = True
            else:
                self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
            return
        if self.path == "/stream" or (self.path == "/v1/messages" and json.loads(body).get("stream")):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            try:
                for _ in range(2000):
                    self.wfile.write(b"data: " + b"x" * 8192 + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.01)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pathlib.Path(config["cancelled"]).write_text("upstream observed disconnect")
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("x-rayline-selected-model", "fixture-model")
        if config.get("derived_actions"):
            requested = json.loads(body)["model"]
            entry = next(row for row in config["derived_actions"] if row["model"] == requested)
            self.send_header("x-rayline-instance-id", f"fixture-{os.getpid()}")
            self.send_header("x-rayline-executed-model", entry["model"])
            self.send_header("x-rayline-executed-worker", entry["worker"])
            self.send_header("x-rayline-executed-action-id", "dynamic-registry-action")
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
                  "pid": core_pid}), flush=True)
server.serve_forever()
