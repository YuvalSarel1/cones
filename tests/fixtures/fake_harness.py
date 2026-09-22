#!/usr/bin/python3
"""Disposable launch targets for the detached-launch tests. No model is contacted.

Installed under a fixture HOME as `.local/bin/claude` and `.local/bin/pi`, which is where
`harness::launch_path` looks first. The shebang names the interpreter directly so `ps` shows
`python3 <path>/pi`, the form pi's process discovery recognises.

`FAKE_LAUNCH` selects the failure being exercised:
  ok       start normally (default)
  no-id    start, print an unusable line instead of a background id
  fail     refuse to start, with a diagnostic on stderr
"""
import json
import os
import pathlib
import signal
import subprocess
import sys
import time
import uuid

name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
mode = os.environ.get("FAKE_LAUNCH", "ok")

if name == "claude":
    if "--help" in args:
        print("--bg  start a background session\nattach  join one")
        sys.exit(0)
    if mode == "fail":
        print("fixture refused this launch", file=sys.stderr)
        sys.exit(3)
    session = str(uuid.uuid4())
    # A background launch hands the session to something that outlives the launcher, and a
    # registry entry whose pid is dead is not a session. This stand-in daemon is that process;
    # the test kills every pid it finds in this registry.
    daemon = subprocess.Popen(["/bin/sleep", "600"], start_new_session=True,
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    home = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
    registry = home / "sessions"
    registry.mkdir(parents=True, exist_ok=True)
    (registry / f"{session}.json").write_text(json.dumps({
        "pid": daemon.pid, "sessionId": session, "cwd": os.getcwd(),
        "startedAt": int(time.time() * 1000), "kind": "bg", "status": "idle",
        "jobId": session[:8], "entrypoint": "cli",
    }))
    if mode == "no-id":
        print("started, somewhere")
        sys.exit(0)
    print(f"backgrounded · {session[:8]} (idle)")
    sys.exit(0)

if name == "pi":
    if "--version" in args:
        print("0.0.0-fixture")
        sys.exit(0)
    if mode == "fail":
        print("fixture refused this launch", file=sys.stderr)
        sys.exit(3)
    # A terminal client: hold the pty, report the prompt it was given, and wait to be
    # stopped. This is what a detached launch has to keep alive past its launcher.
    sys.stdout.write(f"pi fixture ready: {' '.join(args)}\r\n")
    sys.stdout.flush()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        time.sleep(0.05)

print(f"unexpected fixture name {name}", file=sys.stderr)
sys.exit(2)
