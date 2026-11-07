#!/usr/bin/python3
"""A `claude` for the dashboard flows in tests/dashboard_claude.rs. No model is contacted.

Every call appends its argv to `$HOME/claude-calls.jsonl`, so a test can assert which native
commands the dashboard ran, and its environment to `$HOME/claude-env.jsonl`. `$HOME/fake-launch` selects how `--bg` behaves (default `ok`):
  ok    hand the session to a stand-in daemon (`/bin/sleep`), list it in the registry with a
        transcript and an AI title, and print the background id the way Claude does
  fail  refuse to start, with a diagnostic on stderr
  untrusted  refuse `--bg` with Claude's own trust error until `$HOME/trusted` exists or
        `$CLAUDE_CONFIG_DIR/.claude.json` accepts the folder's trust dialog; a
        foreground start asks Claude's trust question on the pty, and enter trusts the folder,
        lists the session in the registry under the fixture's pid and holds the pty
With `$HOME/fake-resume-in-place` naming a socket, `--bg --resume ID` lists session ID again
under a new stand-in, keeps its transcript and publishes that socket as its cross-session
inbox; otherwise a resume is a copy under a new id. `$HOME/registry-delay` holds a `--bg` session's registry entry back that many seconds, the
way the daemon can take a while to report a session. `$HOME/print-delay` holds the printed
background id back instead, the way the daemon can list a session before `--bg` returns.
`attach` holds the pty as a viewer client until hung up. With `$HOME/fake-attach-fullscreen` it
draws the composer of Claude's fullscreen renderer (`CLAUDE_CODE_NO_FLICKER=1`): hidden terminal
cursor, inverse software caret after `❯`. Typed text becomes a draft and every read from the pty
is logged to `$HOME/attach-input.jsonl`. `$HOME/fake-attach-mouse` also turns on SGR mouse
reporting, as Claude's fullscreen renderer does, so wheel reports reach it. `rm` ends the session the way the
daemon does: registry entry and job record removed, stand-in killed. With
`$CLAUDE_CONFIG_DIR/rm-worktree-<short>` naming a worktree, `rm` refuses the way Claude does for
unpushed commits until it is given `--discard-unpushed`.
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
with open(HOME / "claude-env.jsonl", "a") as log:
    log.write(json.dumps(dict(os.environ)) + "\n")

if "--help" in args and args[:1] != ["stop"]:
    print("--bg  start a background session\nattach  join one\n--fork-session  fork one")
    sys.exit(0)

if args[:1] == ["stop"]:
    print("Usage: claude stop <id>")
    sys.exit(0)

if args[:1] == ["attach"] and (HOME / "fake-attach-fullscreen").exists():
    import select
    import tty
    fd = sys.stdin.fileno()
    tty.setraw(fd)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    draft = ""
    mouse = "\x1b[?1000h\x1b[?1006h" if (HOME / "fake-attach-mouse").exists() else ""
    while True:
        sys.stdout.write(f"{mouse}\x1b[?25l\x1b[2J\x1b[Hfixture attached to {args[1]}\r\n"
                         f"{'─' * 20}\r\n❯ {draft}\x1b[7m \x1b[0m\r\n{'─' * 20}")
        sys.stdout.flush()
        if not select.select([fd], [], [], 0.5)[0]:
            continue
        data = os.read(fd, 1024)
        if not data:
            sys.exit(0)
        text = data.decode(errors="replace")
        with open(HOME / "attach-input.jsonl", "a") as log:
            log.write(json.dumps(text) + "\n")
        if text == "\x7f":
            draft = draft[:-1]
        elif text.isprintable():
            draft += text

if args[:1] == ["attach"]:
    sys.stdout.write(f"fixture attached to {args[1]}\r\n")
    sys.stdout.flush()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        time.sleep(0.05)

if args[:1] == ["rm"]:
    short = args[1]
    worktree = CLAUDE / f"rm-worktree-{short}"
    if worktree.exists() and "--discard-unpushed" not in args:
        # Claude keeps a session whose worktree holds commits no remote has.
        path = worktree.read_text().strip()
        head = subprocess.run(["git", "-C", path, "rev-parse", "HEAD"], capture_output=True,
                              text=True, check=True).stdout.strip()
        print(f"kept {short} \u2014 its worktree is still at \u201c{path}\u201d\n"
              f"  1 unpushed commits on \u201cfeature\u201d. They exist on no remote.\n"
              f"  push them and run 'claude rm {short}' again, or discard the worktree and its "
              f"commits: claude rm {short} --discard-unpushed {head}@0123abcd", file=sys.stderr)
        sys.exit(1)
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
trusted = HOME / "trusted"
def accepted():
    try:
        projects = json.loads((CLAUDE / ".claude.json").read_text()).get("projects", {})
    except FileNotFoundError:
        return False
    return projects.get(os.getcwd(), {}).get("hasTrustDialogAccepted") is True
if mode == "untrusted" and "--bg" in args and not (trusted.exists() or accepted()):
    print(f"Workspace not trusted. Run `claude` in {os.getcwd()} once and accept the trust "
          "prompt, then retry.", file=sys.stderr)
    sys.exit(1)
if "--bg" not in args:
    sys.stdout.write(f"Quick safety check: do you trust {os.getcwd()}?\r\n")
    sys.stdout.flush()
    sys.stdin.readline()
    trusted.touch()
    session = str(uuid.uuid4())
    registry = CLAUDE / "sessions"
    registry.mkdir(parents=True, exist_ok=True)
    (registry / f"{os.getpid()}.json").write_text(json.dumps({
        "pid": os.getpid(), "sessionId": session, "cwd": os.getcwd(), "kind": "interactive",
        "status": "busy", "startedAt": int(time.time() * 1000),
    }))
    sys.stdout.write(f"fixture trusted the folder and started: {prompt}\r\n")
    sys.stdout.flush()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        time.sleep(0.05)
in_place = HOME / "fake-resume-in-place"
resume = (args[args.index("--resume") + 1]
          if "--resume" in args and in_place.exists() else None)
session = resume or str(uuid.uuid4())
short = session[:8]
cwd = os.getcwd()
daemon = subprocess.Popen(["/bin/sleep", "600"], start_new_session=True,
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL)
key = "".join(c if c.isascii() and c.isalnum() else "-" for c in cwd)
transcript = CLAUDE / "projects" / key / f"{session}.jsonl"
transcript.parent.mkdir(parents=True, exist_ok=True)
if not resume:
  transcript.write_text(
    json.dumps({"sessionId": session, "type": "user", "cwd": cwd,
                "timestamp": "2026-10-01T09:00:00.000Z",
                "message": {"role": "user", "content": prompt}}) + "\n"
    + json.dumps({"type": "ai-title", "sessionId": session,
                  "aiTitle": "Fixture title for the launch"}) + "\n"
)
registry = CLAUDE / "sessions"
registry.mkdir(parents=True, exist_ok=True)
entry = {
    "pid": daemon.pid, "sessionId": session, "cwd": cwd, "kind": "bg",
    "jobId": short, "status": "busy", "startedAt": int(time.time() * 1000),
}
if resume:
    import hashlib
    socket = in_place.read_text()
    entry.update({"messagingSocketPath": socket, "peerProtocol": 1, "version": "2.1.281"})
    digest = hashlib.sha256(socket.encode()).hexdigest()
    registry.mkdir(parents=True, exist_ok=True)
    (registry / f"{daemon.pid}.{digest}.key").write_text(
        json.dumps({"peerToken": "0123456789abcdef0123456789abcdef"}))
entry = json.dumps(entry)
delay_file = HOME / "registry-delay"
if delay_file.exists() and os.fork() == 0:
    os.setsid()
    null = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(null, fd)
    time.sleep(float(delay_file.read_text()))
    (registry / f"{daemon.pid}.json").write_text(entry)
    os._exit(0)
if not delay_file.exists():
    (registry / f"{daemon.pid}.json").write_text(entry)
print_delay = HOME / "print-delay"
if print_delay.exists():
    time.sleep(float(print_delay.read_text()))
print(f"backgrounded · {short} (busy)")
