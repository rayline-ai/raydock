#!/usr/bin/env python3
"""Smoke test for a built rayline-arc-encoder.

    python3 encoder/smoke_test.py BINARY [MODEL.gguf]

Without a model it checks the usage text and that a missing model fails. With a model (any small GGUF works) it
also starts the server on a free port, checks /health, a one-shot and a retained /encode, a 409 for an unknown
session, request validation, KV-space eviction, that the server stops when its stdin closes (--parent-pipe), and
saved sessions (--state-dir).
"""

import json
import os
import stat
import struct
import subprocess
import sys
import tempfile
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


def start(binary, model, *extra):
    server = subprocess.Popen(
        [binary, "--model", model, "--port", "0", "--ctx", "256", "--budget", "64", "--ubatch", "64", "--parent-pipe",
         *extra],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    return server, json.loads(server.stdout.readline())["listening"]


def stop(server):
    server.stdin.close()
    check(server.wait(timeout=60) == 0, "stops when its stdin closes")


def refused_state_dir(binary, model, directory, why):
    result = subprocess.run(
        [binary, "--model", model, "--port", "0", "--ctx", "256", "--budget", "64", "--ubatch", "64",
         "--state-dir", directory],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    check(result.returncode != 0 and result.stdout == "" and "refusing" in result.stderr, f"--state-dir {why} is refused")


def saved_sessions(binary, model):
    """--state-dir: owner-only files, restore across restarts, and no restore under another configuration."""
    with tempfile.TemporaryDirectory() as root:
        state = ["--state-dir", root, "--persist-min", "1"]
        tokens = list(range(10, 50))
        server, port = start(binary, model, *state)
        try:
            check(post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [39], "session": "A", "owner": "a"})[0]
                  == 200, "session A encodes")
            post(port, "/close", {"owner": "a"})
            status, headers, _ = post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [39], "session": "B",
                                                        "owner": "b"})
            check(status == 200 and headers.get("x-restored-tokens") == "40", "session B restores A's saved prefix")
            post(port, "/close", {"owner": "b"})
        finally:
            if server.poll() is None:
                stop(server)
        (subdir,) = [entry for entry in os.scandir(root) if entry.is_dir()]
        if os.name == "posix":
            check(stat.S_IMODE(os.stat(subdir.path).st_mode) == 0o700, "the saved-session directory is owner-only")
            modes = {stat.S_IMODE(os.stat(f.path).st_mode) for f in os.scandir(subdir.path)}
            check(modes == {0o600}, f"saved-session files are owner-only {sorted(oct(m) for m in modes)}")

        server, port = start(binary, model, *state)
        try:
            for session in ("A", "B"):
                status, _, _ = post(port, "/encode", {"base": 40, "tokens": [7], "offsets": [40], "session": session})
                check(status == 200, f"after a restart, session {session} resumes from its own saved state")
        finally:
            stop(server)

        # A session restarted with new content and then lost (killed before a save) never resumes its old content.
        server, port = start(binary, model, *state)
        try:
            new = list(range(60, 100))
            check(post(port, "/encode", {"base": 0, "tokens": new, "offsets": [39], "session": "A"})[0] == 200,
                  "session A restarts with new content")
        finally:
            server.kill()
            server.wait(timeout=60)
        server, port = start(binary, model, *state)
        try:
            # A's old content was saved at 41 tokens: an append there must not resume it.
            status, _, _ = post(port, "/encode", {"base": 41, "tokens": [7], "offsets": [41], "session": "A"})
            check(status == 409, "an append after the lost restart does not resume A's old content")
            # (A and B were saved at 41 tokens, tokens + [7], by the restart above.)
            status, headers, _ = post(port, "/encode", {"base": 0, "tokens": tokens + [7, 8], "offsets": [41],
                                                        "retain": False})
            check(status == 200 and headers.get("x-restored-tokens") == "41", "the old content still serves as a prefix")

            # A second encoder on the same directory runs without saved sessions instead of racing the first.
            files = sorted(os.listdir(subdir.path))
            second, second_port = start(binary, model, *state)
            try:
                status, _, _ = post(second_port, "/encode", {"base": 0, "tokens": list(range(100, 140)),
                                                             "offsets": [39], "session": "C", "owner": "c"})
                post(second_port, "/close", {"owner": "c"})
                check(status == 200, "a second encoder on the same state dir still encodes")
            finally:
                stop(second)
            check(sorted(os.listdir(subdir.path)) == files, "the second encoder neither saves nor cleans up there")
        finally:
            stop(server)

        if os.name == "posix":
            link = os.path.join(root, "link")
            os.symlink(subdir.path, link)
            refused_state_dir(binary, model, link, "that is a symlink")
            shared = os.path.join(root, "shared")
            os.mkdir(shared)
            os.chmod(shared, 0o777)
            refused_state_dir(binary, model, shared, "that others can write to")

        # A session larger than the whole --state-max-gb budget is never saved.
        with tempfile.TemporaryDirectory() as small:
            server, port = start(binary, model, "--state-dir", small, "--persist-min", "1", "--state-max-gb", "0.000001")
            try:
                post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [39], "session": "big", "owner": "big"})
                post(port, "/close", {"owner": "big"})
            finally:
                stop(server)
            saved = [f for d in os.scandir(small) if d.is_dir() for f in os.listdir(d.path) if f != "lock"]
            check(saved == [], f"nothing is saved beyond --state-max-gb {saved}")

        server, port = start(binary, model, *state, "--ubatch", "32")
        try:
            status, _, _ = post(port, "/encode", {"base": 41, "tokens": [8], "offsets": [41], "session": "A"})
            check(status == 409, "another encode configuration does not restore those sessions")
        finally:
            stop(server)


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

    # A decode chunk (--budget) larger than the context is refused at start, not aborted on the first long request.
    too_big = subprocess.run(
        [binary, "--model", model, "--port", "0", "--ctx", "200", "--budget", "256", "--parent-pipe"],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    check(too_big.returncode != 0 and too_big.stdout == "", "--budget larger than --ctx is refused at start")
    # --budget equal to an unpadded --ctx: a request filling the (padded) context is served or refused, never aborts.
    server, port = start(binary, model, "--ctx", "250", "--budget", "250")
    try:
        status, _, _ = post(port, "/encode", {"base": 0, "tokens": [2] * 256, "offsets": [255], "retain": False})
        check(status in (200, 422) and server.poll() is None, f"a context-filling request answers {status}, no abort")
    finally:
        stop(server)
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
        status, _, _ = post(port, "/encode", {"base": 0, "tokens": [1.5], "offsets": [0], "retain": False})
        check(status == 422, "a non-integer token is refused")
        rebound = urllib.request.Request(
            f"http://127.0.0.1:{port}/encode", data=b"{}",
            headers={"Content-Type": "application/json", "Host": f"rebound.example:{port}"})
        try:
            urllib.request.urlopen(rebound, timeout=30)
            check(False, "a foreign Host is refused")
        except urllib.error.HTTPError as error:
            check(error.code == 403, "a foreign Host is refused with 403")
        status, _, refused = post(port, "/encode", {"base": 0, "tokens": [1], "offsets": [0], "session": "x", "owner": 1})
        check(status == 422, "a non-string owner is refused")
        status, _, refused = post(port, "/encode", {"base": 0, "tokens": tokens, "offsets": [8], "retain": False})
        check(status == 422 and json.loads(refused)["error"] == "offsets_out_of_range", "an offset past the input 422")

        # A refused append changes nothing: it must not evict a held session to make room first.
        status, _, _ = post(port, "/encode", {"base": 0, "tokens": [3] * 40, "offsets": [39], "session": "held"})
        check(status == 200, "session 'held' encodes")
        status, _, _ = post(port, "/encode", {"base": 5, "tokens": [4] * 230, "offsets": [5], "session": "unknown2"})
        check(status == 409, "an append to an unknown session that would need eviction answers 409")
        status, _, _ = post(port, "/encode", {"base": 40, "tokens": [5], "offsets": [40], "session": "held"})
        check(status == 200, "the refused append left session 'held' in place")

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
    saved_sessions(binary, model)


if __name__ == "__main__":
    main()
