"""Windows checks for rayline-arc-encoder: --parent-pipe shutdown, --state-dir save/restore, replace of existing
files (also while another handle holds one open), long and non-ASCII state paths. Stdlib only.
    python wintest.py BINARY MODEL WORKDIR"""
import json, os, subprocess, sys, time, urllib.request, urllib.error
binary, model, work = sys.argv[1], sys.argv[2], sys.argv[3]
def files_in(sd):
    return sorted(os.path.relpath(os.path.join(r, f), sd) for r, _, fs in os.walk(sd) for f in fs)
def start(sd, extra=(), err="wintest.err"):
    p = subprocess.Popen([binary, "--model", model, "--port", "0", "--ctx", "512", "--budget", "64", "--ubatch", "64",
                          "--sessions", "1", "--state-dir", sd, "--persist-min", "1", "--parent-pipe", *extra],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(os.path.join(work, err), "a"), text=True)
    line = p.stdout.readline()
    try: return p, json.loads(line)["listening"]
    except Exception:
        p.kill(); p.wait()
        tail = open(os.path.join(work, err), errors="replace").read().splitlines()[-2:]
        raise SystemExit(f"FAIL: no listening line ({line!r}); stderr: {tail}")
def post(port, body, path="/encode"):
    r = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=120) as x: return x.status, dict(x.headers)
    except urllib.error.HTTPError as e: return e.code, dict(e.headers)
def stop(p):
    t = time.time(); p.stdin.close(); rc = p.wait(timeout=60); return rc, round(time.time() - t, 2)
def report(name, ok, detail=""):
    print(("ok  " if ok else "FAIL") + f" {name} {detail}", flush=True)
def scenario(label, sd):
    os.makedirs(sd, exist_ok=True)
    import shutil; shutil.rmtree(sd, ignore_errors=True); os.makedirs(sd, exist_ok=True)
    p, port = start(sd)
    s, _ = post(port, {"base": 0, "tokens": list(range(10, 40)), "offsets": [29], "session": "A", "owner": "o"})
    report(f"[{label}] encode", s == 200, s)
    rc, secs = stop(p)
    files = files_in(sd)
    report(f"[{label}] --parent-pipe stdin close exits 0 and saves", rc == 0 and len([f for f in files if f.endswith('.state')]) >= 1, f"rc={rc} {secs}s files={files}")
    p, port = start(sd)
    s, h = post(port, {"base": 30, "tokens": [50, 51], "offsets": [31], "session": "A"})
    report(f"[{label}] append after restart restores saved state", s == 200 and h.get("x-restored-tokens") == "30", f"{s} restored={h.get('x-restored-tokens')}")
    # same session, same length again: the save renames over existing files
    s, _ = post(port, {"base": 0, "tokens": list(range(60, 92)), "offsets": [31], "session": "A", "owner": "o"})
    s2, _ = post(port, {"owner": "o"}, "/close")
    s3, _ = post(port, {"base": 0, "tokens": list(range(70, 102)), "offsets": [31], "session": "A", "owner": "o"})
    s4, _ = post(port, {"owner": "o"}, "/close")
    files = files_in(sd)
    report(f"[{label}] re-save same stem replaces files", s == s2 == s3 == s4 == 200 and not any(f.endswith('.tmp') for f in files), f"files={files}")
    return p, port
work = os.path.abspath(work); os.makedirs(work, exist_ok=True)
p, port = scenario("plain", os.path.join(work, "state"))
# A reader holding a saved file open (antivirus, indexer, backup): does the rename-over still work?
sd = os.path.join(work, "state"); state = [f for f in files_in(sd) if f.endswith(".json")][0]
h = open(os.path.join(sd, state), "rb")
s, _ = post(port, {"base": 0, "tokens": list(range(80, 112)), "offsets": [31], "session": "A", "owner": "o"})
s2, _ = post(port, {"owner": "o"}, "/close")
files = files_in(sd); h.close()
errtail = open(os.path.join(work, "wintest.err"), errors="replace").read().splitlines()[-3:]
report("[open-handle] save while another handle reads the .state", not any(f.endswith('.tmp') for f in files) and not any("failed" in l for l in errtail), f"files={files} log={errtail}")
stop(p)
long_dir = os.path.join(work, *(["d" * 60] * 5), "state")
print("long path length", len(long_dir))
try:
    p, port = scenario("long>260", long_dir); stop(p)
except (Exception, SystemExit) as e: report("[long>260]", False, repr(e))
uni = os.path.join(work, "Nguy\u1ec5n Th\u1ecb", "state")
try:
    p, port = scenario("non-ascii", uni); stop(p)
except (Exception, SystemExit) as e: report("[non-ascii]", False, repr(e))
# A hard kill (what TerminateProcess / kill_on_drop does) saves nothing
sd = os.path.join(work, "state-kill"); os.makedirs(sd, exist_ok=True)
p, port = start(sd); post(port, {"base": 0, "tokens": list(range(10, 40)), "offsets": [29], "session": "A"})
p.kill(); p.wait()
report("[kill] TerminateProcess saves nothing (expected)", not any(f.endswith(".state") for f in files_in(sd)), f"files={files_in(sd)}")

# Two encoders on one --state-dir: the second warns and runs without saved sessions (LockFileEx)
import shutil
sd = os.path.join(work, "state-lock"); shutil.rmtree(sd, ignore_errors=True); os.makedirs(sd)
for e in ("lockA.err", "lockB.err"):
    if os.path.exists(os.path.join(work, e)): os.remove(os.path.join(work, e))
pa, porta = start(sd, err="lockA.err")
pb, portb = start(sd, err="lockB.err")
sb, _ = post(portb, {"base": 0, "tokens": list(range(10, 40)), "offsets": [29], "session": "B"})
rcb, _ = stop(pb)
after_b = files_in(sd)
warn = [l for l in open(os.path.join(work, "lockB.err"), errors="replace").read().splitlines() if "another encoder" in l]
report("[lock] second encoder on same state dir warns", bool(warn), warn[:1])
report("[lock] second encoder still serves and exits 0", sb == 200 and rcb == 0, f"encode={sb} rc={rcb}")
report("[lock] second encoder saved nothing", not any(f.endswith(".state") for f in after_b), f"files={after_b}")
sa, _ = post(porta, {"base": 0, "tokens": list(range(10, 40)), "offsets": [29], "session": "A"})
rca, _ = stop(pa)
report("[lock] first encoder still persists", sa == 200 and rca == 0 and any(f.endswith(".state") for f in files_in(sd)), f"files={files_in(sd)}")
