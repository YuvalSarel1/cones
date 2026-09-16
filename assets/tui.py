#!/usr/bin/env python3
"""Render assets/tui.svg, a screenshot of `cones tui` on a fixed cast of sessions, from a tmux pane.
Run from the repo root with a built binary: python3 assets/tui.py [path/to/cones] [jobs file].

The capture runs against a home of its own, with a fixture `claude` on its PATH: the dashboard
attaches to the selected row through it, so the pane peeks into a running agent, and no session
of this machine's, no model call and no key of its own is in reach."""
import html, json, os, re, shutil, subprocess, sys, tempfile, time, uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from tui_fixture import child_process

# Wide enough for the context column and the whole keys row, tall enough for both tables and the
# agent's screen in the pane, with no band of empty rows under them.
COLS, ROWS = 140, 26
# The dashboard starts in the fixture home, not in this checkout, so both paths are resolved
# here: a relative binary would otherwise come from a worktree that captures a stale build.
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/debug/cones")
JOBS = os.path.abspath(sys.argv[2] if len(sys.argv) > 2 else "jobs.example.yaml")
BG, FG, DIM = "#0d1117", "#e6edf3", "#7d8590"
ANSI16 = ["#000", "#f85149", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#e6edf3"] * 2
C256 = {202: "#ff5f00", 208: "#ff8700", 214: "#ffaf00", 237: "#3a3a3a"}  # the cone's tones and the menu button fill

# The rows the asset shows. A capture of the machine's own registry painted whatever happened
# to be running into the README, so the cast is written here instead: a folder with work in
# flight and one waiting on a human, another folder with a run that finished. `bars` is lines
# per minute over the sparkline's window, `context` the tokens the row reports.
# A home of the capture's own. The dashboard shortens paths under it, so the folder rows read
# ~/personal/cones and ~/work while nothing of this machine's home is in reach.
HOME = tempfile.mkdtemp(prefix="coneshome-")
CWD = os.path.join(HOME, "personal", "cones")  # the dashboard's folder, where the menu row launches
CAST = [
    (".", "busy", "the ledger's write path", "claude-opus-5", 56_000, [1, 2, 5, 7, 6, 3, 2, 4], "Reading the ledger writer to see where the lock is taken"),
    (".", "waiting", "sparkline bounds", "claude-opus-5", 86_000, [2, 3, 2, 1, 1, 2, 1, 1], "Two ways to scale the bars; which one do you want?"),
    (".", "busy", "the harness table in the docs", "claude-sonnet-5", 120_000, [4, 3, 6, 5, 2, 3, 5, 4], "Every state a harness reports now has a row"),
    ("work", "busy", "the flaky checkout test", "claude-opus-5", 67_000, [1, 1, 2, 3, 3, 2, 1, 2], "The failure needs the clock frozen, not another retry"),
    ("work", "done", "the invoice export endpoint", "claude-sonnet-5", 41_000, [2, 4, 3, 1, 1, 1, 1, 1], "Shipped behind the export flag; the tests cover both currencies"),
]

# The `claude` the dashboard finds on the fixture home's PATH. `attach` paints one screen of the
# first row's session, the one the dashboard selects, and then holds the pty open, so the pane
# shows a running agent for the capture. A real attach would put this machine's own work, and a
# model call, into a committed asset.
PEEK = r'''
import os, sys

if len(sys.argv) < 2 or sys.argv[1] != "attach":
    print("2.1.0 (Claude Code): --bg, attach")  # what a version or capability check reads
    raise SystemExit(0)

O, D, B, R = "\x1b[38;5;208m", "\x1b[2m", "\x1b[1m", "\x1b[0m"
SCREEN = [
    f"{D}> Give every state a harness reports its own row in the table{R}",
    "",
    f"{O}⏺{R} Read({B}docs/harness.md{R})",
    f"  {D}⎿  148 lines{R}",
    "",
    f"{O}⏺{R} Update({B}docs/harness.md{R})",
    f"  {D}⎿  6 rows: queued, working, input, idle, done, gone{R}",
    "",
    f"{O}⏺{R} Bash({B}cargo test --all-targets harness{R})",
    f"  {D}⎿  3 passed in 4.1s{R}",
    "",
    f"{O}⏺{R} Every state a harness reports now has a row.",
    "",
    f"{O}✻{R} Working{D}… (24s · ↑ 1.2k tokens · esc to interrupt){R}",
]
cols, rows = os.get_terminal_size(0)
box = cols - 2
composer = [f"{D}╭{'─' * box}╮{R}", f"{D}│{R} > {' ' * (box - 3)}{D}│{R}", f"{D}╰{'─' * box}╯{R}"]
body = SCREEN + [""] * max(0, rows - len(SCREEN) - len(composer)) + composer
sys.stdout.write("\x1b[?1049h\x1b[2J\x1b[H" + "\r\n".join(body[:rows]))
sys.stdout.flush()
# The dashboard closes a viewer by closing its pty, so end of input is the exit.
while os.read(0, 1) not in (b"", b"\x1a"):
    pass
sys.stdout.write("\x1b[?1049l")
'''

def tmux(*a, **k): return subprocess.run(["tmux", *a], text=True, capture_output=True, **k)

def cast(claude):
    """Claude's own layout under `claude`: a registry entry, a transcript and a statusLine
    payload per row. The pid is this script's, alive for the capture, so cones reads the rows
    as live; `bg` is the kind a background session has, the one the dashboard opens a viewer
    for, which is how the pane peeks into the selected row through the fixture `claude`."""
    now = datetime.now(timezone.utc)
    for i, (folder, status, title, model, context, bars, last) in enumerate(CAST):
        # "." is the folder the dashboard itself opens in, where its own work sits.
        sid = f"{uuid.uuid4()}"
        cwd = CWD if folder == "." else os.path.join(HOME, folder)
        os.makedirs(cwd, exist_ok=True)
        os.makedirs(os.path.join(claude, "sessions"), exist_ok=True)
        os.makedirs(os.path.join(claude, "statusline"), exist_ok=True)
        project = os.path.join(claude, "projects", re.sub(r"[^A-Za-z0-9]", "-", cwd))
        os.makedirs(project, exist_ok=True)
        started = int((now - timedelta(minutes=len(bars) + i + 4)).timestamp() * 1000)
        json.dump({"pid": os.getpid(), "sessionId": sid, "cwd": cwd, "kind": "bg",
                   "status": status, "startedAt": started, "updatedAt": started},
                  open(os.path.join(claude, "sessions", f"{sid}.json"), "w"))
        json.dump({"context_window": {"context_window_size": 1_000_000}},
                  open(os.path.join(claude, "statusline", f"{sid}.json"), "w"))
        # One line per count in each bucket, oldest bucket first, so the sparkline has a
        # shape; the last line is the reply the `last` column shows.
        lines = [json.dumps({"type": "ai-title", "aiTitle": title})]
        # Each row's timeline sits a minute further back than the one before, so no two
        # agents report in lockstep and the rows keep one order between captures.
        for back, count in enumerate(reversed(bars)):
            at = (now - timedelta(minutes=back + i, seconds=20)).strftime("%Y-%m-%dT%H:%M:%SZ")
            for n in range(count):
                text = last if (back, n) == (0, count - 1) else f"working on {title}"
                lines.append(json.dumps({"type": "assistant", "timestamp": at, "message": {
                    "id": f"m{i}-{back}-{n}", "model": model,
                    "usage": {"input_tokens": 12, "cache_read_input_tokens": context, "output_tokens": 40},
                    "content": [{"type": "text", "text": text}]}}))
        open(os.path.join(project, f"{sid}.jsonl"), "w").write("\n".join(reversed(lines)) + "\n")

def runs(state):
    """One finished run of the example job, in a state dir of its own: the machine's own ledger
    holds whatever ran here today, and its job names have no place in a committed asset."""
    os.makedirs(state, mode=0o700, exist_ok=True)
    fired = datetime.now(timezone.utc) - timedelta(hours=3)
    stamp = lambda t: t.strftime("%Y-%m-%dT%H:%M:%SZ")
    run_id = f"{uuid.uuid4()}"
    records = [
        {"v": 1, "run_id": run_id, "status": "started", "job": "readme-check", "trigger": "schedule",
         "fired_at": stamp(fired), "harness": "claude", "cwd": os.path.abspath(CWD)},
        {"v": 1, "run_id": run_id, "status": "ok", "ended_at": stamp(fired + timedelta(seconds=74)),
         "duration_s": 74.2, "exit": 0, "tokens_in": 18_402, "tokens_out": 1_120, "cost_usd": 0.21},
    ]
    open(os.path.join(state, "runs.jsonl"), "w").write("\n".join(json.dumps(r) for r in records) + "\n")

claude = os.path.join(HOME, ".claude")
state = os.path.join(HOME, "state")
peek = os.path.join(HOME, ".local", "bin", "claude")
os.makedirs(os.path.dirname(peek))
open(peek, "w").write(f"#!{sys.executable}\n{PEEK}")
os.chmod(peek, 0o755)
cast(claude)
runs(state)
session = f"conescap-{uuid.uuid4().hex[:8]}"
# The dashboard resolves `claude` under HOME/.local/bin, reads the registry under
# CLAUDE_CONFIG_DIR and keeps its runs in --state-dir, so all three come from the fixture home;
# CODEX_HOME goes with it, or the machine's own Codex threads land in the asset. The example job
# is the jobs file, so no job row has a run of this machine's in flight.
try:
    # tmux holds the terminal; this script owns the dashboard and waits for it.
    tmux("new-session", "-d", "-s", session, "-c", CWD, "-x", str(COLS), "-y", str(ROWS), "exec /bin/sleep 3600", check=True)
    tty = tmux("display-message", "-p", "-t", session, "#{pane_tty}", check=True).stdout.strip()
    command = ["env", "-u", "NO_COLOR", "-u", "CODEX_HOME", "-u", "PI_CODING_AGENT_DIR",
               "TERM=xterm-256color", f"HOME={HOME}", f"CLAUDE_CONFIG_DIR={claude}",
               BIN, "--jobs", JOBS, "--state-dir", state]
    with open(tty, "r+b", buffering=0) as terminal:
        with child_process(command, cwd=CWD, stdin=terminal, stdout=terminal, stderr=terminal):
            # Give the selected fixture viewer time to attach and paint.
            time.sleep(8)
            lines = tmux("capture-pane", "-p", "-e", "-t", session, check=True).stdout.rstrip("\n").split("\n")
finally:
    tmux("kill-session", "-t", session)
    shutil.rmtree(HOME, ignore_errors=True)

CW, LH, PAD, FS = 8.43, 20, 16, 14
W, H = int(COLS * CW + 2 * PAD), ROWS * LH + 2 * PAD
out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="SFMono-Regular,Menlo,Consolas,monospace" font-size="{FS}">',
       f'<rect width="{W}" height="{H}" rx="8" fill="{BG}"/>']
