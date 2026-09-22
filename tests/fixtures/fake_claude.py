#!/usr/bin/python3
"""A deterministic background-session fixture. It never contacts a model.

`--bg` returns the way Claude's does, after handing the session to a stand-in process that
plays the daemon: it writes the registry entry and the job record cones reads, moves the
session through the states a mode asks for, and stays listed afterwards the way a finished
background session does. `rm` ends it.
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
SESSIONS = HOME / ".claude/sessions"
JOBS = HOME / ".claude/jobs"
STATUSLINE = HOME / ".claude/statusline"
LINGER = 30


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value))
    temporary.replace(path)


def job(short, state, tempo):
    write(JOBS / short / "state.json", {"state": state, "tempo": tempo})


def transcript(session):
    key = "".join(c if c.isascii() and c.isalnum() else "-" for c in os.getcwd())
    path = HOME / ".claude/projects" / key / (session + ".jsonl")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"sessionId": session, "type": "user", "message": {"role": "user", "content": "test"}})
        + "\n"
        + json.dumps({
            "sessionId": session, "type": "assistant",
            "message": {
                "id": "msg_fake", "role": "assistant", "model": "claude-opus-5",
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 30, "output_tokens": 10},
            },
        })
        + "\n"
    )
    return path


def stand_in(session, mode, short):
    """The daemon's side of a background session, in its own process."""
    registry = SESSIONS / f"{os.getpid()}.json"
    write(registry, {
        "pid": os.getpid(), "sessionId": session, "cwd": os.getcwd(), "kind": "bg",
        "jobId": short, "status": "idle", "startedAt": int(time.time() * 1000),
    })
    job(short, "active", "active")
    transcript(session)
    if mode == "barrier":
        while not (pathlib.Path(os.environ["FAKE_LEDGER"]).parent / "release").exists():
            time.sleep(0.01)
    if mode in ("hang", "descendant"):
        child = subprocess.Popen(
            ["/bin/sh", "-c", "trap '' TERM; while :; do sleep 1; done"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        pathlib.Path(os.environ["FAKE_CHILD_PID"]).write_text(str(child.pid))
        if mode == "hang":
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if mode == "failed":
        job(short, "failed", "idle")
    elif mode == "blocked":
        job(short, "blocked", "idle")
    elif mode != "hang":
        write(STATUSLINE / f"{session}.json", {"cost": {"total_cost_usd": 0.01}})
        job(short, "done", "idle")
    # A finished background session stays listed, and attachable, until it is removed.
    for _ in range(LINGER * 100):
        if not registry.exists():
            return
        time.sleep(0.01)


args = sys.argv[1:]

if args[:1] == ["__fake-session"]:
    stand_in(args[1], args[2], args[3])
    sys.exit(0)

if args[:1] == ["stop"]:
    print("Usage: claude stop <id>")
    sys.exit(0)

if "--help" in args:
    print("--bg  start a background session\nattach  join one\n--fork-session  fork one")
    sys.exit(0)

if args[:1] == ["rm"]:
    short = args[1]
    for entry in SESSIONS.glob("*.json"):
        value = json.loads(entry.read_text())
        if value.get("jobId") == short:
            entry.unlink()
            try:
                # The daemon ends the session's whole process tree, descendants included.
                os.killpg(os.getpgid(value["pid"]), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
    if (JOBS / short).is_dir():
        for leftover in (JOBS / short).glob("*"):
            leftover.unlink()
        (JOBS / short).rmdir()
    sys.exit(0)

# `claude --bg` names the conversation itself; the launcher learns the id from this line.
session = str(uuid.uuid4())
mode = args[args.index("--model") + 1]

if mode == "missing":
    # Backgrounded, and never listed: the run has no session to watch.
    print(f"backgrounded · {session[:8]} (idle)")
    sys.exit(0)
if mode == "mismatch":
    print("backgrounded · deadbeef (idle)")
    sys.exit(0)
if mode == "unnamed":
    print("started")
    sys.exit(0)
if mode == "refused":
    print("no session started", file=sys.stderr)
    sys.exit(1)

subprocess.Popen(
    [sys.executable, __file__, "__fake-session", session, mode, session[:8]],
    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    start_new_session=True,
)
print(f"backgrounded · {session[:8]} (idle)")
