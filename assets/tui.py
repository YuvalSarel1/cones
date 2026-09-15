#!/usr/bin/env python3
"""Render assets/tui.svg, a screenshot of a live `cones tui`, from a tmux pane.
Run from the repo root with a built binary: python3 assets/tui.py [path/to/cones] [folder to open in] [jobs file]."""
import html, re, shlex, subprocess, sys, time, uuid

COLS, ROWS = 120, 34
BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/cones"
CWD = sys.argv[2] if len(sys.argv) > 2 else "."  # the dashboard's folder, where the menu row launches
JOBS = sys.argv[3] if len(sys.argv) > 3 else "jobs.example.yaml"  # the example job, so no live session's viewer opens
BG, FG, DIM = "#0d1117", "#e6edf3", "#7d8590"
ANSI16 = ["#000", "#f85149", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#e6edf3"] * 2
C256 = {202: "#ff5f00", 208: "#ff8700", 214: "#ffaf00", 237: "#3a3a3a"}  # the cone's tones and the menu button fill

def tmux(*a, **k): return subprocess.run(["tmux", *a], text=True, capture_output=True, **k)

session = f"conescap-{uuid.uuid4().hex[:8]}"
# Start on the example job, so capturing the dashboard does not open a live session's viewer.
tmux("new-session", "-d", "-s", session, "-c", CWD, "-x", str(COLS), "-y", str(ROWS), f"env -u NO_COLOR {shlex.quote(BIN)} --jobs {shlex.quote(JOBS)} tui", check=True)
try:
    time.sleep(5)
    # The cursor starts on the first session row, whose live viewer would paint another
    # agent's transcript into the pane; `up` lands on the menu row, where the pane shows the
    # picked button's screen instead. Nothing of a live session goes into a committed asset.
    tmux("send-keys", "-t", session, "Up", check=True)
    time.sleep(1)
    tmux("send-keys", "-t", session, "Right", check=True)
    time.sleep(2)
    lines = tmux("capture-pane", "-p", "-e", "-t", session, check=True).stdout.rstrip("\n").split("\n")
finally:
    tmux("kill-session", "-t", session)

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
