#!/usr/bin/python3
"""A `claude` for the dashboard flows in tests/dashboard_claude.rs. No model is contacted.

Every call appends its argv to `$HOME/claude-calls.jsonl`, so a test can assert which native
commands the dashboard ran. `$HOME/fake-launch` selects how `--bg` behaves (default `ok`):
  ok    hand the session to a stand-in daemon (`/bin/sleep`), list it in the registry with a
        transcript and an AI title, and print the background id the way Claude does
  fail  refuse to start, with a diagnostic on stderr
`attach` holds the pty as a viewer client until hung up; `rm` ends the session the way the
daemon does: registry entry and job record removed, stand-in killed.
"""
import json
import os
import pathlib
import signal
import subprocess
import sys
import time
import uuid

HOME = pathlib.Path(os.environ["HOME"])
CLAUDE = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
args = sys.argv[1:]

with open(HOME / "claude-calls.jsonl", "a") as log:
    log.write(json.dumps(args) + "\n")

if "--help" in args and args[:1] != ["stop"]:
    print("--bg  start a background session\nattach  join one\n--fork-session  fork one")
    sys.exit(0)

if args[:1] == ["stop"]:
    print("Usage: claude stop <id>")
    sys.exit(0)

if args[:1] == ["attach"]:
    sys.stdout.write(f"fixture attached to {args[1]}\r\n")
    sys.stdout.flush()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        time.sleep(0.05)

if args[:1] == ["rm"]:
    short = args[1]
    for entry in (CLAUDE / "sessions").glob("*.json"):
        value = json.loads(entry.read_text())
        if value.get("jobId") == short:
            entry.unlink()
            try:
                os.kill(value["pid"], signal.SIGKILL)
            except ProcessLookupError:
                pass
    job = CLAUDE / "jobs" / short
    if job.is_dir():
        for leftover in job.glob("*"):
            leftover.unlink()
        job.rmdir()
    sys.exit(0)

mode_file = HOME / "fake-launch"
mode = mode_file.read_text().strip() if mode_file.exists() else "ok"
if mode == "fail":
    print("fixture refused this launch", file=sys.stderr)
    sys.exit(3)

prompt = args[args.index("--") + 1] if "--" in args else ""
session = str(uuid.uuid4())
short = session[:8]
cwd = os.getcwd()
daemon = subprocess.Popen(["/bin/sleep", "600"], start_new_session=True,
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL)
key = "".join(c if c.isascii() and c.isalnum() else "-" for c in cwd)
transcript = CLAUDE / "projects" / key / f"{session}.jsonl"
transcript.parent.mkdir(parents=True, exist_ok=True)
transcript.write_text(
    json.dumps({"sessionId": session, "type": "user", "cwd": cwd,
                "timestamp": "2026-10-01T09:00:00.000Z",
                "message": {"role": "user", "content": prompt}}) + "\n"
    + json.dumps({"type": "ai-title", "sessionId": session,
                  "aiTitle": "Fixture title for the launch"}) + "\n"
)
registry = CLAUDE / "sessions"
registry.mkdir(parents=True, exist_ok=True)
(registry / f"{daemon.pid}.json").write_text(json.dumps({
    "pid": daemon.pid, "sessionId": session, "cwd": cwd, "kind": "bg",
    "jobId": short, "status": "busy", "startedAt": int(time.time() * 1000),
}))
print(f"backgrounded · {short} (busy)")
