#!/usr/bin/python3
"""An interactive pi stand-in for the dashboard's native viewer tests. No model is contacted.

Installed under a fixture HOME as `.local/bin/pi`, where `harness::launch_path` looks first.
The shebang names the interpreter directly so `ps` shows `python3 <path>/pi`, the form pi's
process discovery recognises.

It behaves like pi's terminal client: it takes the raw pty, draws the prompt it was given
above pi's standard editor (full-width rules around one line with an inverse-video software
caret, the hardware cursor hidden on it), echoes keys into that editor and prints each
submitted line. While it runs it publishes the private report cones' extension would write.

`$HOME/pi-fixture/<pid>.json` records what this process was asked to do, so a test can tell
its fixture from any other pi: argv, cwd, the report path and every submitted line.
"""
import fcntl
import json
import os
import pathlib
import select
import struct
import sys
import termios
import tty
import uuid

args = sys.argv[1:]
if "--version" in args:
    print("0.0.0-fixture")
    sys.exit(0)
if "--help" in args:
    print("--fork <file>  fork a session\n--session-id <id>  name it")
    sys.exit(0)

prompt = " ".join(args[args.index("--") + 1:]) if "--" in args else ""
report = os.environ.get("CONES_PI_REPORT")
session = str(uuid.uuid4())
record_path = pathlib.Path(os.environ["HOME"]) / "pi-fixture" / f"{os.getpid()}.json"
record_path.parent.mkdir(parents=True, exist_ok=True)
record = {"pid": os.getpid(), "argv": sys.argv, "cwd": os.getcwd(), "report": report,
          "submitted": []}


def save():
    tmp = record_path.with_suffix(".tmp")
    tmp.write_text(json.dumps(record))
    tmp.replace(record_path)


def publish():
    if not report:
        return
    value = {"version": 1, "pid": os.getpid(),
             "session": {"id": session, "directory": os.getcwd(), "file": None},
             "waiting": False, "idle": True}
    tmp = report + ".tmp"
    with open(tmp, "w") as f:
        json.dump(value, f)
    os.replace(tmp, report)


def size():
    try:
        rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
        return max(rows, 6), max(cols, 20)
    except OSError:
        return 24, 80


history = [f"pi fixture ready: {prompt}"]
draft = ""


def draw():
    rows, cols = size()
    out = ["\x1b[?25l\x1b[2J\x1b[H"]
    for i, line in enumerate(history[-(rows - 4):]):
        out.append(f"\x1b[{i + 1};1H{line[:cols]}")
    rule = "─" * cols
    top = rows - 2
    out.append(f"\x1b[{top};1H{rule}")
    out.append(f"\x1b[{top + 1};1H {draft}\x1b[7m \x1b[0m")
    out.append(f"\x1b[{top + 2};1H{rule}")
    out.append(f"\x1b[{top + 1};{len(draft) + 2}H")
    sys.stdout.write("".join(out))
    sys.stdout.flush()


save()
publish()
fd = sys.stdin.fileno()
tty.setraw(fd)
draw()
while True:
    ready, _, _ = select.select([fd], [], [], 0.5)
    publish()
    if not ready:
        draw()
        continue
    data = os.read(fd, 1024)
    if not data:
        break
    text = data.decode("utf-8", "replace")
    if text.startswith("\x1b"):
        continue
    for c in text:
        if c in "\r\n":
            if draft:
                history.append(f"pi heard: {draft}")
                record["submitted"].append(draft)
                save()
            draft = ""
        elif c == "\x7f":
            draft = draft[:-1]
        elif c.isprintable():
            draft += c
    draw()
