#!/usr/bin/env python3
"""Run real Pi tasks through an externally configured ARC/local/cloud stack.

No policy or inference fixtures are created. The caller owns inference services
and supplies credential environment variables. Verification runs in this parent
process, outside the task directories; agent-written tests are never trusted.
"""
import argparse
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def write_json(path, value):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def events_since(path, offset):
    events = []
    if path.exists():
        with path.open() as stream:
            stream.seek(offset)
            for line in stream:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if event.get("msg") in ("decision selected", "decision refused", "execution completed", "execution aborted", "execution refused"):
                    events.append(event)
    return events


def require_admitted_decisions(events):
    for event in events:
        if event["msg"] == "decision refused":
            raise RuntimeError(f"ARC decision refused: {event.get('reason')} (route_id={event.get('route_id')})")


def require_routing_config(decisions):
    routing = decisions["routing"]
    assert decisions["result"] == "custom", "routing proof requires an explicit custom V result"
    assert "value_usd" in routing and decisions.get("price_sha256"), "routing proof requires pinned V and caller price identity"
    if "price_profile_id" in routing:
        assert routing["price_profile_id"] and routing.get("price_profile_version"), "routing proof requires a profile ID and explicit version"
        assert "prices" not in routing, "routing proof cannot combine a price profile and inline prices"
    else:
        assert "prices" in routing, "routing proof requires caller prices or a pinned price profile"


def require_applied_routing(decisions, selected):
    routing = decisions["routing"]
    assert selected and all(event["applied_value_usd"] == routing["value_usd"] and event["price_basis"] == "caller"
                            and event["price_sha256"] == decisions["price_sha256"] for event in selected), "V or discounted prices were not applied"
    if "price_profile_id" in routing:
        expected = {"id": routing["price_profile_id"], "version": routing["price_profile_version"],
                    "content_hash": decisions["price_sha256"]}
        assert all(event.get("price_profile") == expected for event in selected), "requested price profile was not applied"


def schedule_reference(tasks, capacity):
    if type(capacity) is not int or capacity <= 0:
        raise ValueError("invalid capacity")
    by_id = {}
    for task in tasks:
        if (not isinstance(task["id"], str) or not task["id"] or task["id"] in by_id
                or type(task["duration"]) is not int or task["duration"] <= 0
                or type(task["workers"]) is not int or not 1 <= task["workers"] <= capacity):
            raise ValueError("invalid task")
        by_id[task["id"]] = task
    for task in tasks:
        if len(set(task["depends"])) != len(task["depends"]) or any(dep not in by_id for dep in task["depends"]):
            raise ValueError("invalid dependencies")
    now, available, pending, running, done, result = 0, capacity, set(by_id), {}, set(), {}
    while pending or running:
        for name in sorted([name for name, end in running.items() if end == now]):
            running.pop(name)
            available += by_id[name]["workers"]
            done.add(name)
        for name in sorted(pending):
            task = by_id[name]
            if set(task["depends"]) <= done and task["workers"] <= available:
                pending.remove(name)
                available -= task["workers"]
                running[name] = now + task["duration"]
                result[name] = [now, running[name]]
        if running:
            now = min(running.values())
        elif pending:
            raise ValueError("cycle")
    return result


def scheduler_cases():
    rng = random.Random(731425)
    cases = [{"tasks": [], "capacity": 1}]
    for count in range(1, 14):
        capacity = rng.randrange(1, 5)
        tasks = [{"id": f"task-{index:02d}", "duration": rng.randrange(1, 9),
                  "workers": rng.randrange(1, capacity + 1),
                  "depends": [f"task-{prior:02d}" for prior in range(index) if rng.random() < .2]}
                 for index in range(count)]
        rng.shuffle(tasks)
        cases.append({"tasks": tasks, "capacity": capacity})
    cases += [
        {"tasks": [{"id": "a", "duration": 1, "workers": 1, "depends": ["b"]},
                   {"id": "b", "duration": 1, "workers": 1, "depends": ["a"]}], "capacity": 2},
        {"tasks": [{"id": "a", "duration": 1, "workers": 1, "depends": ["missing"]}], "capacity": 2},
        {"tasks": [{"id": "a", "duration": 0, "workers": 1, "depends": []}], "capacity": 1},
        {"tasks": [{"id": "a", "duration": 1, "workers": 3, "depends": []}], "capacity": 2},
        {"tasks": [], "capacity": 0},
    ]
    expected = []
    for case in cases:
        try:
            expected.append({"result": schedule_reference(**case)})
        except ValueError:
            expected.append({"error": "ValueError"})
    return cases, expected


