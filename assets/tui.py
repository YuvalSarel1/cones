#!/usr/bin/env python3
"""Render assets/tui.svg, a screenshot of `cones tui` on a fixed cast of sessions, from a tmux pane.
Run from the repo root with a built binary: python3 assets/tui.py [path/to/cones] [folder to open in] [jobs file]."""
import html, json, os, re, shlex, shutil, subprocess, sys, tempfile, time, uuid
from datetime import datetime, timedelta, timezone

COLS, ROWS = 120, 34
# A relative binary path resolves against CWD below, the dashboard's folder, not the shell's:
# from a worktree that captures the main checkout's stale binary. Pass BIN absolute, and pass
# the main repo as the folder, or its path lands in the header of a committed asset.
BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/cones"
CWD = sys.argv[2] if len(sys.argv) > 2 else "."  # the dashboard's folder, where the menu row launches
JOBS = sys.argv[3] if len(sys.argv) > 3 else "jobs.example.yaml"  # the example job, so no live session's viewer opens
BG, FG, DIM = "#0d1117", "#e6edf3", "#7d8590"
ANSI16 = ["#000", "#f85149", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#e6edf3"] * 2
C256 = {202: "#ff5f00", 208: "#ff8700", 214: "#ffaf00", 237: "#3a3a3a"}  # the cone's tones and the menu button fill

# The rows the asset shows. A capture of the machine's own registry painted whatever happened
# to be running into the README, so the cast is written here instead: a folder with work in
# flight and one waiting on a human, another folder with a run that finished. `bars` is lines
# per minute over the sparkline's window, `context` the tokens the row reports.
HOME = os.path.expanduser("~")
CAST = [
    (".", "busy", "the ledger's write path", "claude-opus-5", 56_000, [1, 2, 5, 7, 6, 3, 2, 4], "Reading the ledger writer to see where the lock is taken"),
    (".", "waiting", "sparkline bounds", "claude-opus-5", 86_000, [2, 3, 2, 1, 1, 2, 1, 1], "Two ways to scale the bars; which one do you want?"),
    (".", "busy", "the harness table in the docs", "claude-sonnet-5", 120_000, [4, 3, 6, 5, 2, 3, 5, 4], "Every state a harness reports now has a row"),
    ("work", "busy", "the flaky checkout test", "claude-opus-5", 67_000, [1, 1, 2, 3, 3, 2, 1, 2], "The failure needs the clock frozen, not another retry"),
    ("work", "done", "the invoice export endpoint", "claude-sonnet-5", 41_000, [2, 4, 3, 1, 1, 1, 1, 1], "Shipped behind the export flag; the tests cover both currencies"),
]

def tmux(*a, **k): return subprocess.run(["tmux", *a], text=True, capture_output=True, **k)

def cast(claude):
    """Claude's own layout under `claude`: a registry entry, a transcript and a statusLine
    payload per row. The pid is this script's, alive for the capture, so cones reads the rows
    as live; `interactive` is the kind a session in its own terminal has, which the dashboard
    never opens a viewer for, so capturing launches no client."""
    now = datetime.now(timezone.utc)
    for i, (folder, status, title, model, context, bars, last) in enumerate(CAST):
        # "." is the folder the dashboard itself opens in, where its own work sits.
        sid = f"{uuid.uuid4()}"
        cwd = os.path.abspath(CWD) if folder == "." else os.path.join(HOME, folder)
        os.makedirs(os.path.join(claude, "sessions"), exist_ok=True)
        os.makedirs(os.path.join(claude, "statusline"), exist_ok=True)
        project = os.path.join(claude, "projects", re.sub(r"[^A-Za-z0-9]", "-", cwd))
        os.makedirs(project, exist_ok=True)
        started = int((now - timedelta(minutes=len(bars) + i + 4)).timestamp() * 1000)
        json.dump({"pid": os.getpid(), "sessionId": sid, "cwd": cwd, "kind": "interactive",
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

claude = tempfile.mkdtemp(prefix="conescast-")
cast(claude)
session = f"conescap-{uuid.uuid4().hex[:8]}"
# CLAUDE_CONFIG_DIR is the override Claude Code itself honors, so the dashboard reads the cast
# above and nothing of this machine's own work. Start on the example job, so no job row's run
# is live either.
tmux("new-session", "-d", "-s", session, "-c", CWD, "-x", str(COLS), "-y", str(ROWS), f"env -u NO_COLOR CLAUDE_CONFIG_DIR={shlex.quote(claude)} {shlex.quote(BIN)} --jobs {shlex.quote(JOBS)} tui", check=True)
try:
    time.sleep(5)
    # `up` lands on the menu row, where the pane shows the picked button's screen rather than
    # a session's viewer, and `right` picks the button the asset shows.
    tmux("send-keys", "-t", session, "Up", check=True)
    time.sleep(1)
    tmux("send-keys", "-t", session, "Right", check=True)
    time.sleep(2)
    lines = tmux("capture-pane", "-p", "-e", "-t", session, check=True).stdout.rstrip("\n").split("\n")
finally:
    tmux("kill-session", "-t", session)
    shutil.rmtree(claude, ignore_errors=True)

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