for row, line in enumerate(lines):
    fg, bg, bold, dim, col = FG, None, False, False, 0
    y = PAD + row * LH + FS
    for tok in re.split(r"(\x1b\[[0-9;]*m)", line):
        if tok.startswith("\x1b["):
            p = [int(x or 0) for x in tok[2:-1].split(";")]
            i = 0
            while i < len(p):
                c = p[i]
                if c == 0: fg, bg, bold, dim = FG, None, False, False
                elif c == 49: bg = None
                elif c == 1: bold = True
                elif c == 2: dim = True
                elif c == 22: bold = dim = False
                elif c == 39: fg = FG
                elif 30 <= c <= 37: fg = ANSI16[c - 30]
                elif 90 <= c <= 97: fg = ANSI16[c - 90]
                elif c in (38, 48) and p[i + 1] == 5:
                    n5 = p[i + 2]; i += 2
                    color = ANSI16[n5] if n5 < 16 else C256.get(n5, FG)
                    if c == 38: fg = color
                    else: bg = color
                elif c == 38 and p[i + 1] == 2:
                    fg = "#%02x%02x%02x" % tuple(p[i + 2:i + 5]); i += 4
                i += 1
        elif tok.strip():
            blocks = all(c in " ▀▄█" for c in tok)
            if bg:  # a filled cell, the menu's buttons
                out.append(f'<rect x="{PAD + col * CW:.1f}" y="{y - FS - 3}" width="{len(tok) * CW:.1f}" height="{LH}" rx="{0 if blocks else 3}" fill="{bg}"/>')
            if blocks:
                # Block glyphs fill terminal cells; font leading must not add seams to the sprite.
                for offset, char in enumerate(tok):
                    if char == " ":
                        continue
                    top = y - FS - 3 + (LH / 2 if char == "▄" else 0)
                    height = LH if char == "█" else LH / 2
                    out.append(f'<rect x="{PAD + (col + offset) * CW:.2f}" y="{top}" width="{CW}" height="{height}" fill="{DIM if dim else fg}"/>')
            else:
                style = f' fill="{DIM if dim else fg}"' + (' font-weight="700"' if bold else "")
                out.append(f'<text x="{PAD + col * CW:.1f}" y="{y}" xml:space="preserve" textLength="{len(tok) * CW:.1f}" lengthAdjust="spacingAndGlyphs"{style}>{html.escape(tok)}</text>')
            col += len(tok)
        else:
            col += len(tok)
out.append("</svg>")
open("assets/tui.svg", "w").write("\n".join(out))
print(f"assets/tui.svg: {W}x{H}")
