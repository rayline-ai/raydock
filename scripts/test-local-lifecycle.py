#!/usr/bin/env python3
"""Exercise `raydock local` and `run --local` against the execution-contract fixture core.

Run after `cargo build --locked -p rayline-cli`.
This verifies supervision and harness wiring; the VSR core suite verifies routing.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "target/debug/raydock"
# A core that speaks contract version 99. Like the fixture, it writes its pid
# beside the config and exits when the local process closes its stdin.
INCOMPATIBLE_CORE = """#!/usr/bin/env python3
import json, os, pathlib, sys, threading, time
config = pathlib.Path(sys.argv[sys.argv.index("--config") + 1])
config.with_suffix(".pid").write_text(str(os.getpid()))
if os.environ.get("RAYLINE_EXEC_PARENT_PIPE") == "1":
    threading.Thread(target=lambda: (sys.stdin.buffer.read(), os._exit(0)), daemon=True).start()
print(json.dumps({"contract_version": 99, "listen_url": "http://127.0.0.1:9", "pid": os.getpid()}), flush=True)
time.sleep(3600)
"""


def main():
    with tempfile.TemporaryDirectory(prefix="rayline-local-cli-") as tmp:
        root = Path(tmp).resolve()
        # The local process keeps its state in ~/.rayline/local.
        state = root / ".rayline" / "local"
        env = dict(os.environ, HOME=str(root))
        env.pop("OPENCLAW_CONTAINER", None)
        # The local process runs from raydock and finds rayline-core beside it.
        installed = root / "installed"
        installed.mkdir()
        raydock = installed / "raydock"
        shutil.copy(CLI, raydock)
        # The endpoints' own keys come from the shell; the router key comes
        # only from the saved credentials, never from the shell.
        local_env = ["RAYLINE_TEST_LOCAL_ALIAS", "ANTHROPIC_AUTH_TOKEN"]
        credential_env = sorted(local_env + ["RAYLINE_ROUTER_API_KEY"])
        for name in local_env:
            env[name] = "synthetic-runtime-only-" + name
        for name in ("RAYLINE_ROUTER_API_KEY", "RAYLINE_EXEC_AUTH_TOKEN"):
            env.pop(name, None)
        core = installed / "rayline-core"
        shutil.copy(ROOT / "crates/rayline-cli/tests/fixtures/execution-contract.py", core)
        core.chmod(0o700)
        # Decisions go to the loopback `fixture` router, which start does not probe.
        settings = root / ".config/rayline/settings.json"
        settings.parent.mkdir(parents=True)
        credentials = settings.parent / "credentials.json"
        credentials.write_text(json.dumps({"environments": {"fixture": {
            "session": {"email": "fixture@example.com", "subject": "fixture",
                        "access_token": "synthetic-access", "access_token_expires_at": "2100-01-01T00:00:00Z",
                        "refresh_token": "synthetic-refresh", "refresh_token_expires_at": "2100-01-01T00:00:00Z",
                        "signed_in_at": "2023-11-14T22:13:20Z"},
            "router_key": {"api_key": "synthetic-saved-router-key", "subject": "fixture",
                           "saved_at": "2023-11-14T22:13:20Z"}}}}))
        credentials.chmod(0o600)
        # Both endpoints list the protocols of the harnesses run below: claude
        # and pi speak anthropic_messages, openclaw speaks openai_chat.
        settings.write_text(json.dumps({
            "environments": {"fixture": {"base_url": "http://127.0.0.1:9",
                                         "login_url": "http://127.0.0.1:9/cli-auth"}},
            "endpoints": {
                "box": {"base_url": "http://127.0.0.1:9",
                        "protocols": ["openai_chat", "anthropic_messages"],
                        "api_key_env": "RAYLINE_TEST_LOCAL_ALIAS"},
                "gateway": {"base_url": "http://127.0.0.1:9",
                            "protocols": ["anthropic_messages", "openai_chat"],
                            "api_key_env": "ANTHROPIC_AUTH_TOKEN"},
            },
            "models": {
                "z-ai/glm-5.3-flash": {"endpoint": "box", "model": "glm",
                                       "context_window": 131072, "max_output_tokens": 8192},
                "qwen/qwen3.8-27b": {"endpoint": "gateway", "model": "qwen",
                                     "context_window": 98304, "max_output_tokens": 8192},
            },
            "routing": {"decisions_model": "arc-swift-1.0",
                        "classifier_model": "anthropic/claude-haiku-5.5",
                        "policy": {"preset": "arc-policy.prefer-local.v1"}},
        }))

        def raydock_cmd(*args, success=True):
            result = subprocess.run([str(raydock), *args], env=env, capture_output=True, text=True, timeout=75)
            assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
            return result.stdout if success else result.stderr

        def local(*args, success=True):
            return raydock_cmd("local", *args, success=success)

        def run(harness, *args, success=True):
            return raydock_cmd("--env", "fixture", "run", harness, "--local", *args, success=success)

        def start():
            raydock_cmd("--env", "fixture", "local", "start")
            return json.loads((state / "ready.json").read_text())

        def wait_dead(pid):
            deadline = time.monotonic() + 10
            while True:
                proc = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True)
                if proc.returncode or b"Z" in proc.stdout:
                    return
                assert time.monotonic() < deadline, f"process {pid} survived shutdown"
                time.sleep(0.05)

        first = start()
        try:
            assert local("status").splitlines() == [
                f"Local process: running (pid {first['pid']})",
                f"Address: {first['listen_url']}",
                f"Config: {settings}",
                "Environment: fixture",
            ]
            refused = raydock_cmd("--env", "fixture", "local", "start", success=False)
            assert str(first["pid"]) in refused and str(settings) in refused, refused
            token = (state / "gateway-token").read_text()
            assert token not in json.dumps(first)
            assert token not in (state / "runtime.log").read_text()
            assert first["credential_env"] == credential_env
            assert all(env[name] not in json.dumps(first) for name in local_env)
            assert "synthetic-saved-router-key" not in json.dumps(first)

            # A real child executable uses the Claude launch environment to send HTTP.
            harness_dir = root / "bin"
            harness_dir.mkdir()
            harness = harness_dir / "claude"
            harness.write_text("""#!/usr/bin/env python3
