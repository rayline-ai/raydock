#!/usr/bin/env python3
"""Smoke test for a built rayline-arc-encoder.

    python3 encoder/smoke_test.py BINARY [MODEL.gguf]

Without a model it checks the usage text and that a missing model fails. With a model (any small GGUF works) it
also starts the server on a free port, checks /health, a one-shot and a retained /encode, a 409 for an unknown
session, and that the server stops when its stdin closes (--parent-pipe).
"""

import json
import struct
import subprocess
import sys
import urllib.error
import urllib.request


def post(port, path, body):
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), error.read()


def check(condition, message):
    if not condition:
        sys.exit(f"FAIL: {message}")
    print(f"ok: {message}")


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    binary = sys.argv[1]

    usage = subprocess.run([binary, "--help"], capture_output=True, text=True, timeout=30)
    check(usage.returncode == 2, "--help exits 2")
    for flag in ("--model", "--state-dir", "--parent-pipe", "--checkpoint"):
        check(flag in usage.stderr, f"usage names {flag}")

    missing = subprocess.run(
        [binary, "--model", "/nonexistent/model.gguf", "--port", "0"], capture_output=True, text=True, timeout=60
    )
    check(missing.returncode != 0 and missing.stdout == "", "a missing model fails before listening")

    if len(sys.argv) == 2:
        return
    model = sys.argv[2]
    server = subprocess.Popen(
        [binary, "--model", model, "--port", "0", "--ctx", "256", "--budget", "64", "--ubatch", "64", "--parent-pipe"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    try:
        port = json.loads(server.stdout.readline())["listening"]
        check(port > 0, f"listening on port {port}")

        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=30) as response:
            health = json.load(response)
        dim = health["dim"]
        check(dim > 0 and health["budget"] == 64 and health["max_model_len"] >= 256, f"/health {health}")

        tokens = [1, 2, 3, 4, 5, 6, 7, 8]
        status, headers, body = post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [3, 7], "retain": False})
        check(status == 200, "one-shot /encode answers 200")
        check(headers.get("x-rows") == "2" and headers.get("x-dim") == str(dim), "x-rows and x-dim headers")
        check(len(body) == 2 * dim * 4, "two float32 rows")
        rows = struct.unpack(f"<{2 * dim}f", body)
        check(all(value == value for value in rows), "rows are numbers")

        status, _, _ = post(port, "/encode", {"base": 0, "tokens": tokens[:4], "offsets": [3], "session": "s"})
        check(status == 200, "retained /encode answers 200")
        status, _, appended = post(port, "/encode", {"base": 4, "tokens": tokens[4:], "offsets": [7], "session": "s"})
        check(status == 200 and len(appended) == dim * 4, "an append to the session answers one row")
        status, _, _ = post(port, "/encode", {"base": 4, "tokens": [9], "offsets": [4], "session": "unknown"})
        check(status == 409, "an append to an unknown session answers 409")
        form = urllib.request.Request(f"http://127.0.0.1:{port}/encode", data=b"tokens=1")
        try:
            urllib.request.urlopen(form, timeout=30)
            check(False, "a non-JSON request is refused")
        except urllib.error.HTTPError as error:
            check(error.code == 415, "a non-JSON request is refused with 415")
        status, _, refused = post(port, "/encode", {"base": 0, "tokens": [1], "offsets": [0], "session": "x", "owner": 1})
        check(status == 422, "a non-string owner is refused")
        status, _, refused = post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [8], "retain": False})
        check(status == 422 and json.loads(refused)["error"] == "offsets_out_of_range", "an offset past the input 422")

        # Sessions share one KV cache of --ctx tokens: a session that does not fit evicts the least recently used.
        status, _, _ = post(port, "/encode", {"base": 0, "tokens": [5] * 100, "offsets": [99], "session": "t"})
        check(status == 200, "a second session fits")
        status, _, _ = post(port, "/encode", {"base": 0, "tokens": [6] * 200, "offsets": [199], "session": "u"})
        check(status == 200, "a session too big for the free KV space evicts older ones")

        server.stdin.close()
        check(server.wait(timeout=60) == 0, "stops when its stdin closes")
    finally:
        if server.poll() is None:
            server.kill()


if __name__ == "__main__":
    main()
