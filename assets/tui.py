#!/usr/bin/env python3
"""Generate assets/tui.gif and tui.svg using cones and the installed native CLIs.

Run: python3 assets/tui.py [--claude /path/to/claude] [--codex /path/to/codex]
Re-render saved cells without starting CLIs: python3 assets/tui.py --render-from /path/to/capture
Requires Pillow (python3 -m pip install Pillow).

Native CLIs execute the example tasks in isolated homes against a loopback provider.
Cones reads their real state and renders their live terminals. The recording browses
three running sessions in a twelve-session fleet, answers a Claude question, then
adds a folder and starts a new Codex session. Claude uses its native focus view with
compact edit counts. No external model is called. See tui_demo.py for the sample tasks.
"""
import argparse
import html
from functools import lru_cache
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
from tui_demo import prepare, provider


REPO = Path(__file__).resolve().parent.parent
COLS, ROWS = 146, 33
# Terminal background, text and muted colours from the owner's Claude Code reference.
BG, FG = "#191a1b", "#cccccc"
# Square half-block pixels keep terminal sprites at their native proportions.
CW, LH, PAD, FS = 10, 20, 16, 16

ANSI16 = [
    "#191a1b", "#b16e7a", "#56a366", "#ebcb8b", "#81a1c1", "#b4b9f5", "#88c0d0", "#cccccc",
    "#999999", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b4b9f5", "#8fbcbb", "#ffffff",
]
NAMED = dict(zip(
    ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "Gray",
     "DarkGray", "LightRed", "LightGreen", "LightYellow", "LightBlue", "LightMagenta", "LightCyan", "White"],
    ANSI16,
))

# Terminal graphics occupy the entire cell, regardless of font baseline or leading.
QUADRANTS = {
    "▀": 0b0011, "▄": 0b1100, "█": 0b1111, "▌": 0b0101, "▐": 0b1010,
    "▖": 0b0100, "▗": 0b1000, "▘": 0b0001, "▙": 0b1101, "▚": 0b1001,
    "▛": 0b0111, "▜": 0b1011, "▝": 0b0010, "▞": 0b0110, "▟": 0b1110,
}
BOX_LINES = {
    "─": [(0, .5, 1, .5)], "│": [(.5, 0, .5, 1)],
    "┌": [(.5, 1, .5, .5), (.5, .5, 1, .5)],
    "┐": [(0, .5, .5, .5), (.5, .5, .5, 1)],
    "└": [(.5, 0, .5, .5), (.5, .5, 1, .5)],
    "┘": [(0, .5, .5, .5), (.5, .5, .5, 0)],
    "├": [(.5, 0, .5, 1), (.5, .5, 1, .5)],
    "┤": [(.5, 0, .5, 1), (0, .5, .5, .5)],
    "┬": [(0, .5, 1, .5), (.5, .5, .5, 1)],
    "┴": [(0, .5, 1, .5), (.5, .5, .5, 0)],
    "┼": [(0, .5, 1, .5), (.5, 0, .5, 1)],
}


