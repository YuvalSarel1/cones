#!/usr/bin/python3
"""A deterministic subprocess fixture. It never contacts a model."""
import json
import os
import pathlib
import signal
import subprocess
import sys
import time

args = sys.argv[1:]
session = args[args.index("--session-id") + 1]
mode = args[args.index("--model") + 1]
ledger = pathlib.Path(os.environ["FAKE_LEDGER"])
starts = [json.loads(line) for line in ledger.read_text().splitlines()]
assert any(r.get("session_id") == session and r["status"] == "started" for r in starts)


def emit(value):
    print(json.dumps(value), flush=True)


emit({"type": "system", "subtype": "init", "session_id": session})
cwd_key = "".join(c if c.isascii() and c.isalnum() else "-" for c in os.getcwd())
transcript = pathlib.Path.home() / ".claude/projects" / cwd_key / (session + ".jsonl")
transcript.parent.mkdir(parents=True, exist_ok=True)
transcript.write_text(json.dumps({"sessionId": session, "type": "user"}) + "\n")

if mode in ("hang", "slow", "descendant"):
    if mode in ("hang", "descendant"):
        child = subprocess.Popen(
            ["/bin/sh", "-c", "trap '' TERM; while :; do sleep 1; done"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        pathlib.Path(os.environ["FAKE_CHILD_PID"]).write_text(str(child.pid))
    if mode == "hang":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(120)
    if mode == "slow":
        time.sleep(2)

if mode == "permission":
    emit({"type": "system", "subtype": "permission_denied", "session_id": session})
    time.sleep(120)
if mode == "sandbox":
    emit({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "bash-denied", "name": "Bash", "input": {"command": "touch outside-workspace"}}
    ]}})
    emit({
        "type": "user",
        "message": {"content": [{
            "type": "tool_result", "tool_use_id": "bash-denied", "is_error": True,
            "content": "Exit code 1\ntouch: outside-workspace: Operation not permitted"
        }]}
    })
    time.sleep(120)
if mode == "read-permissions":
    fixture = pathlib.Path(__file__).with_name("claude-read-permissions.jsonl")
    for line in fixture.read_text().splitlines():
        event = json.loads(line)
        if "session_id" in event:
            event["session_id"] = session
        emit(event)
    sys.exit(0)
if mode == "malformed":
    print("not json", flush=True)
    time.sleep(120)
if mode == "mismatch":
    emit({"type": "system", "session_id": "wrong"})
    time.sleep(120)
if mode == "missing":
    sys.exit(0)
if mode == "failed":
    emit({"type": "result", "subtype": "error_during_execution", "is_error": True,
          "session_id": session})
    sys.exit(0)
if mode == "oversized":
    print("x" * (1024 * 1024 + 2), flush=True)
    time.sleep(120)

emit({
    "type": "result", "subtype": "success", "is_error": False,
    "session_id": session, "total_cost_usd": 0.01,
    "usage": {"input_tokens": 30, "output_tokens": 10},
    "permission_denials": []
})