def verify_scheduler(path, cases, expected):
    # The checker is supplied on argv by this running parent, never read from
    # agent-writable tests. Only generated solution code comes from the task cwd.
    checker = """import json,runpy,sys
function=runpy.run_path(sys.argv[1])['schedule']
out=[]
for case in json.load(sys.stdin):
    before=json.dumps(case,sort_keys=True)
    try: out.append({'result':function(case['tasks'],case['capacity'])})
    except ValueError: out.append({'error':'ValueError'})
    if json.dumps(case,sort_keys=True)!=before: raise RuntimeError('solution mutated its input')
print(json.dumps(out))
"""
    result = subprocess.run([sys.executable, "-I", "-S", "-c", checker, str(path)],
                            input=json.dumps(cases), capture_output=True, text=True,
                            env={"PATH": os.defpath}, cwd=path.parent, timeout=15)
    assert result.returncode == 0, "generated scheduler failed independent verification"
    assert json.loads(result.stdout) == expected, "generated scheduler disagrees with the parent oracle"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True, help="new private directory for evidence and synthetic tasks")
    parser.add_argument("--proof", choices=("routing", "transport"), required=True,
                        help="routing requires applied V/prices and local/cloud outcomes; transport explicitly makes no such claim")
    parser.add_argument("--rayline", type=Path, default=ROOT / "target/debug/rayline")
    parser.add_argument("--rld", type=Path, default=ROOT / "target/debug/rld")
    parser.add_argument("--task-timeout-secs", type=int, default=300)
    parser.add_argument("--max-generations-per-task", type=int, default=4)
    parser.add_argument("--max-run-cost-usd", type=Decimal, default=Decimal("5"))
    parser.add_argument("--generation-cost-bound-usd", type=Decimal, required=True,
                        help="operator-verified worst-case cost of one admitted inference at the configured token limits")
    args = parser.parse_args()
    assert args.task_timeout_secs > 0 and args.max_generations_per_task > 0
    config_bytes = args.config.read_bytes()
    config = json.loads(config_bytes)
    attempt_limit = config.get("max_generation_attempts")
    assert type(attempt_limit) is int and attempt_limit > 0, "live acceptance requires a positive core max_generation_attempts"
    assert args.generation_cost_bound_usd.is_finite() and args.generation_cost_bound_usd > 0
    assert args.max_run_cost_usd.is_finite() and args.max_run_cost_usd > 0
    cost_bound = attempt_limit * args.generation_cost_bound_usd
    assert cost_bound <= args.max_run_cost_usd, "configured attempt/token cost bound exceeds the run budget"
    routing = config["decisions"]["routing"]
    if args.proof == "routing":
        require_routing_config(config["decisions"])
    else:
        assert config["decisions"]["result"] == "top" and routing == {}, "transport proof must explicitly use top with empty routing"
    assert {deployment["kind"] for deployment in config["deployments"]} >= {"local", "cloud"}
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    output = args.output_dir.resolve()
    state, home = output / "state", output / "home"
    home.mkdir(mode=0o700)
    env = dict(os.environ, HOME=str(home), RLD_BIN=str(args.rld.resolve()), PI_TELEMETRY="0", PI_OFFLINE="1")
    cli = [str(args.rayline.resolve()), "hybrid", "--state-dir", str(state)]
    log = state / "runtime.log"
    report = {"proof": args.proof, "config_sha256": hashlib.sha256(config_bytes).hexdigest(),
              "package_sha256": config["package_sha256"], "decisions_url": config["decisions"]["url"],
              "result": config["decisions"]["result"], "routing": routing, "tasks": [], "passed": False,
              "generation_attempt_limit": attempt_limit, "inference_cost_bound_usd": str(cost_bound),
              "max_run_cost_usd": str(args.max_run_cost_usd),
              "limitations": (["Real top-only transport proof; no V or discounted-price acceptance."] if args.proof == "transport" else [])}
    cases, expected = scheduler_cases()  # Ground truth exists before the harness starts.
    easy = output / "easy"
    hard = output / "scheduler"
    easy.mkdir(); hard.mkdir()
    ledger = [{"item": "alpha", "quantity": 3, "unit_cents": 117},
              {"item": "beta", "quantity": 2, "unit_cents": 409},
              {"item": "gamma", "quantity": 5, "unit_cents": 23}]
    write_json(easy / "ledger.json", ledger)
    (hard / "scheduler.py").write_text("def schedule(tasks, capacity):\n    raise NotImplementedError\n")
    tasks = [
        ("easy", easy, "Read ledger.json. Write answer.json containing only the integer fields total_quantity and total_cents (sum quantity * unit_cents). Then report completion.",
         lambda: json.loads((easy / "answer.json").read_text()) == {"total_quantity": 10, "total_cents": 1284}),
        ("scheduler", hard, "Implement schedule(tasks, capacity) in scheduler.py using only Python's standard library. Tasks have unique nonempty string id, positive integer duration and workers, and depends (list of unique known task IDs). Capacity is a positive integer; each task workers must fit. Return dict id -> [start,end]. At time zero and each completion event, release ALL completed tasks, then scan ready unstarted tasks by lexicographic id and start every one that fits available capacity; skip non-fitting tasks and continue the scan. Dependencies must all have completed before starting. Tasks run nonpreemptively. Jump to the next completion time until done. Return {} for empty input. Raise ValueError for duplicate IDs, missing/duplicate dependencies, invalid capacity/duration/workers or dependency cycles (including self cycles). Do not mutate inputs. Run your own checks, then report completion.",
         lambda: verify_scheduler(hard / "scheduler.py", cases, expected) is None),
    ]
    started = False
    try:
        startup = subprocess.run(cli + ["start", "--core", str(args.core.resolve()), "--config", str(args.config.resolve()), "--port", "0"], env=env, capture_output=True, text=True, timeout=150)
        assert startup.returncode == 0, "hybrid startup failed; inspect the private runtime log"
        started = True
        report["ready"] = json.loads((state / "ready.json").read_text())
        for name, cwd, prompt, verify in tasks:
            offset = log.stat().st_size
            task_report = {"name": name, "objective_passed": False, "events": []}
            report["tasks"].append(task_report)
            stdout_path, stderr_path = output / f"{name}-pi.jsonl", output / f"{name}-pi.stderr"
            with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
                process = subprocess.Popen(cli + ["run", "pi", "--", "--provider", "rayline", "--model", "rayline-router",
                    "--no-session", "--offline", "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-context-files",
                    "--tools", "read,write,edit,bash", "--thinking", "off", "--mode", "json", "--print", prompt],
                    env=env, cwd=cwd, stdout=stdout, stderr=stderr, start_new_session=True)
                deadline = time.monotonic() + args.task_timeout_secs
                try:
                    while process.poll() is None:
                        task_events = events_since(log, offset)
                        require_admitted_decisions(task_events)
                        selected = [event for event in task_events if event["msg"] == "decision selected"]
                        assert len(selected) <= args.max_generations_per_task, "generation limit exceeded"
                        assert time.monotonic() < deadline, "task wall timeout exceeded"
                        time.sleep(.05)
                    assert process.returncode == 0, f"Pi exited unsuccessfully for {name}"
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGTERM)
                        try: process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL); process.wait()
                    task_report["events"] = events_since(log, offset)
            require_admitted_decisions(task_report["events"])
            assert verify(), f"{name} objective failed"
            task_report["objective_passed"] = True
            selected = [event for event in task_report["events"] if event["msg"] == "decision selected"]
            completed = [event for event in task_report["events"] if event["msg"] == "execution completed"]
            assert selected and len(selected) == len(completed) <= args.max_generations_per_task, "missing or duplicate generation evidence"
            assert [event["route_id"] for event in selected] == [event["route_id"] for event in completed], "selected/executed route IDs differ"
            assert not any(event["msg"] in ("execution aborted", "execution refused") for event in task_report["events"])
            assert all(event["package_sha256"] == config["package_sha256"] for event in selected)
            kinds = [event["deployment_kind"] for event in completed]
            task_report["executed_kinds"] = kinds
            task_report["requested_route_pattern_met"] = all(kind == "local" for kind in kinds) if name == "easy" else kinds[-1] == "cloud"
            if args.proof == "routing":
                assert task_report["requested_route_pattern_met"], f"ARC did not choose the required {name} route"
                require_applied_routing(config["decisions"], selected)
        assert args.config.read_bytes() == config_bytes, "configuration changed between tasks"
        report["passed"] = True
    except BaseException as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        if started:
            stopped = subprocess.run(cli + ["stop"], env=env, capture_output=True, text=True, timeout=20)
            report["cleanup_succeeded"] = stopped.returncode == 0
            if stopped.returncode:
                report["passed"] = False
                write_json(output / "report.json", report)
                raise RuntimeError("hybrid cleanup failed; inspect the private runtime log")
        write_json(output / "report.json", report)
    print(f"Real Pi {args.proof} proof passed. Evidence: {output / 'report.json'}")
    if args.proof == "transport":
        print("This does not satisfy low-V/discounted-price routing acceptance; actual route choices are recorded.")


if __name__ == "__main__":
    main()