def block_rects(text):
    """Normalized rectangles for Unicode block elements, including the crab's eyes."""
    if text in QUADRANTS:
        mask = QUADRANTS[text]
        if mask == 15:
            return [(0, 0, 1, 1)]
        return [
            ((bit % 2) / 2, (bit // 2) / 2, (bit % 2 + 1) / 2, (bit // 2 + 1) / 2)
            for bit in range(4) if mask & (1 << bit)
        ]
    if len(text) == 1:
        code = ord(text)
        if 0x2581 <= code <= 0x2587:
            return [(0, 1 - (code - 0x2580) / 8, 1, 1)]
        if 0x2589 <= code <= 0x258F:
            return [(0, 0, (0x2590 - code) / 8, 1)]
        if text == "▔":
            return [(0, 0, 1, .125)]
        if text == "▕":
            return [(.875, 0, 1, 1)]
    return None


def braille_dots(text):
    """Braille pixels used by native terminal plots and Codex's subtle composer texture."""
    if len(text) != 1 or not 0x2800 <= ord(text) <= 0x28FF:
        return None
    mask = ord(text) - 0x2800
    positions = [(.25, .125), (.25, .375), (.25, .625), (.75, .125),
                 (.75, .375), (.75, .625), (.25, .875), (.75, .875)]
    return [position for bit, position in enumerate(positions) if mask & (1 << bit)]


def colour(value, fallback):
    if value == "Reset":
        return fallback
    if value in NAMED:
        return NAMED[value]
    numbers = [int(n) for n in re.findall(r"\d+", value)]
    if value.startswith("Rgb("):
        return "#%02x%02x%02x" % tuple(numbers)
    if value.startswith("Indexed("):
        n = numbers[0]
        if n < 16:
            return ANSI16[n]
        if n >= 232:
            return "#%02x%02x%02x" % ((8 + 10 * (n - 232),) * 3)
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (steps[n // 36], steps[n // 6 % 6], steps[n % 6])
    raise ValueError(f"unsupported terminal colour: {value}")


def render(cells):
    width, height = int(COLS * CW + PAD * 2), ROWS * LH + PAD * 2
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="SFMono-Regular,Menlo,Consolas,monospace" font-size="{FS}" shape-rendering="crispEdges">',
        '<title>cones: running Claude Code and Codex sessions</title>',
        '<desc>Sample API and web projects with native terminals running tests and waiting for input.</desc>',
        f'<rect width="{width}" height="{height}" rx="8" fill="{BG}" shape-rendering="auto"/>',
    ]
    # Every cell and style comes from ratatui, including spaces in Claude's diff.
    for index, cell in enumerate(cells):
        row, col = divmod(index, COLS)
        x, y = PAD + col * CW, PAD + row * LH
        fg, bg = colour(cell["fg"], FG), colour(cell["bg"], BG)
        if cell["reverse"]:
            fg, bg = bg, fg
        if cell["dim"]:
            fg = "#%02x%02x%02x" % tuple(
                (int(fg[n:n+2], 16) + int(bg[n:n+2], 16)) // 2 for n in (1, 3, 5)
            )
        if bg != BG:
            svg.append(f'<rect x="{x:.2f}" y="{y}" width="{CW}" height="{LH}" fill="{bg}"/>')
        text = cell["text"]
        if not text.strip():
            continue
        rectangles = block_rects(text)
        dots = braille_dots(text)
        if rectangles is not None:
            for left, top, right, bottom in rectangles:
                svg.append(
                    f'<rect x="{x + left * CW:.2f}" y="{y + top * LH}" '
                    f'width="{(right - left) * CW}" height="{(bottom - top) * LH}" fill="{fg}"/>'
                )
        elif dots is not None:
            for dx, dy in dots:
                svg.append(f'<circle cx="{x + dx * CW:.2f}" cy="{y + dy * LH}" r="1" fill="{fg}"/>')
        elif text in BOX_LINES:
            for x0, y0, x1, y1 in BOX_LINES[text]:
                svg.append(
                    f'<line x1="{x + x0 * CW:.2f}" y1="{y + y0 * LH}" '
                    f'x2="{x + x1 * CW:.2f}" y2="{y + y1 * LH}" stroke="{fg}"/>'
                )
        else:
            style = (' font-weight="700"' if cell["bold"] else "") + (
                ' text-decoration="underline"' if cell["underline"] else "")
            svg.append(f'<text x="{x:.2f}" y="{y + FS + 1}" fill="{fg}"{style}>{html.escape(text)}</text>')
    svg.append("</svg>")
    return "\n".join(svg) + "\n"


def rasterizer():
    """Create a reusable cell painter for GIF frames and local image inspection."""
    from PIL import Image, ImageDraw, ImageFont

    font_path = Path("/System/Library/Fonts/Menlo.ttc")
    regular = ImageFont.truetype(str(font_path), FS, index=0)
    bold = ImageFont.truetype(str(font_path), FS, index=1)
    width, height = int(COLS * CW + PAD * 2), ROWS * LH + PAD * 2

    @lru_cache(maxsize=8192)
    def tile(text, fg, bg, is_bold, underline, cell_width):
        image = Image.new("RGB", (cell_width, LH), bg)
        draw = ImageDraw.Draw(image)
        rectangles = block_rects(text)
        dots = braille_dots(text)
        if rectangles is not None:
            for left, top, right, bottom in rectangles:
                draw.rectangle((
                    round(left * cell_width), round(top * LH),
                    round(right * cell_width) - 1, round(bottom * LH) - 1,
                ), fill=fg)
        elif dots is not None:
            for dx, dy in dots:
                px, py = int(dx * cell_width), int(dy * LH)
                draw.ellipse((px, py, px + 1, py + 1), fill=fg)
        elif text in BOX_LINES:
            for x0, y0, x1, y1 in BOX_LINES[text]:
                draw.line((round(x0 * cell_width), round(y0 * LH),
                           round(x1 * cell_width), round(y1 * LH)), fill=fg)
        elif text == "⏺":
            draw.ellipse((1, 6, 7, 12), fill=fg)
        elif text == "⏸":
            draw.rectangle((1, 5, 3, 13), fill=fg)
            draw.rectangle((5, 5, 7, 13), fill=fg)
        elif text == "⎿":
            draw.line((2, 5, 2, 12, 7, 12), fill=fg)
        elif text == "⏵":
            draw.polygon(((1, 4), (7, 9), (1, 14)), fill=fg)
        elif text.strip():
            draw.text((0, FS + 1), text, font=bold if is_bold else regular, fill=fg, anchor="ls")
            if underline:
                draw.line((0, FS + 3, CW, FS + 3), fill=fg)
        return image

    def paint(cells):
        image = Image.new("RGB", (width, height), BG)
        for index, cell in enumerate(cells):
            row, col = divmod(index, COLS)
            fg, bg = colour(cell["fg"], FG), colour(cell["bg"], BG)
            if cell["reverse"]:
                fg, bg = bg, fg
            if cell["dim"]:
                fg = "#%02x%02x%02x" % tuple(
                    (int(fg[n:n+2], 16) + int(bg[n:n+2], 16)) // 2 for n in (1, 3, 5)
                )
            left, right = round(col * CW), round((col + 1) * CW)
            image.paste(
                tile(cell["text"], fg, bg, cell["bold"], cell["underline"], right - left),
                (PAD + left, PAD + row * LH),
            )
        return image
    return paint


def render_gif(root):
    """Rasterize recorded cells, keeping terminal colours stable across frames."""
    from PIL import Image

    paint = rasterizer()
    frames, durations = [], []
    for path in sorted((root / "frames").glob("*.json")):
        frame = json.loads(path.read_text())
        text = "".join(cell["text"] for cell in frame["cells"])
        for forbidden in ("Connection refused", "Retrying in", "no longer available", "/private/",
                          "/Users/", "cones-readme-", "~/personal"):
            assert forbidden not in text, f"{path.name}: unwanted capture content: {forbidden}"
        frames.append(paint(frame["cells"]))
        durations.append(frame["duration_ms"])
    assert len(frames) > 1, "capture produced no animation"
    # Sample every frame for one palette, avoiding colour flicker and large full-frame diffs.
    width = frames[0].width
    samples = Image.new("RGB", (width, 48 * len(frames)))
    for i, frame in enumerate(frames):
        samples.paste(frame.resize((width, 48), Image.Resampling.NEAREST), (0, i * 48))
    palette = samples.quantize(colors=256)
    indexed = [frame.quantize(palette=palette, dither=Image.Dither.NONE) for frame in frames]
    output = REPO / "assets/tui.gif"
    indexed[0].save(
        output, save_all=True, append_images=indexed[1:], duration=durations,
        loop=0, disposal=1, optimize=True,
    )
    with Image.open(output) as gif:
        assert gif.n_frames > 1 and gif.info["loop"] == 0
    print(f"assets/tui.gif: {sum(durations) / 1000:.1f}s, {output.stat().st_size / 1024:.0f} KiB")


def run_capture(command, env):
    """Own the capture process group and reap it if generation is interrupted."""
    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    signals = (signal.SIGTERM, signal.SIGHUP)
    previous = {sig: signal.signal(sig, interrupted) for sig in signals}
    child = None
    try:
        child = subprocess.Popen(command, cwd=REPO, env=env, start_new_session=True)
        status = child.wait()
        if status:
            raise subprocess.CalledProcessError(status, command)
    finally:
        for sig in signals:
            signal.signal(sig, signal.SIG_IGN)
        try:
            if child is not None:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                child.wait(timeout=5)
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claude", type=Path, default=shutil.which("claude"))
    parser.add_argument("--codex", type=Path, default=shutil.which("codex"))
    parser.add_argument("--render-from", type=Path, help="reuse a saved capture without starting any harness")
    args = parser.parse_args()
    if args.render_from is not None:
        export(args.render_from.resolve())
        return
    for name in ("claude", "codex"):
        binary = getattr(args, name)
        if binary is None or not binary.is_file():
            parser.error(f"{name} must be installed; pass --{name} /path/to/{name}")
    original_home = Path.home()
    # Codex's Unix-domain socket path must fit macOS's short sockaddr_un limit.
    root = Path(tempfile.mkdtemp(prefix="cones-readme-", dir="/tmp")).resolve()
    print(f"Capture: {root}", flush=True)
    with provider(root) as api_url:
        env = prepare(root, args.claude.resolve(), args.codex.resolve(), api_url, COLS, ROWS, FG, BG)
        env.update({
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(original_home / ".cargo")),
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(original_home / ".rustup")),
            "PATH": os.environ["PATH"],
            "CONES_README_FIXTURE": str(root),
        })
        if target := os.environ.get("CARGO_TARGET_DIR"):
            env["CARGO_TARGET_DIR"] = str(Path(target).resolve())
        try:
            run_capture(
                [str(REPO / "scripts/check"), "test", "--lib", "tui::readme_capture::capture",
                 "--", "--ignored", "--exact", "--nocapture"],
                env,
            )
        finally:
            # Composer-created Codex threads belong to this isolated daemon.
            (root / "rolling").touch()
            subprocess.run([str(args.codex.resolve()), "app-server", "daemon", "stop"],
                           env=env, cwd=root, capture_output=True, timeout=25, check=False)
    export(root)


def export(root):
    metadata = json.loads((root / "capture.json").read_text())
    assert (metadata["cols"], metadata["rows"]) == (COLS, ROWS), "capture dimensions do not match"
    cells = json.loads((root / "cells.json").read_text())
    assert len(cells) == COLS * ROWS
    text = "\n".join(
        "".join(c["text"] for c in cells[row * COLS:(row + 1) * COLS])
        for row in range(ROWS)
    )
    for spec in metadata["viewers"]:
        title = spec["title"]
        assert title in text, f"session was clipped: {title}"
    for harness in ("claude", "codex"):
        assert harness in text, f"harness label missing: {harness}"
    assert all(value not in text for value in ("~/personal", "readme-check", "/private/", "cones-readme-"))
    (root / "dashboard.txt").write_text(text + "\n")
    (REPO / "assets/tui.svg").write_text(render(cells))
    render_gif(root)
    print(f"assets/tui.svg: {COLS}x{ROWS} terminal cells; recorded cells in {root}")


if __name__ == "__main__":
    main()
