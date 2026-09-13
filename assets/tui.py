#!/usr/bin/env python3
"""Render assets/tui.svg, an animated capture of a live `cones tui`, from a tmux pane.
Run from the repo root with a built binary: python3 assets/tui.py [path/to/cones]."""
import html, re, subprocess, sys, time

COLS, ROWS, FRAMES, STEP = 120, 34, 14, 0.7
BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/cones"
KEYS = {3: "Down", 5: "Down", 8: "Down", 11: "Up"}  # what the cursor does between frames; never Enter, it starts a job
BG, FG, DIM = "#0d1117", "#e6edf3", "#7d8590"
ANSI16 = ["#000", "#f85149", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#e6edf3"] * 2

def tmux(*a, **k): return subprocess.run(["tmux", *a], text=True, capture_output=True, **k)

tmux("kill-session", "-t", "conescap")
tmux("new-session", "-d", "-s", "conescap", "-x", str(COLS), "-y", str(ROWS), f"{BIN} tui", check=True)
time.sleep(4)
frames = []
for i in range(FRAMES):
    if i in KEYS: tmux("send-keys", "-t", "conescap", KEYS[i])
    time.sleep(STEP)
    frames.append(tmux("capture-pane", "-p", "-e", "-t", "conescap", check=True).stdout.rstrip("\n").split("\n"))
tmux("kill-session", "-t", "conescap")

CW, LH, PAD, FS = 8.43, 20, 16, 14
W, H = int(COLS * CW + 2 * PAD), ROWS * LH + 2 * PAD
dur = FRAMES * STEP
out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="SFMono-Regular,Menlo,Consolas,monospace" font-size="{FS}">',
       f'<style>.f{{visibility:hidden;animation:s {dur}s steps(1) infinite}}@keyframes s{{0%{{visibility:visible}}{100/FRAMES:.3f}%{{visibility:hidden}}}}</style>',
       f'<rect width="{W}" height="{H}" rx="8" fill="{BG}"/>']
for n, lines in enumerate(frames):
    out.append(f'<g class="f" style="animation-delay:{n * STEP:.2f}s">')
    for row, line in enumerate(lines):
        fg, bold, dim, col = FG, False, False, 0
        y = PAD + row * LH + FS
        for tok in re.split(r"(\x1b\[[0-9;]*m)", line):
            if tok.startswith("\x1b["):
                p = [int(x or 0) for x in tok[2:-1].split(";")]
                i = 0
                while i < len(p):
                    c = p[i]
                    if c == 0: fg, bold, dim = FG, False, False
                    elif c == 1: bold = True
                    elif c == 2: dim = True
                    elif c == 22: bold = dim = False
                    elif c == 39: fg = FG
                    elif 30 <= c <= 37: fg = ANSI16[c - 30]
                    elif 90 <= c <= 97: fg = ANSI16[c - 90]
                    elif c == 38 and p[i + 1] == 5:
                        n5 = p[i + 2]; i += 2
                        fg = ANSI16[n5] if n5 < 16 else "#ff8700" if n5 == 208 else FG
                    elif c == 38 and p[i + 1] == 2:
                        fg = "#%02x%02x%02x" % tuple(p[i + 2:i + 5]); i += 4
                    i += 1
            elif tok.strip():
                style = f' fill="{DIM if dim else fg}"' + (' font-weight="700"' if bold else "")
                out.append(f'<text x="{PAD + col * CW:.1f}" y="{y}" xml:space="preserve" textLength="{len(tok) * CW:.1f}" lengthAdjust="spacingAndGlyphs"{style}>{html.escape(tok)}</text>')
                col += len(tok)
            else:
                col += len(tok)
    out.append("</g>")
out.append("</svg>")
open("assets/tui.svg", "w").write("\n".join(out))
print(f"assets/tui.svg: {FRAMES} frames, {dur:.1f}s loop")