import json, os, urllib.request
assert 'RAYLINE_ROUTER_API_KEY' not in os.environ
assert 'RAYLINE_EXEC_AUTH_TOKEN' not in os.environ
assert 'RAYLINE_TEST_LOCAL_ALIAS' not in os.environ
assert not os.environ['ANTHROPIC_AUTH_TOKEN'].startswith('synthetic-runtime-only-')
assert os.environ['CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK'] == '1'
request = urllib.request.Request(os.environ['ANTHROPIC_BASE_URL'] + '/v1/messages',
    data=b'{"model":"rayline-auto","metadata":{"user_id":"session_fixture"}}',
    headers={'Authorization':'Bearer ' + os.environ['ANTHROPIC_AUTH_TOKEN'],
             'Content-Type':'application/json'})
with urllib.request.urlopen(request) as response:
    assert response.status == 200
print('harness request reached execution core')
""")
            harness.chmod(0o700)
            env["PATH"] = str(harness_dir) + os.pathsep + env["PATH"]
            assert "harness request reached execution core" in run("claude", "--", "-p", "test")
            # The fixture core writes its requests beside the config it was given.
            event = json.loads((state / "events.jsonl").read_text().splitlines()[-1])
            assert event["path"] == "/v1/messages"
            # The local process hands the core the saved router key (Decisions
            # and the cloud pins) and the endpoints' keys. The fixture lists a
            # name once per config entry that references it.
            assert sorted(set(event["credential_env_present"])) == credential_env
            assert json.loads(event["body"])["metadata"]["user_id"] == "session_fixture"
            # A run whose config file is not the running process's is refused,
            # even when the contents match.
            other = root / "other-settings.json"
            shutil.copy(settings, other)
            refused = raydock_cmd("--env", "fixture", "run", "claude", "--config", str(other), "--", "-p", "test",
                                  success=False)
            assert str(settings) in refused and str(other) in refused, refused
            pi = harness_dir / "pi"
            pi.write_text("""#!/usr/bin/env python3
