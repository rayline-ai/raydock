#!/usr/bin/env python3
"""Hybrid v2 live acceptance (pathfinder #5899 scenarios a-c) with stock Pi.

Runs the real Rayline CLI, rld, Go core (execution_context_v2), stock installed
Pi, the live dev Decisions cell and real local/cloud inference. Nothing is
aliased or forced. Rayline Local composes the profile (hybrid default preset,
optional settings schedule, `local_model`). Oracles run in this parent process
on files the agent cannot edit; receipts come from the core's structured log.

  a        easy ledger task; expects local Qwen to do the work
  b        an easy prompt then the scheduler prompt in one session; a model
           switch must only happen at an eligible point (never a plain tool
           result) unless it is a capacity handoff
  c        capacity spill: Pi compaction disabled (stock setting), files that
           grow the conversation past the local window during a tool loop

The caller supplies RAYLINE_DEV_KEY (Decisions + cloud) and LOCAL_QWEN_KEY in
the environment and owns the llama.cpp runtime. Evidence contains no prompts,
file contents or credentials.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("hybrid_live", ROOT / "scripts" / "test-hybrid-live.py")
hybrid_live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hybrid_live)

LEDGER_PROMPT = ("Read ledger.json. Write answer.json containing only the integer fields total_quantity and total_cents "
                 "(sum quantity * unit_cents). Then report completion.")
WARMUP_PROMPT = "Read ledger.json and tell me how many line items it contains."
KEYWORDS = ["amber", "basalt", "cobalt", "dune"]
LOG_MESSAGES = ("execution context applied", "decision selected", "decision refused", "execution completed",
                "execution aborted", "execution refused", "execution reconsult")


def read_events(path):
    events = []
    for line in path.read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("msg") in LOG_MESSAGES:
            events.append({key: value for key, value in event.items() if key != "cache"})
    return events


def generations(events):
    """One row per completed generation, joined with its decision receipt."""
    applied = {e["route_id"]: e for e in events if e["msg"] == "execution context applied"}
    rows = []
    for completed in (e for e in events if e["msg"] == "execution completed"):
        receipt = applied.get(completed["route_id"], {})
        rows.append({key: receipt.get(key) for key in ("event_kind", "event_basis", "attempt", "reconsult", "decided_by",
                                                       "eligible", "held_from", "excluded", "capacity", "capacity_unknown", "compaction")}
                    | {"route_id": completed["route_id"], "executed_model": completed.get("executed_model"),
                       "deployment": completed.get("deployment"), "deployment_kind": completed.get("deployment_kind"),
                       "input_tokens": completed.get("input_tokens"), "output_tokens": completed.get("output_tokens")})
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--scenario", choices=("a", "b", "c"), required=True)
    parser.add_argument("--schedule-model", nargs="+", help="settings hybrid.routing.schedule.model (e.g. cache_boundaries user_messages)")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--rayline", type=Path, default=ROOT / "target/debug/rayline")
    parser.add_argument("--rld", type=Path, default=ROOT / "target/debug/rld")
    parser.add_argument("--max-generations", type=int, default=12)
    parser.add_argument("--task-timeout-secs", type=int, default=600)
    args = parser.parse_args()
    for name in ("RAYLINE_DEV_KEY", "LOCAL_QWEN_KEY"):
        assert os.environ.get(name), f"{name} must be supplied in the environment"
    args.output_dir.mkdir(mode=0o700, parents=True, exist_ok=False)
    output = args.output_dir.resolve()
    state, home, work = output / "state", output / "home", output / "work"
    for path in (home, work):
        path.mkdir(mode=0o700)
    profile = json.loads(args.profile.read_text())
    local = next(d for d in profile["deployments"] if d["kind"] == "local")
    settings = {"local_model": {"mode": "custom", "base_url": local["url"].rsplit("/v1/", 1)[0], "model": local["wire_model"]}}
    if args.schedule_model:
        settings["hybrid"] = {"routing": {"schedule": {"model": args.schedule_model}}}
    (home / ".config" / "rayline").mkdir(parents=True, mode=0o700)
    (home / ".config" / "rayline" / "settings.json").write_text(json.dumps(settings))
    pi_settings = {"defaultProvider": "rayline", "defaultModel": "rayline-router"}
    if args.scenario == "c":
        pi_settings["compaction"] = {"enabled": False}  # no compaction point in the harness
    (state / "harness" / "pi").mkdir(parents=True, mode=0o700)
    (state / "harness" / "pi" / "settings.json").write_text(json.dumps(pi_settings))

    hybrid_live.write_json(work / "ledger.json", [{"item": "alpha", "quantity": 3, "unit_cents": 117},
                                                  {"item": "beta", "quantity": 2, "unit_cents": 409},
                                                  {"item": "gamma", "quantity": 5, "unit_cents": 23}])
    tools = "read,write,edit,bash"
    if args.scenario == "a":
        prompts = [LEDGER_PROMPT]
        oracle = lambda: json.loads((work / "answer.json").read_text()) == {"total_quantity": 10, "total_cents": 1284}
    elif args.scenario == "b":
        cases, expected = hybrid_live.scheduler_cases()
        (work / "scheduler.py").write_text("def schedule(tasks, capacity):\n    raise NotImplementedError\n")
        scheduler_prompt = next(_scheduler_prompts())
        prompts = [WARMUP_PROMPT, scheduler_prompt]
        oracle = lambda: hybrid_live.verify_scheduler(work / "scheduler.py", cases, expected) is None
    else:
        rng = random.Random(20261024)
        words = ["river", "stone", "lantern", "meadow", "harbor", "copper", "willow", "signal", "orchard", "glacier"]
        for index, keyword in enumerate(KEYWORDS, 1):
            lines = [" ".join(rng.choice(words) for _ in range(12)) for _ in range(330)]
            lines.insert(rng.randrange(len(lines)), f"KEY: {keyword}")
            (work / f"part{index}.txt").write_text("\n".join(lines) + "\n")
        tools = "read,write"
        prompts = ["Read part1.txt, part2.txt, part3.txt and part4.txt in that order, one whole file per read call "
                   "(no offset or limit). Each file contains exactly one line of the form 'KEY: <word>'. Write answer.json "
                   "containing only {\"keys\": [the four words in file order]}. Then report completion."]
        oracle = lambda: json.loads((work / "answer.json").read_text()) == {"keys": KEYWORDS}

    env = dict(os.environ, HOME=str(home), RLD_BIN=str(args.rld.resolve()), PI_TELEMETRY="0", PI_OFFLINE="1")
    cli = [str(args.rayline.resolve()), "hybrid", "--state-dir", str(state)]
    report = {"scenario": args.scenario, "profile": args.profile.name, "schedule_model": args.schedule_model,
              "decisions": {k: profile["decisions"].get(k) for k in ("url", "model", "cell_tag", "result", "routing")},
              "targets": sorted({d["id"] for d in profile["deployments"]}), "passed": False}
    started = False
    try:
        startup = subprocess.run(cli + ["start", "--core", str(args.core.resolve()), "--config", str(args.profile.resolve()), "--port", "0"],
                                 env=env, capture_output=True, text=True, timeout=180)
        report["start_output"] = [line for line in startup.stdout.splitlines() if not line.startswith("Gateway credential")]
        assert startup.returncode == 0, "hybrid startup failed: " + startup.stderr[-2000:]
        started = True
        log = state / "runtime.log"
        with (output / "pi.jsonl").open("w") as stdout, (output / "pi.stderr").open("w") as stderr:
            process = subprocess.Popen(cli + ["run", "pi", "--", "--provider", "rayline", "--model", "rayline-router",
                                              "--no-session", "--offline", "--no-extensions", "--no-skills", "--no-prompt-templates",
                                              "--no-context-files", "--tools", tools, "--thinking", "off", "--mode", "json", "--print", *prompts],
                                       env=env, cwd=work, stdout=stdout, stderr=stderr, start_new_session=True)
            deadline = time.monotonic() + args.task_timeout_secs
            try:
                while process.poll() is None:
                    events = read_events(log)
                    assert not any(e["msg"] == "decision refused" for e in events), "a decision was refused"
                    assert sum(e["msg"] == "execution context applied" for e in events) <= args.max_generations, "generation bound exceeded"
                    assert time.monotonic() < deadline, "task wall timeout"
                    time.sleep(0.2)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
        events = read_events(log)
        rows = generations(events)
        report["generations"] = rows
        report["reconsults"] = [e for e in events if e["msg"] == "execution reconsult"]
        report["failures"] = [e for e in events if e["msg"] in ("decision refused", "execution aborted", "execution refused")]
        report["pi_exit"] = process.returncode
        try:
            report["oracle_passed"] = bool(oracle())
        except Exception as error:  # noqa: BLE001 - the oracle's failure is evidence
            report["oracle_passed"], report["oracle_error"] = False, f"{type(error).__name__}: {error}"
        kinds = [row["deployment_kind"] for row in rows]
        models = [row["executed_model"] for row in rows]
        # A model switch is admissible only at an eligible point or as a capacity handoff.
        switches = [{"index": i, "from": models[i - 1], "to": models[i], "event": rows[i]["event_kind"], "decided_by": rows[i]["decided_by"]}
                    for i in range(1, len(rows)) if models[i] != models[i - 1]]
        report["switches"] = switches
        report["switch_at_plain_tool_result"] = [s for s in switches if s["event"] == "tool_result"
                                                 and not str(s["decided_by"]).startswith("capacity_handoff")]
        if args.scenario == "a":
            report["route_pattern_met"] = bool(kinds) and all(k == "local" for k in kinds)
        elif args.scenario == "b":
            report["route_pattern_met"] = not report["switch_at_plain_tool_result"]
        else:
            handoffs = [r for r in rows if str(r["decided_by"]).startswith("capacity_handoff")]
            report["route_pattern_met"] = (bool(kinds) and kinds[0] == "local" and kinds[-1] == "cloud" and bool(handoffs)
                                           and not report["switch_at_plain_tool_result"])
        report["passed"] = bool(report["oracle_passed"] and report["route_pattern_met"] and report["pi_exit"] == 0 and not report["failures"])
    except BaseException as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        if started:
            stopped = subprocess.run(cli + ["stop"], env=env, capture_output=True, text=True, timeout=30)
            report["cleanup_succeeded"] = stopped.returncode == 0
        hybrid_live.write_json(output / "report.json", report)
    print(json.dumps({k: report.get(k) for k in ("scenario", "passed", "oracle_passed", "route_pattern_met", "switches")}, indent=1))


def _scheduler_prompts():
    source = (ROOT / "scripts" / "test-hybrid-live.py").read_text()
    start = source.index('("scheduler", hard, "') + len('("scheduler", hard, "')
    yield source[start:source.index('",', start)]


if __name__ == "__main__":
    main()
