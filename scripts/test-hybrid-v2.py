#!/usr/bin/env python3
"""Joined v2 smoke: real Rayline CLI + rld + Go core + installed Pi, with HTTP fixtures.

Exercises the seams the hybrid v2 spec adds, end to end:
  * `rayline hybrid start` composes Rayline Local's defaults into the core config
    (decisions.api_mode execution_context_v2, explicit routing.policy preset, and an
    optional settings `hybrid.routing` schedule override);
  * `rayline hybrid run pi` loads the event extension; Pi's typed events reach the
    core as `rayline_event` and Decisions sees them as `basis: explicit_client`;
  * the core strips the member before Decisions and inference, and sends the v2
    envelope (turn, event, last_served, history, targets) and validates the receipt.

Decisions and model responses are synthetic fixtures; this is not a live ARC or
model-quality evaluation, and capacity facts (which need a qualified llama.cpp
counter) are covered by the Go core's own tests and the live acceptance.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[1]
PRESET = "arc-policy.hybrid-default.v1"


def digest(value):
    return hashlib.sha256(value).hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True, help="rayline-core built from the v2 Go core")
    parser.add_argument("--registry", type=Path, required=True, help="public compiled thinking registry")
    parser.add_argument("--rayline", type=Path, default=ROOT / "target/debug/rayline")
    parser.add_argument("--rld", type=Path, default=ROOT / "target/debug/rld")
    args = parser.parse_args()
    registry_bytes = args.registry.read_bytes()
    registry = json.loads(registry_bytes)
    registry_sha = digest(registry_bytes)

    def action(model, provider, steer):
        cell = next(c for c in registry["cells"] if c["model"] == model
                    and c["provider"] == provider and c["format"] == "chat")
        for control_id in cell["controls"]:
            control = registry["controls"][control_id]
            instruction = control["instruction"]
            if steer:
                matches = (instruction and instruction["level"] == "up"
                           and instruction["rules"].get("emit") == "on_change_v1" and "unit" not in instruction["rules"])
            else:
                matches = control["base"]["native"] == "default" and instruction is None
            if matches:
                return {"action_id": digest(encoded({"synthetic_model": model, "control_id": control_id})),
                        "model": model, "control_id": control_id, "control": control}
        raise AssertionError(f"public registry has no test control for {model}")

    local = action("qwen/qwen3.8-27b", "openai", False)
    cloud = action("moonshotai/kimi-k3", "openrouter", True)
    manifest = {"schema_version": "rayline.arc-policy-package.v6", "package_id": digest(b"synthetic-v2-package"),
                "thinking_controls_sha256": registry_sha, "actions": [local, cloud]}
    package_bytes = encoded(manifest)
    package_sha = digest(package_bytes)
    decisions, dispatches = [], []
    scenario = {"path": "", "turns": 0}

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def send_json(self, value):
            body = encoded(value)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            body = json.loads(raw)
            if self.path == "/decisions":
                decisions.append(body)
                execution, policy = body["routing"]["execution"], body["routing"]["policy"]
                self.send_json({"model": local["model"], "worker": "local", "policy_action_id": local["action_id"],
                    "route_id": f"fixture-route-{len(decisions)}",
                    "identity": {"package_sha256": package_sha, "thinking_controls_sha256": registry_sha,
                        "offer_sha256": None, "action_id": local["action_id"], "policy_action_id": local["action_id"],
                        "control_id": local["control_id"], "arm_id": None,
                        "prices": {"basis": "catalog", "price_sha256": None, "exclusive": None}},
                    "receipt": {"action_id": local["action_id"], "control_id": local["control_id"],
                        "policy": policy["preset"], "decided_by": {"model": "schedule", "thinking": "schedule"},
                        "eligible": {"model": None, "thinking": "all_turns"}, "held_from": None, "excluded": [],
                        "basis": execution["event"]["basis"]},
                    "routing": {"unapplied_keys": [],
                        "execution": {"status": "applied", "version": "arc-execution-context.v2", "turn": execution["turn"],
                            "pinned": {"package_sha256": "matched", "thinking_controls_sha256": "matched", "offer_sha256": None}},
                        "policy": {"status": "applied", "preset": policy["preset"]}}})
                return
            # local_model's base URL plus the protocol path (from_local_model).
            assert self.path == "/v1/chat/completions", self.path
            dispatches.append(body)
            scenario["turns"] += 1
            if scenario["turns"] == 1:
                message = {"role": "assistant", "content": None, "tool_calls": [{"id": "call_native_read", "type": "function",
                    "function": {"name": "read", "arguments": json.dumps({"path": scenario["path"]})}}]}
                finish = "tool_calls"
            else:
                message, finish = {"role": "assistant", "content": "Synthetic execution complete."}, "stop"
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                delta = {"tool_calls": [dict(message["tool_calls"][0], index=0)]} if finish == "tool_calls" else {"content": message["content"]}
                for delta, reason, usage in [({"role": "assistant", "content": ""}, None, None), (delta, None, None),
                        ({}, finish, None), (None, None, {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48})]:
                    chunk = {"id": "chatcmpl-stream", "object": "chat.completion.chunk", "created": 1, "model": body["model"],
                             "choices": [] if delta is None else [{"index": 0, "delta": delta, "finish_reason": reason}]}
                    if usage:
                        chunk["usage"] = usage
                    self.wfile.write(b"data: " + encoded(chunk) + b"\n\n")
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                self.close_connection = True
                return
            self.send_json({"id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": body["model"],
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48}})

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    external = f"http://127.0.0.1:{server.server_port}"
    with tempfile.TemporaryDirectory(prefix="rayline-v2-") as tmp:
        root = Path(tmp)
        state = root / "state"
        package = root / "synthetic-package.json"
        package.write_bytes(package_bytes)
        # No api_mode and no policy: Rayline Local must supply both.
        config = {"contract_version": 1, "package_path": package.name, "package_sha256": package_sha,
            "registry_path": str(args.registry.resolve()), "registry_sha256": registry_sha,
            "allow_experimental_controls": True,
            "decisions": {"url": external + "/decisions", "auth_env": "RAYLINE_TEST_DECISIONS_KEY",
                          "model": "synthetic-arc-model", "result": "top", "routing": {}},
            "bindings": [{"action_id": local["action_id"], "worker": "local", "deployment_id": "local"},
                         {"action_id": cloud["action_id"], "worker": "cloud", "deployment_id": "cloud"}],
            "deployments": [
                {"id": "local", "kind": "local", "from_local_model": True, "url": "http://127.0.0.1:9/unused",
                 "protocol": "openai.chat.v1", "wire_model": "unused", "registry_model": local["model"],
                 "registry_provider": "openai", "runtime_identity": "synthetic-http-fixture",
                 "max_output_tokens": 8192, "context_window": 65536, "input_modalities": ["text"], "token_units": "fixture-tokenizer"},
                {"id": "cloud", "kind": "cloud", "url": external + "/cloud", "protocol": "openai.chat.v1",
                 "wire_model": "fixture-cloud-model", "registry_model": cloud["model"], "registry_provider": "openrouter",
                 "runtime_identity": "synthetic-http-fixture", "max_output_tokens": 8192, "context_window": 65536,
                 "input_modalities": ["text"], "token_units": "fixture-cloud-tokenizer", "auth_env": "RAYLINE_TEST_CLOUD_KEY",
                 "dispatch_profile_sha256": digest(b"profile"), "issuer_sha256": digest(b"issuer"),
                 "provider_pin": {"only": ["moonshotai/mxfp4"], "allow_fallbacks": False}}]}
        config_path = root / "profile.json"
        config_path.write_bytes(encoded(config))
        settings_dir = root / ".config" / "rayline"
        settings_dir.mkdir(parents=True)
        settings = {"local_model": {"mode": "custom", "base_url": external, "model": "fixture-local-qwen"},
                    "hybrid": {"routing": {"schedule": {"model": ["cache_boundaries", "user_messages"]}}}}
        (settings_dir / "settings.json").write_text(json.dumps(settings))
        env = dict(os.environ, HOME=str(root), RLD_BIN=str(args.rld.resolve()),
                   RAYLINE_TEST_DECISIONS_KEY="synthetic-decisions-credential", RAYLINE_TEST_CLOUD_KEY="synthetic-cloud")
        env.pop("RAYLINE_ROUTER_API_KEY", None)

        def cli(*parts):
            result = subprocess.run([str(args.rayline.resolve()), "hybrid", "--state-dir", str(state), *parts],
                                    env=env, cwd=root, capture_output=True, text=True, timeout=180)
            assert result.returncode == 0, (result.stdout, result.stderr,
                (state / "runtime.log").read_text() if (state / "runtime.log").exists() else "")
            return result.stdout

        started = cli("start", "--core", str(args.core.resolve()), "--config", str(config_path), "--port", "0")
        try:
            assert f"Routing policy: {PRESET} (settings hybrid.routing)" in started, started
            assert "Local model: fixture-local-qwen" in started, started
            composed = json.loads((state / "core-config.json").read_text())
            assert composed["decisions"]["api_mode"] == "execution_context_v2"
            assert composed["deployments"][0]["url"] == external + "/v1/chat/completions"
            sample = root / "synthetic.txt"
            sample.write_text("SYNTHETIC-FILE-READ-PROOF\n")
            scenario["path"] = str(sample)
            output = cli("run", "pi", "--", "--provider", "rayline", "--model", "rayline-router", "--no-session",
                         "--offline", "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-context-files",
                         "--tools", "read", "--thinking", "off", "--print",
                         f"Read {sample} and then reply with Synthetic execution complete.")
            assert "Synthetic execution complete." in output, output
        finally:
            cli("stop")
            server.shutdown()
            server.server_close()
    assert len(decisions) == 2 and len(dispatches) == 2, (len(decisions), len(dispatches))
    first, second = (d["routing"]["execution"] for d in decisions)
    for body in decisions:
        assert body["routing"]["policy"] == {"preset": PRESET, "overrides": {"schedule": {"model": ["cache_boundaries", "user_messages"]}}}
        assert "rayline_event" not in body
    for body in dispatches:
        assert "rayline_event" not in body
    assert first["version"] == "arc-execution-context.v2"
    assert first["event"] == {"kind": "task_start", "basis": "explicit_client", "compaction": None}, first["event"]
    assert second["event"] == {"kind": "tool_result", "basis": "explicit_client", "compaction": None}, second["event"]
    assert first["last_served"] is None and second["last_served"]["action_id"] == local["action_id"]
    assert [entry["action_id"] for entry in second["history"]] == [local["action_id"]]
    assert second["turn"]["epoch"] == first["turn"]["epoch"] and second["turn"]["attempt"] == 1
    assert [target["placement"] for target in second["targets"]] in (["local", "cloud"], ["cloud", "local"])
    assert all(target["capacity"] is None for target in second["targets"])
    print("Joined v2 smoke passed: Rayline Local composed the hybrid preset and schedule override, the installed Pi "
          "extension's explicit events reached Decisions, and the core stripped them before Decisions and inference. "
          "Decisions and model responses were synthetic fixtures.")


if __name__ == "__main__":
    main()
