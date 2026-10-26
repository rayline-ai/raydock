#!/usr/bin/env python3
"""Exercise built CLI/rld processes and a local execution-contract fixture.

Run after `cargo build --locked -p rayline-cli -p rayline-daemon`.
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
CLI = ROOT / "target/debug/rayline"
RLD = ROOT / "target/debug/rld"


def main():
    with tempfile.TemporaryDirectory(prefix="rayline-hybrid-cli-") as tmp:
        root = Path(tmp)
        state = root / "state"
        env = dict(os.environ, HOME=str(root))
        env.pop("RLD_BIN", None)
        env.pop("OPENCLAW_CONTAINER", None)
        installed = root / "installed"
        installed.mkdir()
        shutil.copy(CLI, installed / "rayline")
        shutil.copy(RLD, installed / "rld")
        credential_env = ["RAYLINE_TEST_HOSTED_ALIAS", "RAYLINE_TEST_LOCAL_ALIAS", "ANTHROPIC_AUTH_TOKEN"]
        for name in credential_env:
            env[name] = "synthetic-runtime-only-" + name
        for name in ("RAYLINE_ROUTER_API_KEY", "RAYLINE_EXEC_AUTH_TOKEN", "RAYLINE_API_KEY"):
            env.pop(name, None)
        core = installed / "rayline-core"
        shutil.copy(ROOT / "crates/rayline-daemon/tests/fixtures/execution-contract.py", core)
        core.chmod(0o700)
        config = root / "core.json"
        config.write_text(json.dumps({"events": str(root / "events.jsonl"),
            "cancelled": str(root / "cancelled"),
            "decisions": {"url": "http://127.0.0.1:9/decisions", "auth_env": credential_env[0]},
            "deployments": [{"auth_env": name} for name in credential_env[1:]]}))

        def cli(*args, success=True):
            result = subprocess.run([str(installed / "rayline"), "hybrid", "--state-dir", str(state), *args],
                                    env=env, capture_output=True, text=True, timeout=75)
            assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
            return result.stdout

        def start():
            cli("start", "--config", str(config), "--port", "0")
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
            status = json.loads(cli("status"))
            assert status["health"]["instance_id"] == first["instance_id"]
            cli("start", "--core", str(core), "--config", str(config), "--port", "0", success=False)
            token = (state / "gateway-token").read_text()
            assert token not in json.dumps(first)
            assert token not in (state / "runtime.log").read_text()
            assert first["credential_env"] == sorted(credential_env)
            assert all(env[name] not in json.dumps(first) for name in credential_env)

            # A real child executable uses the Claude launch environment to send HTTP.
            harness_dir = root / "bin"
            harness_dir.mkdir()
            harness = harness_dir / "claude"
            harness.write_text("""#!/usr/bin/env python3
import json, os, urllib.request
assert 'RAYLINE_ROUTER_API_KEY' not in os.environ
assert 'RAYLINE_EXEC_AUTH_TOKEN' not in os.environ
assert 'RAYLINE_TEST_HOSTED_ALIAS' not in os.environ
assert 'RAYLINE_TEST_LOCAL_ALIAS' not in os.environ
assert not os.environ['ANTHROPIC_AUTH_TOKEN'].startswith('synthetic-runtime-only-')
assert os.environ['CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK'] == '1'
request = urllib.request.Request(os.environ['ANTHROPIC_BASE_URL'] + '/v1/messages',
    data=b'{"model":"rayline-router","metadata":{"user_id":"session_fixture"}}',
    headers={'Authorization':'Bearer ' + os.environ['ANTHROPIC_AUTH_TOKEN'],
             'Content-Type':'application/json'})
with urllib.request.urlopen(request) as response:
    assert response.status == 200
print('harness request reached execution core')
""")
            harness.chmod(0o700)
            env["PATH"] = str(harness_dir) + os.pathsep + env["PATH"]
            assert "harness request reached execution core" in cli("run", "claude", "--", "-p", "test")
            event = json.loads((root / "events.jsonl").read_text().splitlines()[-1])
            assert event["path"] == "/v1/messages"
            assert sorted(event["credential_env_present"]) == sorted(credential_env)
            assert json.loads(event["body"])["metadata"]["user_id"] == "session_fixture"
            pi = harness_dir / "pi"
            pi.write_text("""#!/usr/bin/env python3
import json, os, pathlib
assert 'RAYLINE_TEST_HOSTED_ALIAS' not in os.environ
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
            assert "qualified Pi profile loaded" in cli("run", "pi", "--", "--no-session")
            cli("run", "pi", "--", "--resume", success=False)
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
                assert json.loads(cli("run", "openclaw", "--", *args)) == expected
            for args in (["--container", "box", "agent", "-m", "hello"],
                         ["--container=box", "agent", "-m", "hello"]):
                cli("run", "openclaw", "--", *args, success=False)
            env["OPENCLAW_CONTAINER"] = "box"
            cli("run", "openclaw", "--", "agent", "-m", "hello", success=False)
            env.pop("OPENCLAW_CONTAINER")
            request = urllib.request.Request(first["listen_url"] + "/unready", data=b"{}",
                headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            with urllib.request.urlopen(request) as response:
                assert response.status == 200
            cli("status", success=False)
            cli("stop")
            wait_dead(first["core_pid"])
            assert not (state / "gateway-token").exists()
            second = start()
            assert second["instance_id"] != first["instance_id"]
            assert (state / "gateway-token").read_text() != token
            # Abrupt loss cleans up the core; next explicit start removes stale metadata.
            os.kill(second["pid"], signal.SIGKILL)
            wait_dead(second["pid"])
            wait_dead(second["core_pid"])
            cli("status", success=False)
            third = start()
            cli("stop")
            wait_dead(third["core_pid"])
            # An incompatible executable fails startup and leaves no owned process/state.
            bad = json.loads(config.read_text())
            bad["contract_version"] = 99
            config.write_text(json.dumps(bad))
            cli("start", "--core", str(core), "--config", str(config), "--port", "0", success=False)
            # The core receives Rayline Local's composed configuration.
            wait_dead(int((state / "core-config.pid").read_text()))
            assert not (state / "ready.json").exists()
            assert not (state / "gateway-token").exists()
        finally:
            cli("stop")
    print("Hybrid CLI lifecycle acceptance passed.")


if __name__ == "__main__":
    main()
