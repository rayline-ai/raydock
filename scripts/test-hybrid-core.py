#!/usr/bin/env python3
"""Real Rayline CLI + rld + Go core, with external HTTP Decisions/inference fixtures.

This is not a live ARC/model evaluation. All manifests and requests are synthetic;
controls come from the public compiled registry supplied by the caller.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def digest(value):
    return hashlib.sha256(value).hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--registry", type=Path, required=True)
    parser.add_argument("--rayline", type=Path, default=ROOT / "target/debug/rayline")
    parser.add_argument("--rld", type=Path, default=ROOT / "target/debug/rld")
    parser.add_argument("--claude", action="store_true", help="also run the installed Claude Code harness")
    parser.add_argument("--pi", action="store_true", help="also run the installed Pi harness")
    parser.add_argument("--gateway-fixture-control-url", help="loopback test bootstrap for the real exact-dispatch app")
    parser.add_argument("--local-deployment", type=Path, help="qualified BYO deployment JSON for the actual harness turn")
    parser.add_argument("--local-auth-file", type=Path, help="private key file loaded into the deployment's auth_env")
    parser.add_argument("--artifacts-dir", type=Path, help="save synthetic request bodies without credentials for diagnosis")
    args = parser.parse_args()
    if args.artifacts_dir:
        args.artifacts_dir.mkdir(parents=True, exist_ok=True, mode=0o700)

    def save_artifact(name, value):
        if args.artifacts_dir:
            destination = args.artifacts_dir / name
            with os.fdopen(os.open(destination, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600), "wb") as stream:
                stream.write(encoded(value))
    if args.claude and args.pi:
        parser.error("choose one actual harness per run")
    if args.local_deployment and not (args.claude or args.pi):
        parser.error("--local-deployment requires --claude or --pi")
    if args.local_auth_file and not args.local_deployment:
        parser.error("--local-auth-file requires --local-deployment")
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
                           and instruction["rules"].get("emit") == "on_change_v1"
                           and "unit" not in instruction["rules"])
            else:
                matches = control["base"]["native"] == "default" and instruction is None
            if matches:
                return {"action_id": digest(encoded({"synthetic_model": model, "control_id": control_id})),
                        "model": model, "control_id": control_id, "control": control}
        raise AssertionError(f"public registry has no test control for {model}")

    local = action("qwen/qwen3.8-27b", "openai", False)
    cloud = action("moonshotai/kimi-k3", "openrouter", True)
    private_text = cloud["control"]["instruction"]["text"]
    actions = {"local": local, "cloud": cloud}
    offer_sha, profile_sha, issuer_sha = [digest(label.encode()) for label in
                                         ("synthetic-offer", "synthetic-profile", "synthetic-issuer")]
    manifest = {"schema_version": "rayline.arc-policy-package.v6",
                "package_id": digest(b"synthetic-test-package"),
                "thinking_controls_sha256": registry_sha, "actions": [local, cloud]}
    package_bytes = encoded(manifest)
    package_sha = digest(package_bytes)
    decisions, dispatches = [], []
    decision_times, first_completion = [], []
    harness_scenario = {"active": False, "turns": 0, "path": ""}
    cloud_key = "synthetic-cloud-credential"
    provider_key = cloud_key
    cloud_wire_model = "fixture-cloud-model"

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def send_json(self, value, headers=None):
            body = encoded(value)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json({"external_service_alive": True})

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/decisions":
                assert self.headers["Authorization"] == "Bearer synthetic-decisions-credential"
                decisions.append(body)
                save_artifact(f"decisions-{len(decisions)}.json", body)
                decision_times.append(time.monotonic())
                # Two cloud turns exercise private-append replay; other calls use Qwen.
                if harness_scenario["active"]:
                    harness_scenario["turns"] += 1
                    target = "local" if harness_scenario["turns"] == 1 else "cloud"
                else:
                    target = "cloud" if len(decisions) in (2, 3) else "local"
                selected = actions[target]
                execution = body["routing"]["execution"]
                held_action = execution["hold"]["action_id"] if execution["hold"] else (
                    execution["attribution"][-1]["action_id"] if execution["attribution"] else None)
                action_models = {action["action_id"]: action["model"] for action in actions.values()}
                worker_echo = []
                for worker in execution["workers"]:
                    held = action_models[worker["action_id"]] == action_models.get(held_action)
                    prefix = worker["current_prefix"]
                    qualified = prefix and prefix["provenance"] != "caller_estimated"
                    # This fixture declares conservative pricing defaults, never
                    # observed cache warmth. Qualified runtime counts remain exact.
                    worker_echo.append({"action_id": worker["action_id"], "worker": worker["worker"],
                        "read": True, "reason": None, "held": held, "basis": "unknown",
                        "pricing": "default_held_warm" if held else "default_rival_cold",
                        "p_warm": 1 if held else 0,
                        "prefix_tokens": prefix["tokens"] if qualified else 40,
                        "prefix_basis": "worker_count" if qualified else "encoder_fallback",
                        "not_admitted": []})
                self.send_json({"model": selected["model"], "worker": target,
                    "policy_action_id": selected["action_id"], "route_id": f"fixture-route-{len(decisions)}",
                    "identity": {"package_sha256": package_sha, "thinking_controls_sha256": registry_sha,
                        "offer_sha256": offer_sha, "action_id": selected["action_id"],
                        "policy_action_id": selected["action_id"], "control_id": selected["control_id"],
                        "arm_id": None, "prices": {"basis": "catalog", "price_sha256": None, "exclusive": False}},
                    "routing": {"unapplied_keys": [], "execution": {"status": "applied",
                        "turn": execution["turn"], "pinned": {name: "matched" for name in execution["expect"]},
                        "boundary": "user_message" if worker_echo else "task_start",
                        "user_turn_cache": "repartition_multiwarm", "user_turn_cache_source": "service_config",
                        "episode_identity": "per_lookup_random", "multiwarm": True, "workers": worker_echo}}})
                return
            target = self.path.removeprefix("/")
            assert target in actions, self.path
            dispatches.append({"target": target, "body": body})
            save_artifact(f"{target}-{len(decisions)}.json", body)
            headers = {}
            if target == "local":
                assert self.headers.get("Authorization") is None
            else:
                assert self.headers.get("Authorization") == "Bearer " + provider_key
                assert body["provider"] == {"only": ["moonshotai/mxfp4"], "allow_fallbacks": False}
                if args.gateway_fixture_control_url:
                    assert self.headers.get("X-Rayline-Worker") is None
                else:
                    assert self.headers["X-Rayline-Worker"] == "cloud"
                    headers = {"X-Rayline-Dispatch-Profile": profile_sha,
                               "X-Rayline-Dispatch-Issuer": issuer_sha,
                               "X-Rayline-Dispatched-Model": cloud_wire_model,
                               "X-Rayline-Worker": "cloud", "X-Rayline-Action-Id": cloud["action_id"]}
            if harness_scenario["active"] and harness_scenario["turns"] == 1:
                tool_name, parameter = ("read", "path") if args.pi else ("Read", "file_path")
                message = {"role": "assistant", "content": None, "tool_calls": [{"id": "call_native_read",
                    "type": "function", "function": {"name": tool_name, "arguments": json.dumps({parameter: harness_scenario["path"]})}}]}
                finish = "tool_calls"
            elif len(dispatches) == 1:
                # Age must start after this generation completes, not before it.
                time.sleep(1.1)
                message = {"role": "assistant", "content": None, "tool_calls": [{"id": "call_fixture",
                    "type": "function", "function": {"name": "lookup", "arguments": '{"name":"sample"}'}}]}
                finish = "tool_calls"
            else:
                message = {"role": "assistant", "content": "Synthetic execution complete."}
                finish = "stop"
            if len(dispatches) == 1:
                first_completion.append(time.monotonic())
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                for name, value in headers.items():
                    self.send_header(name, value)
                self.end_headers()
                if finish == "tool_calls":
                    delta = {"tool_calls": [dict(message["tool_calls"][0], index=0)]}
                else:
                    delta = {"content": message["content"]}
                for delta, reason, usage in [({"role": "assistant", "content": ""}, None, None),
                        (delta, None, None), ({}, finish, None),
                        (None, None, {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48})]:
                    chunk = {"id": "chatcmpl-stream", "object": "chat.completion.chunk", "created": 1,
                        "model": body["model"], "choices": [] if delta is None else
                        [{"index": 0, "delta": delta, "finish_reason": reason}]}
                    if usage:
                        chunk["usage"] = usage
                    self.wfile.write(b"data: " + encoded(chunk) + b"\n\n")
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                self.close_connection = True
                return
            self.send_json({"id": f"chatcmpl-{len(dispatches)}", "object": "chat.completion",
                "created": 1, "model": body["model"], "choices": [{"index": 0, "message": message,
                    "finish_reason": finish}], "usage": {"prompt_tokens": 40, "completion_tokens": 8,
                    "total_tokens": 48, "prompt_tokens_details": {"cached_tokens": 0}}}, headers)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    external = f"http://127.0.0.1:{server.server_port}"
    cloud_url = external + "/cloud"
    previous_usage_ids = set()
    if args.gateway_fixture_control_url:
        bootstrap = urllib.request.Request(args.gateway_fixture_control_url, data=encoded({
            "provider_fixture_url": cloud_url, "package_sha256": package_sha, "offer_id": offer_sha,
            "worker_id": "cloud", "action_id": cloud["action_id"], "control_id": cloud["control_id"],
            "control": cloud["control"], "native_body": {"provider": {"only": ["moonshotai/mxfp4"], "allow_fallbacks": False}}
        }), headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(bootstrap, timeout=30) as response:
                gateway = json.load(response)
        except urllib.error.HTTPError as error:
            server.shutdown()
            server.server_close()
            raise AssertionError(f"bootstrap HTTP {error.code}: {error.read().decode()}") from error
        cloud_url, profile_sha, issuer_sha = gateway["url"], gateway["profile_sha256"], gateway["issuer_sha256"]
        cloud_key, provider_key = gateway["api_key"], gateway["provider_api_key"]
        cloud_wire_model = gateway["wire_model"]
        usage_url = args.gateway_fixture_control_url.rsplit("/", 1)[0] + "/usage"
        with urllib.request.urlopen(usage_url, timeout=10) as response:
            previous_usage_ids = {row["id"] for row in json.load(response)["rows"]}
    with tempfile.TemporaryDirectory(prefix="rayline-real-core-") as tmp:
        root = Path(tmp)
        state = root / "state"
        package = root / "synthetic-package.json"
        package.write_bytes(package_bytes)
        config = {"contract_version": 1, "package_path": str(package), "package_sha256": package_sha,
            "registry_path": str(args.registry.resolve()), "registry_sha256": registry_sha,
            "allow_experimental_controls": True, "decisions": {"url": external + "/decisions",
                "api_mode": "execution_context_v1",
                "auth_env": "RAYLINE_TEST_DECISIONS_KEY", "model": "synthetic-arc-model", "result": "top",
                "routing": {}, "offer_sha256": offer_sha}, "bindings": [], "deployments": []}
        for name, selected in actions.items():
            config["bindings"].append({"action_id": selected["action_id"], "worker": name, "deployment_id": name})
            deployment = {"id": name, "kind": name, "url": external + "/" + name,
                "protocol": "openai.chat.v1", "wire_model": f"fixture-{name}-model",
                "registry_model": selected["model"], "registry_provider": "openai" if name == "local" else "openrouter",
                "runtime_identity": "synthetic-http-fixture", "max_output_tokens": 32768,
                "context_window": 65536, "input_modalities": ["text"],
                "token_units": "fixture-tokenizer"}
            if name == "cloud":
                deployment.update(auth_env="RAYLINE_TEST_CLOUD_KEY", dispatch_profile_sha256=profile_sha,
                                  issuer_sha256=issuer_sha, url=cloud_url, wire_model=cloud_wire_model,
                                  provider_pin={"only": ["moonshotai/mxfp4"], "allow_fallbacks": False})
            config["deployments"].append(deployment)
        config_path = root / "execution.json"
        config_path.write_bytes(encoded(config))
        env = dict(os.environ, HOME=str(root), RLD_BIN=str(args.rld.resolve()),
            RAYLINE_TEST_DECISIONS_KEY="synthetic-decisions-credential", RAYLINE_TEST_CLOUD_KEY=cloud_key,
            CLAUDE_CONFIG_DIR=str(root / "claude"), CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
        env.pop("RAYLINE_ROUTER_API_KEY", None)

        def cli(*parts):
            result = subprocess.run([str(args.rayline.resolve()), "hybrid", "--state-dir", str(state), *parts],
                                    env=env, cwd=root, capture_output=True, text=True, timeout=180)
            assert result.returncode == 0, (result.stdout, result.stderr,
                (state / "runtime.log").read_text() if (state / "runtime.log").exists() else "")
            return result.stdout

        def start():
            cli("start", "--core", str(args.core.resolve()), "--config", str(config_path), "--port", "0")
            return json.loads((state / "ready.json").read_text())

        current = start()
        try:
            def request(path, body, session="synthetic-tool-session"):
                token = (state / "gateway-token").read_text()
                req = urllib.request.Request(current["listen_url"] + path, data=encoded(body), headers={
                    "Content-Type": "application/json", "Authorization": "Bearer " + token,
                    "x-rayline-session-id": session})
                try:
                    with urllib.request.urlopen(req, timeout=120 if args.local_deployment else 15) as response:
                        return json.load(response), dict(response.headers)
                except urllib.error.HTTPError as error:
                    raise AssertionError(f"HTTP {error.code}: {error.read().decode()}") from error

            messages = [{"role": "user", "content": "Call lookup for sample, then report its result."}]
            base = {"model": "rayline-router", "max_tokens": 512, "tools": [{"name": "lookup",
                "description": "Lookup a synthetic record", "input_schema": {"type": "object",
                    "properties": {"name": {"type": "string"}}, "required": ["name"]}}]}
            one, _ = request("/v1/messages", dict(base, messages=messages))
            assert one["stop_reason"] == "tool_use", one
            tool = next(block for block in one["content"] if block["type"] == "tool_use")
            assert tool["id"] == "call_fixture" and tool["input"] == {"name": "sample"}
            messages += [{"role": "assistant", "content": one["content"]},
                         {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool["id"], "content": "record found"}]}]
            time.sleep(0.12)
            two, _ = request("/v1/messages", dict(base, messages=messages))
            second = decisions[1]["routing"]["execution"]
            assert second["hold"]["action_id"] == local["action_id"]
            assert len(second["workers"]) == 1, second
            evidence = second["workers"][0]
            completion_gap = decision_times[1] - first_completion[0]
            assert abs(evidence["gap_s"] - completion_gap) < 0.5, (evidence, completion_gap)
            assert evidence["ttl"] is None and evidence["residency"] is None
            assert evidence["usage"]["prompt_tokens"] == 40
            assert second["attribution"] and second["attribution"][0]["action_id"] == local["action_id"], second
            assert private_text in json.dumps(dispatches[1]["body"])
            assert any(m["role"] == "tool" and m.get("tool_call_id") == "call_fixture"
                       for m in dispatches[1]["body"]["messages"])
            messages += [{"role": "assistant", "content": two["content"]}, {"role": "user", "content": "Confirm once more."}]
            three, _ = request("/v1/messages", dict(base, messages=messages))
            assert json.dumps(dispatches[2]["body"]).count(private_text) == 1
            messages += [{"role": "assistant", "content": three["content"]}, {"role": "user", "content": "Return to the local worker."}]
            request("/v1/messages", dict(base, messages=messages))
            assert private_text not in json.dumps(dispatches[3]["body"])
            assert private_text not in json.dumps(decisions)
            chat, _ = request("/v1/chat/completions", {"model": "rayline-router", "messages": [{"role": "user", "content": "chat wire"}]}, "chat-session")
            assert chat["choices"][0]["message"]["content"] == "Synthetic execution complete."
            responses, _ = request("/v1/responses", {"model": "rayline-router", "input": "responses wire"}, "responses-session")
            assert responses["output"][0]["content"][0]["text"] == "Synthetic execution complete."
            assert [d["target"] for d in dispatches[:4]] == ["local", "cloud", "cloud", "local"]
            actual_local = None
            if args.local_deployment:
                # The original fixture suite finishes unchanged. A new core then
                # qualifies and binds the caller-owned real runtime for the selected harness.
                cli("stop")
                actual_local = json.loads(args.local_deployment.read_text())
                assert actual_local["kind"] == "local" and actual_local["registry_model"] == local["model"]
                actual_local["id"] = "local"
                config["deployments"] = [actual_local if d["id"] == "local" else d for d in config["deployments"]]
                config_path.write_bytes(encoded(config))
                if args.local_auth_file:
                    env[actual_local["auth_env"]] = args.local_auth_file.read_text().strip()
                env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = str(min(2048, actual_local["max_output_tokens"]))
                current = start()
            if args.claude or args.pi:
                before_harness_dispatches = len(dispatches)
                sample = root / "synthetic.txt"
                sample.write_text("SYNTHETIC-FILE-READ-PROOF\n")
                harness_scenario.update(active=True, path=str(sample))
                prompt = f"Read {sample} and then reply with Synthetic execution complete."
                if args.pi:
                    output = cli("run", "pi", "--", "--provider", "rayline", "--model", "rayline-router",
                        "--no-session", "--offline", "--no-extensions", "--no-skills", "--no-prompt-templates",
                        "--no-context-files", "--tools", "read", "--thinking", "off", "--print", prompt)
                else:
                    output = cli("run", "claude", "--", "--print", "--max-turns", "3", "--tools", "Read",
                        "--allowedTools", "Read", "--no-session-persistence", "--setting-sources", "", prompt)
                assert "Synthetic execution complete." in output, output
                assert harness_scenario["turns"] == 2, harness_scenario
                expected_fixture_targets = ["cloud"] if actual_local else ["local", "cloud"]
                assert [d["target"] for d in dispatches[before_harness_dispatches:]] == expected_fixture_targets
                assert "SYNTHETIC-FILE-READ-PROOF" in json.dumps(dispatches[-1]["body"])
                assert private_text in json.dumps(dispatches[-1]["body"])
                if actual_local:
                    evidence = next(worker for worker in decisions[-1]["routing"]["execution"]["workers"] if worker["worker"] == "local")
                    assert evidence["usage"]["prompt_tokens"] > 0
                    assert evidence["current_prefix"]["tokens"] > 0
                    assert evidence["current_prefix"]["units"] == actual_local["token_units"]
            if args.gateway_fixture_control_url:
                expected_count = sum(dispatch["target"] == "cloud" for dispatch in dispatches)
                deadline = time.monotonic() + 5
                while True:
                    with urllib.request.urlopen(usage_url, timeout=10) as response:
                        audit = json.load(response)
                    rows = [row for row in audit["rows"] if row["id"] not in previous_usage_ids]
                    billed = sum(row.get("billedNanodollars", 0) for row in rows)
                    if len(rows) == expected_count and 1_000_000_000 - audit["balance"]["balance_nanodollars"] == billed:
                        break
                    assert time.monotonic() < deadline, "exact-dispatch usage/debit did not settle once per cloud call"
                    time.sleep(0.05)
                for row in rows:
                    assert row["status"] == "success" and row["inputTokens"] == 40 and row["outputTokens"] == 8
                    assert row["selectedModel"] == cloud["model"] and row["selectedProvider"] == "openrouter"
                    receipt = row["exactDispatch"]
                    assert receipt["profileSha256"] == profile_sha and receipt["actionId"] == cloud["action_id"]
                    assert receipt["issuerSha256"] == issuer_sha and receipt["packageSha256"] == package_sha
            previous_token = (state / "gateway-token").read_text()
            cli("stop")
            with urllib.request.urlopen(external + "/health") as response:
                assert json.load(response)["external_service_alive"]
            if actual_local:
                health_url = actual_local["url"].rsplit("/v1/chat/completions", 1)[0] + "/health"
                headers = {"Authorization": "Bearer " + env[actual_local["auth_env"]]} if actual_local.get("auth_env") else {}
                with urllib.request.urlopen(urllib.request.Request(health_url, headers=headers), timeout=10) as response:
                    assert response.status == 200
            old_instance = current["instance_id"]
            current = start()
            assert current["instance_id"] != old_instance
            stale_health = urllib.request.Request(current["listen_url"] + "/healthz",
                headers={"Authorization": "Bearer " + previous_token})
            try:
                urllib.request.urlopen(stale_health)
            except urllib.error.HTTPError as error:
                assert error.code == 401
            else:
                raise AssertionError("new gateway admitted a stale harness credential")
            try:
                request("/v1/messages", dict(base, messages=messages))
            except AssertionError as error:
                assert "HTTP 409" in str(error) and "no execution ledger" in str(error), error
            else:
                raise AssertionError("fresh core silently resumed an unavailable private ledger")
            harness_scenario["active"] = False
            request("/v1/messages", dict(base, messages=[{"role": "user", "content": "Fresh session after restart"}]), "fresh-after-restart")
            fresh = decisions[-1]["routing"]["execution"]
            assert fresh["workers"] == [] and fresh["attribution"] == [] and fresh["hold"] is None
            os.kill(current["pid"], signal.SIGKILL)
            deadline = time.monotonic() + 10
            while True:
                proc = subprocess.run(["ps", "-o", "stat=", "-p", str(current["core_pid"])], capture_output=True)
                if proc.returncode or b"Z" in proc.stdout:
                    break
                assert time.monotonic() < deadline, "real Go core survived closed parent pipe"
                time.sleep(0.05)
            cli("stop")
        finally:
            cli("stop")
            server.shutdown()
            server.server_close()
    print("Real Rust + Go hybrid acceptance passed: three wires, local/cloud/local, tools, private append, completion age, external ownership, restart and parent death. HTTP Decisions and the baseline model responses were synthetic fixtures.")
    if args.claude:
        print("Installed Claude Code completed a real Read tool exchange across local/cloud execution.")
    if args.pi:
        print("Installed Pi completed a real read tool exchange across local/cloud execution.")
    if args.local_deployment:
        print("The selected harness local turn used the caller-owned real Qwen runtime with qualified prompt counting; it remained healthy after Rayline stopped.")
    if args.gateway_fixture_control_url:
        print("Actual exact-dispatch gateway settled one receipt and balance debit per cloud call.")


if __name__ == "__main__":
    main()