import json, os, pathlib
assert 'RAYLINE_TEST_LOCAL_ALIAS' not in os.environ
provider = json.loads((pathlib.Path(os.environ['PI_CODING_AGENT_DIR']) / 'models.json').read_text())['providers']['rayline']
model = provider['models'][0]
assert model['maxTokens'] == 8192
assert model['contextWindow'] == 1000000  # the harness window; the core's is 16384
assert model['input'] == ['text']
assert provider['compat']['sendSessionAffinityHeaders'] is True
print('qualified Pi profile loaded')
""")
            pi.chmod(0o700)
            assert "qualified Pi profile loaded" in run("pi", "--", "--no-session")
            # Codex speaks openai_responses, which neither endpoint lists, so
            # run refuses it before launching anything.
            refused = run("codex", "--", "exec", "hello", success=False)
            assert "openai_responses" in refused and ".protocols" in refused, refused
            run("pi", "--", "--resume", success=False)
            # Capture the actual spawned args: an agent must use the config in
            # this process, never an already-running OpenClaw Gateway.
            openclaw = harness_dir / "openclaw"
            openclaw.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
assert pathlib.Path(os.environ['OPENCLAW_CONFIG_PATH']).is_file()
print(json.dumps(sys.argv[1:]))
""")
            openclaw.chmod(0o700)
            for args, expected in [
                (["agent", "-m", "hello"], ["agent", "--local", "-m", "hello"]),
                (["--profile=work", "agent", "-m", "hello"], ["--profile=work", "agent", "--local", "-m", "hello"]),
                (["agent", "--local", "-m", "hello"], ["agent", "--local", "-m", "hello"]),
                (["agent", "exec", "-m", "hello"], ["agent", "exec", "-m", "hello"]),
            ]:
                assert json.loads(run("openclaw", "--", *args)) == expected
            for args in (["--container", "box", "agent", "-m", "hello"],
                         ["--container=box", "agent", "-m", "hello"]):
                run("openclaw", "--", *args, success=False)
            env["OPENCLAW_CONTAINER"] = "box"
            run("openclaw", "--", "agent", "-m", "hello", success=False)
            env.pop("OPENCLAW_CONTAINER")
            # Local mode writes its harness files apart from cloud mode's.
            for name in ("pi", "openclaw"):
                assert (state / "harness" / name).is_dir(), name
            assert not (root / ".rayline" / "harness").exists()
            # A running process whose health check fails makes status exit 1.
            request = urllib.request.Request(first["listen_url"] + "/unready", data=b"{}",
                headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            with urllib.request.urlopen(request) as response:
                assert response.status == 200
            local("status", success=False)
            assert local("stop").splitlines() == ["Local process stopped."]
            wait_dead(first["core_pid"])
            assert not (state / "gateway-token").exists()
            assert local("status").splitlines() == ["Local process: not running"]
            assert local("stop").splitlines() == ["Local process: not running"]
            second = start()
            assert second["instance_id"] != first["instance_id"]
            assert (state / "gateway-token").read_text() != token
            # Abrupt loss cleans up the core; next explicit start removes stale metadata.
            os.kill(second["pid"], signal.SIGKILL)
            wait_dead(second["pid"])
            wait_dead(second["core_pid"])
            assert local("status").splitlines() == ["Local process: not running"]
            third = start()
            local("stop")
            wait_dead(third["core_pid"])
            # An incompatible core fails startup and leaves no owned process/state.
            core.write_text(INCOMPATIBLE_CORE)
            raydock_cmd("--env", "fixture", "local", "start", success=False)
            # The core receives Raydock's composed configuration.
            wait_dead(int((state / "core-config.pid").read_text()))
            assert not (state / "ready.json").exists()
            assert not (state / "gateway-token").exists()
        finally:
            local("stop")
    print("Local process lifecycle acceptance passed.")


if __name__ == "__main__":
    main()
