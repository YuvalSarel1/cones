#!/usr/bin/python3
"""A stand-in for Grok Build 1.0.77. No model is contacted.

Installed under a fixture HOME as `.local/bin/grok`. Like the CLI it draws its whole screen on
stderr and nothing on stdout, so a pane that pipes stderr stays blank. It keeps its session the
way 1.0.77 does, in record shapes copied from files the CLI wrote:
`$GROK_HOME/sessions/<url-encoded cwd>/<id>/` (GROK_HOME defaults to `~/.grok`) with
`summary.json`, `updates.jsonl`, `usage.json`, `signals.json` and `events.jsonl`, which it holds
open for writing while it runs. `--resume ID` reopens a session, `--fork-session --session-id
NEW` copies one, and `--session-id` names a new one.

The instruction after `--` and every line typed afterwards is one turn, answered with
`reply <pid>: <text>`. A line containing `approve?` stops its turn at a permission prompt until
the next line. Each turn costs 0.12345 USD and adds 1000 context tokens. FAKE_GROK_BROKEN=1
writes an unreadable usage.json and no signals.json, as a client killed mid-write would.
"""
import datetime
import json
import os
import shutil
import signal
import sys
import time
import urllib.parse
import uuid

args = sys.argv[1:]
if "--help" in args:
    print("--always-approve  --session-id <ID>  --fork-session  --trust  --resume  --model")
    sys.exit(0)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))


def option(name):
    return args[args.index(name) + 1] if name in args[:-1] else None


def say(text):
    sys.stderr.write(text + "\r\n")
    sys.stderr.flush()


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


home = os.environ.get("GROK_HOME") or os.path.join(os.environ["HOME"], ".grok")
cwd = os.getcwd()
folder = os.path.join(home, "sessions", urllib.parse.quote(cwd, safe=""))
resume, named = option("--resume"), option("--session-id")
fork = "--fork-session" in args
sid = resume if resume and not fork else (named or str(uuid.uuid4()))
session = os.path.join(folder, sid)
if fork:
    shutil.copytree(os.path.join(folder, resume), session)
os.makedirs(session, exist_ok=True)
path = lambda name: os.path.join(session, name)
summary = {
    "info": {"id": sid, "cwd": cwd},
    "created_at": now(),
    "current_model_id": option("--model") or "grok-4.7",
    "reasoning_effort": "high",
    "session_kind": "interactive",
}
if os.path.exists(path("summary.json")):
    summary = {**json.load(open(path("summary.json"))), "info": {"id": sid, "cwd": cwd}}
if fork:
    summary.update(parent_session_id=resume, session_kind="fork", created_at=now())
broken = os.environ.get("FAKE_GROK_BROKEN") == "1"
events = open(path("events.jsonl"), "a")
counter = [0]


def event(**fields):
    events.write(json.dumps({"ts": now(), **fields}) + "\n")
    events.flush()


def update(kind, **fields):
    counter[0] += 1
    record = {
        "timestamp": int(time.time()),
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": kind, **fields},
            "_meta": {"eventId": f"{sid}-{os.getpid()}-{counter[0]}"},
        },
    }
    with open(path("updates.jsonl"), "a") as f:
        f.write(json.dumps(record) + "\n")


def save():
    summary["last_active_at"] = now()
    with open(path("summary.json"), "w") as f:
        json.dump(summary, f)


def finish(text):
    update("agent_message_chunk", content={"type": "text", "text": f"reply {os.getpid()}: {text}"})
    update("turn_completed", prompt_id=str(uuid.uuid4()), stop_reason="end_turn",
           usage={"inputTokens": 1000, "outputTokens": 10, "costUsdTicks": 1234500000})
    event(type="turn_ended", outcome="completed")
    usage = {"session": {"inputTokens": 0, "outputTokens": 0, "costUsdTicks": 0}}
    if os.path.exists(path("usage.json")):
        try:
            usage = json.load(open(path("usage.json")))
        except ValueError:
            pass
    for key, add in [("inputTokens", 1000), ("outputTokens", 10), ("costUsdTicks", 1234500000)]:
        usage["session"][key] += add
    with open(path("usage.json"), "w") as f:
        f.write("{\"session\": {" if broken else json.dumps(usage))
    if not broken:
        with open(path("signals.json"), "w") as f:
            json.dump({"contextTokensUsed": usage["session"]["inputTokens"],
                       "contextWindowTokens": 256000}, f)
    summary.setdefault("generated_title", f"Fixture {text}")
    save()
    say(f"reply {os.getpid()}: {text}")


def turn(text):
    event(type="turn_started", session_id=sid, model_id=summary["current_model_id"])
    update("user_message_chunk", content={"type": "text", "text": text})
    event(type="phase_changed", phase="waiting_for_model")
    event(type="phase_changed", phase="tool_execution")
    update("tool_call", toolCallId=str(uuid.uuid4()), title="run_terminal_command",
           rawInput={"command": "date"})
    if "approve?" in text:
        event(type="phase_changed", phase="permission_prompt")
        event(type="permission_requested", tool_name="run_terminal_command")
        say("Allow run_terminal_command? (press enter)")
        return text
    finish(text)
    return None


save()
say(f"grok fixture {sid}")
say(f"argv: {' '.join(args)}")
waiting = turn(args[args.index("--") + 1]) if "--" in args else None
for line in sys.stdin:
    line = line.strip()
    if waiting is not None:
        event(type="permission_resolved", tool_name="run_terminal_command", decision="allow")
        event(type="phase_changed", phase="tool_execution")
        finish(waiting)
        waiting = None
    elif line:
        say(f"> {line}")
        waiting = turn(line)
