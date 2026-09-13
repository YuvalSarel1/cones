#!/usr/bin/env python3
"""Write the orchestrator status file cones reads: ~/.claude/orchestrator/<sha1(cwd)>.json.
usage: status.py D WB SELF [JOBID]   (D holds roster.now, started, event.txt, held.json)
Called by sweep.sh every tick; the orchestrator rewrites event.txt/held.json when they change."""
import hashlib, json, os, sys, time
D, WB, SELF = sys.argv[1], sys.argv[2], sys.argv[3]
JOB = sys.argv[4] if len(sys.argv) > 4 else ""
out_dir = os.path.expanduser("~/.claude/orchestrator"); os.makedirs(out_dir, exist_ok=True)
path = os.path.join(out_dir, hashlib.sha1(WB.encode()).hexdigest() + ".json")
def read(name, default):
    p = os.path.join(D, name)
    try: return open(p).read()
    except FileNotFoundError: return default
peers = []
for line in read("roster.now", "").splitlines():
    if not line.strip(): continue
    pid, _, name = line.partition("\t")
    status = "codex" if name.startswith("CODEX:") else "live"
    peers.append({"pid": int(pid), "name": name, "status": status})
try: held = json.loads(read("held.json", "[]"))
except json.JSONDecodeError: held = []
started_p = os.path.join(D, "started")
if not os.path.exists(started_p): open(started_p, "w").write(time.strftime("%Y-%m-%dT%H:%M:%S"))
doc = {"cwd": WB, "pid": int(SELF), "jobId": JOB, "started": open(started_p).read().strip(),
       "updated": time.strftime("%Y-%m-%dT%H:%M:%S"), "peers": peers, "held": held,
       "last_event": (read("event.txt", "").strip().splitlines() or [""])[-1]}
tmp = path + ".tmp"; json.dump(doc, open(tmp, "w"), indent=1); os.replace(tmp, path)
print(path)
