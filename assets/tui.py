#!/usr/bin/env python3
"""Generate assets/tui.gif and tui.svg using cones and the installed native CLIs.

Run: python3 assets/tui.py [--claude /path/to/claude] [--codex /path/to/codex]
Re-render saved cells without starting CLIs: python3 assets/tui.py --render-from /path/to/capture
Requires Pillow and CairoSVG (python3 -m pip install Pillow CairoSVG).
Use --output-dir to render a local preview; --video also exports MP4, desktop/mobile
playback versions and a poster, and requires FFmpeg.

Native CLIs execute the example tasks in isolated homes against a loopback provider.
Cones reads their real state and renders their live terminals. The recording browses
five sessions in the list, opens a native pane, types a follow-up that runs regression
checks, then adds a folder and starts a Codex session. Short captions come from
tui_captions.json. Claude uses its native focus view with compact edit counts.
No external model is called. See tui_demo.py for the sample tasks.
The cost column uses ~/.cones/prices.json, cached by opening cones.
"""
import argparse
import html
from functools import lru_cache
from itertools import groupby
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
from tui_demo import CAST, MODELS, prepare, provider


REPO = Path(__file__).resolve().parent.parent
COLS, ROWS = 80, 24
# Terminal background, text and muted colours from the owner's Claude Code reference.
BG, FG = "#191a1b", "#cccccc"
# Square half-block pixels keep terminal sprites at their native proportions.
CW, LH, PAD, VPAD, FS = 13, 26, 20, 12, 21
CAPTION_HEIGHT = 72
RASTER_SCALE = 2
CAPTIONS = json.loads((REPO / "assets/tui_captions.json").read_text())
# The README art plays at its own speed; five seconds shows it without a long tail.
CLOSING_MS = 5000

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
    width, height = int(COLS * CW + PAD * 2), ROWS * LH + VPAD * 2
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="SFMono-Regular,Menlo,Consolas,monospace" font-size="{FS}" shape-rendering="crispEdges">',
        '<title>cones: running Claude Code and Codex sessions</title>',
        '<desc>Sample API and web projects with native terminals running tests and waiting for input.</desc>',
        f'<rect width="{width}" height="{height}" rx="8" fill="{BG}" shape-rendering="auto"/>',
    ]
    # Every cell and style comes from ratatui, including spaces in Claude's diff.
    for index, cell in enumerate(cells):
        row, col = divmod(index, COLS)
        x, y = PAD + col * CW, VPAD + row * LH
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


def rasterizer(scale=RASTER_SCALE):
    """Create a reusable cell painter for GIF frames and local image inspection."""
    from PIL import Image, ImageDraw, ImageFont

    font_path = Path("/System/Library/Fonts/Menlo.ttc")
    regular = ImageFont.truetype(str(font_path), FS * scale, index=0)
    bold = ImageFont.truetype(str(font_path), FS * scale, index=1)
    width, height = int(COLS * CW + PAD * 2), ROWS * LH + VPAD * 2

    def scaled(*values):
        return tuple(round(value * scale) for value in values)

    @lru_cache(maxsize=8192)
    def tile(text, fg, bg, is_bold, underline, cell_width):
        image = Image.new("RGB", (cell_width * scale, LH * scale), bg)
        draw = ImageDraw.Draw(image)
        rectangles = block_rects(text)
        dots = braille_dots(text)
        if rectangles is not None:
            for left, top, right, bottom in rectangles:
                draw.rectangle((
                    round(left * cell_width * scale), round(top * LH * scale),
                    round(right * cell_width * scale) - 1, round(bottom * LH * scale) - 1,
                ), fill=fg)
        elif dots is not None:
            for dx, dy in dots:
                px, py = int(dx * cell_width), int(dy * LH)
                draw.ellipse(scaled(px, py, px + 1, py + 1), fill=fg)
        elif text in BOX_LINES:
            for x0, y0, x1, y1 in BOX_LINES[text]:
                draw.line(scaled(x0 * cell_width, y0 * LH, x1 * cell_width, y1 * LH),
                          fill=fg, width=scale)
        elif text == "⏺":
            draw.ellipse(scaled(1, 6, 7, 12), fill=fg)
        elif text == "⏸":
            draw.rectangle(scaled(1, 5, 3, 13), fill=fg)
            draw.rectangle(scaled(5, 5, 7, 13), fill=fg)
        elif text == "⎿":
            draw.line(scaled(2, 5, 2, 12, 7, 12), fill=fg, width=scale)
        elif text == "⏵":
            draw.polygon([scaled(1, 4), scaled(7, 9), scaled(1, 14)], fill=fg)
        elif text.strip():
            draw.text(scaled(0, FS + 1), text, font=bold if is_bold else regular,
                      fill=fg, anchor="ls")
            if underline:
                draw.line(scaled(0, FS + 3, CW, FS + 3), fill=fg, width=scale)
        return image

    def paint(cells):
        image = Image.new("RGB", (width * scale, height * scale), BG)
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
                scaled(PAD + left, VPAD + row * LH),
            )
        return image
    return paint


def recorded_frames(root):
    """Validate the recorded interaction before rendering any assets."""
    frames = []
    elapsed, scene_started, previous_scene = 0, 0, None
    for path in sorted((root / "frames").glob("*.json")):
        frame = json.loads(path.read_text())
        assert len(frame["cells"]) == COLS * ROWS, path.name
        if frame["scene"] != previous_scene:
            previous_scene, scene_started = frame["scene"], elapsed
        frame["scene_elapsed_ms"] = elapsed - scene_started
        elapsed += frame["duration_ms"]
        text = "".join(cell["text"] for cell in frame["cells"])
        for forbidden in ("Connection refused", "Retrying in", "Reconnecting...",
                          "WebSocket protocol", "Stream disconnected", "no longer available", "/private/",
                          "/Users/", "cones-readme-", "~/personal", "unread", "✉"):
            assert forbidden not in text, f"{path.name}: unwanted capture content: {forbidden}"
        assert frame["pane_visible"] == (frame["scene"] != "list"), path.name
        expected = {5, 6} if frame["scene"] == "launch" else {5}
        assert len(frame["sessions"]) in expected, path.name
        if frame["scene"] == "typing":
            assert frame["pane_focused"], "typing must reach the native agent"
        frames.append(frame)
    assert len(frames) > 1, "capture produced no animation"
    assert frames[0]["scene"] == "list" and len(frames[-1]["sessions"]) == 6
    opening = "".join(cell["text"] for cell in frames[0]["cells"])
    assert all(label in opening for label in ("context", "model", "activity", "cost")), (
        "the opening must show context, model, activity and cost columns"
    )
    lines = [
        "".join(cell["text"] for cell in frames[0]["cells"][row * COLS:(row + 1) * COLS])
        for row in range(ROWS)
    ]
    header = next(line for line in lines if "context" in line and "model" in line)
    expected_models = {title: MODELS[task].removesuffix("[1m]") for _, _, task, title in CAST}
    for session in frames[0]["sessions"]:
        assert session["model"].removesuffix("[1m]") == expected_models[session["title"]], (
            f"native model does not match the selected model: {session['title']}: {session['model']}"
        )
        assert session["context_tokens"] is not None and session["context_window"] > 0, (
            f"native context report missing: {session['title']}"
        )
        row = next(row for row, line in enumerate(lines) if session["title"] in line)
        context = "".join(
            cell["text"] for cell in frames[0]["cells"][
                row * COLS + header.index("context"):row * COLS + header.index("model")
            ]
        ).strip()
        assert re.fullmatch(r"\d+(?:\.\d+)?[km]?/\d+(?:\.\d+)?[km]?", context, re.IGNORECASE), (
            f"used/total context is not visible: {session['title']}: {context!r}"
        )
        assert session["cost_usd"] is not None, f"cost missing: {session['title']}"
        cost = "".join(
            cell["text"] for cell in frames[0]["cells"][row * COLS + header.index("cost"):(row + 1) * COLS]
        ).strip()
        assert re.fullmatch(r"~?\$\d+(?:\.\d+)?", cost), f"cost is not visible: {session['title']}: {cost!r}"
    follow_up = "Run the regression checks."
    assert follow_up in (root / "native-typed-input.txt").read_text()
    assert "test_keyboard.py" in (root / "native-after-reply.txt").read_text()
    progress = set()
    for frame in frames:
        if frame["scene"] == "typing":
            text = "\n".join(
                "".join(cell["text"] for cell in frame["cells"][row * COLS + 43:(row + 1) * COLS])
                for row in range(ROWS)
            )
            progress.add(max([0] + [n for n in range(1, len(follow_up) + 1) if follow_up[:n] in text]))
    assert len(progress) >= 10 and len(follow_up) in progress, "native typing was not visibly captured"
    return frames


def caption_frames(frames):
    """Allow reading time on static views and carry captions into the next action."""
    result = []
    elapsed, caption_started, active_caption = 0, 0, None
    for scene, group in groupby(frames, key=lambda frame: frame["scene"]):
        if scene == "peek":
            continue
        group = [dict(frame) for frame in group]
        if scene == "folder":
            for _, selection in groupby(group, key=lambda frame: frame["selected"]):
                navigation_ms = 0
                for frame in selection:
                    navigation_ms += frame["duration_ms"]
                    if navigation_ms >= 600:
                        frame["row_pause_after"] = True
                        break
        spec = CAPTIONS.get(scene)
        if spec:
            active_caption, caption_started = spec, elapsed
        recorded_duration = sum(frame["duration_ms"] for frame in group)
        duration = max(recorded_duration, (spec or {}).get("minimum_scene_ms", 0))
        recorded_elapsed, scene_elapsed = 0, 0
        for frame in group:
            recorded_elapsed += frame["duration_ms"]
            next_elapsed = round(recorded_elapsed * duration / recorded_duration)
            frame_duration = next_elapsed - scene_elapsed
            result.append({
                **frame,
                "duration_ms": frame_duration,
                "scene_elapsed_ms": scene_elapsed,
                "caption": active_caption,
                "caption_elapsed_ms": elapsed - caption_started,
            })
            elapsed += frame_duration
            scene_elapsed = next_elapsed
    animated = []
    for frame in result:
        spec = frame["caption"]
        elapsed = frame["caption_elapsed_ms"]
        duration = spec["duration_ms"] if spec else 0
        fades = [(0, 250), (350, 950), (duration - 950, duration - 350),
                 (duration - 400, duration)]
        changing = spec is not None and any(
            elapsed < end and elapsed + frame["duration_ms"] > start
            for start, end in fades
        )
        # Animate the annotations smoothly without changing the native recording's timing.
        step = 50 if changing else frame["duration_ms"]
        for offset in range(0, frame["duration_ms"], step):
            animated.append({
                **frame,
                "duration_ms": min(step, frame["duration_ms"] - offset),
                "scene_elapsed_ms": frame["scene_elapsed_ms"] + offset,
                "caption_elapsed_ms": elapsed + offset,
                "row_pause_after": frame.get("row_pause_after", False)
                and offset + min(step, frame["duration_ms"] - offset) == frame["duration_ms"],
            })
    paced = []
    for scene, group in groupby(animated, key=lambda frame: frame["scene"]):
        for frame in group:
            paced.append(frame)
            if frame.get("row_pause_after"):
                paced.append({**frame, "duration_ms": 1000, "row_pause_after": False})
        if scene == "list":
            last = paced[-1]
            paced.append({
                **last, "duration_ms": 2000,
                "caption_elapsed_ms": CAPTIONS["list"]["duration_ms"],
            })
    last = paced[-1]
    for offset in range(0, CLOSING_MS, 50):
        paced.append({
            **last, "scene": "closing", "caption": None,
            "closing_opacity": min(1, (offset + 50) / 300),
            "closing_time_ms": offset, "duration_ms": min(50, CLOSING_MS - offset),
        })
    return paced


def highlight_opacity(frame):
    """Give a brief cue, then let the interaction continue without an outline."""
    elapsed = frame["caption_elapsed_ms"]
    duration = frame["caption"]["duration_ms"]
    progress = max(0, min(1, (elapsed - 350) / 600, (duration - 350 - elapsed) / 600))
    return progress * progress * (3 - 2 * progress)


def highlight_box(frame):
    """Locate the caption's subject in the captured terminal cell grid."""
    lines = [
        "".join(cell["text"] for cell in frame["cells"][row * COLS:(row + 1) * COLS])
        for row in range(ROWS)
    ]
    target = frame["caption"]["highlight"]
    if target == "metadata":
        header = next(row for row, line in enumerate(lines) if "context" in line)
        assert all(label in lines[header] for label in ("model", "activity", "cost"))
        left = lines[header].index("context")
        rows = [
            row for row, line in enumerate(lines)
            if any(session["title"] in line for session in frame["sessions"])
        ]
        bottom = max(rows) + 1
        right = max(len(line.rstrip()) for line in lines[header:bottom])
        return left - .5, header - .35, right + .5, bottom + .35

    divider = next(
        col for col in range(COLS // 3, COLS * 2 // 3)
        if frame["cells"][col]["text"] == "│"
    )
    if target == "pane":
        return divider + .5, -.25, COLS + .25, ROWS + .25
    if target == "input":
        pane = [
            "".join(cell["text"] for cell in frame["cells"][row * COLS + divider + 1:(row + 1) * COLS])
            for row in range(ROWS)
        ]
        if frame["scene"] == "reply":
            rows = [
                row for row, line in enumerate(pane)
                if "Run the regression checks." in line or "test_keyboard.py" in line
            ]
            if rows:
                return divider + .5, min(rows) - .4, COLS + .25, max(rows) + 1.4
        row = max(row for row, line in enumerate(pane) if line.lstrip().startswith("❯"))
        return divider + .5, row, COLS + .25, row + 1.15

    assert target == "new_folder", target
    left = [
        "".join(cell["text"] for cell in frame["cells"][row * COLS:row * COLS + divider])
        for row in range(ROWS)
    ]
    if frame["scene"] == "folder":
        row = next((row for row, line in enumerate(left) if "+ " in line), None)
        if row is None:
            return None  # The folder row can be offscreen during navigation.
    elif frame["scene"] == "compose":
        row = next(row for row, line in enumerate(left) if "›" in line)
    else:
        row = next(row for row, line in enumerate(left) if "~/projects/docs" in line) + 1
    return -.25, row - .4, divider - .5, row + 1.4


def captioned(image, frame):
    """Keep brief, plain captions above the live terminal and its input area."""
    from PIL import Image, ImageDraw, ImageFont

    scale = image.width // (COLS * CW + PAD * 2)
    canvas = Image.new("RGB", (image.width, image.height + CAPTION_HEIGHT * scale), BG)
    canvas.paste(image, (0, CAPTION_HEIGHT * scale))
    spec = frame["caption"]
    if spec is None:
        return canvas
    elapsed = frame["caption_elapsed_ms"]
    opacity = max(0, min(1, elapsed / 250, (spec["duration_ms"] - elapsed) / 400))
    if opacity == 0:
        return canvas
    overlay = Image.new("RGBA", canvas.size)
    draw = ImageDraw.Draw(overlay)
    highlight_alpha = highlight_opacity(frame)
    if highlight_alpha > 0 and (box := highlight_box(frame)):
        x0, y0, x1, y1 = box
        draw.rounded_rectangle(
            ((PAD + x0 * CW) * scale, (CAPTION_HEIGHT + VPAD + y0 * LH) * scale,
             (PAD + x1 * CW) * scale, (CAPTION_HEIGHT + VPAD + y1 * LH) * scale),
            radius=7 * scale, outline=(128, 205, 230, round(235 * highlight_alpha)),
            fill=(80, 170, 205, round(24 * highlight_alpha)), width=4 * scale,
        )
    font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial Bold.ttf", 38 * scale)
    width = draw.textlength(spec["text"], font=font)
    assert width <= canvas.width - 32 * scale, "caption is too long for the video"
    draw.text((canvas.width // 2, 36 * scale), spec["text"], font=font,
              fill=(245, 246, 248, round(255 * opacity)), anchor="mm")
    return Image.alpha_composite(canvas.convert("RGBA"), overlay).convert("RGB")


def animation_value(animation, seconds):
    """Sample the numeric SVG animations used by the README header."""
    begin = float(animation.get("begin", "0s").removesuffix("s"))
    if seconds < begin:
        return None
    duration = float(animation.get("dur").removesuffix("s"))
    progress = ((seconds - begin) % duration) / duration
    values = [[float(v) for v in value.split()] for value in animation.get("values").split(";")]
    times = [float(t) for t in animation.get(
        "keyTimes", ";".join(str(i / (len(values) - 1)) for i in range(len(values)))
    ).split(";")]
    index = next((i for i in range(len(times) - 1) if progress < times[i + 1]), len(times) - 2)
    fraction = (progress - times[index]) / (times[index + 1] - times[index])
    if animation.get("calcMode") == "spline":
        x1, y1, x2, y2 = map(float, animation.get("keySplines").split(";")[index].split())
        def bezier(t, a, b):
            return 3 * (1 - t) ** 2 * t * a + 3 * (1 - t) * t ** 2 * b + t ** 3
        low, high = 0.0, 1.0
        for _ in range(18):
            middle = (low + high) / 2
            if bezier(middle, x1, x2) < fraction:
                low = middle
            else:
                high = middle
        fraction = bezier((low + high) / 2, y1, y2)
    return " ".join(f"{a + (b - a) * fraction:.5f}" for a, b in zip(values[index], values[index + 1]))


def closing_screen(time_ms=0):
    """Play the README art with its tagline below."""
    from io import BytesIO
    import xml.etree.ElementTree as ET
    for directory in ("/opt/homebrew/lib", "/usr/local/lib"):
        if Path(directory, "libcairo.2.dylib").exists():
            paths = os.environ.get("DYLD_FALLBACK_LIBRARY_PATH", "").split(":")
            if directory not in paths:
                os.environ["DYLD_FALLBACK_LIBRARY_PATH"] = ":".join(
                    [path for path in paths if path] + [directory]
                )
    import cairosvg
    from PIL import Image

    scale = RASTER_SCALE
    width = (COLS * CW + PAD * 2) * scale
    height = (ROWS * LH + VPAD * 2 + CAPTION_HEIGHT) * scale
    image = Image.new("RGB", (width, height), BG)
    svg = ET.fromstring((REPO / "assets/cones.svg").read_text())
    for parent in list(svg.iter()):
        for child in list(parent):
            tag = child.tag.rsplit("}", 1)[-1]
            if tag in ("animate", "animateTransform"):
                value = animation_value(child, time_ms / 1000)
                if value is not None:
                    if tag == "animateTransform":
                        parent.set("transform", f"{child.get('type')}({value})")
                    else:
                        parent.set(child.get("attributeName"), value)
                parent.remove(child)
            elif child.get("class") == "tagline":
                child.set("fill", "#9ca3af")  # the README's dark-mode color; cairo skips @media
            if child.get("font-family"):
                child.set("font-family", "Menlo")  # cairo reads only the first family
    # GitHub shows the 720px header in a column about 880px wide, beside the full-width GIF.
    artwork = Image.open(BytesIO(cairosvg.svg2png(
        bytestring=ET.tostring(svg), output_width=width * 720 // 880,
    ))).convert("RGBA")
    image.paste(artwork, ((width - artwork.width) // 2, (height - artwork.height) // 2), artwork)
    return image


def render_gif(frames, output_dir, video=False, gif=True):
    """Render one continuous recording, using one palette across the GIF."""
    from PIL import Image

    frames = caption_frames(frames)
    paint = rasterizer()
    width = (COLS * CW + PAD * 2) * RASTER_SCALE
    durations = [frame["duration_ms"] for frame in frames]
    with tempfile.TemporaryDirectory(prefix="cones-render-") as folder:
        folder = Path(folder)
        samples = Image.new("RGB", (width, 48 * len(frames))) if gif else None
        concat = ["ffconcat version 1.0"]
        poster_saved = False
        for i, frame in enumerate(frames):
            image = captioned(paint(frame["cells"]), frame)
            if frame["scene"] == "closing":
                image = Image.blend(image, closing_screen(frame["closing_time_ms"]), frame["closing_opacity"])
            image.save(folder / f"{i:05}.png")
            if samples is not None:
                samples.paste(image.resize((width, 48), Image.Resampling.NEAREST), (0, i * 48))
            concat.extend([f"file '{i:05}.png'", f"duration {durations[i] / 1000:.3f}"])
            if video and not poster_saved and frame["scene_elapsed_ms"] >= 300:
                image.save(output_dir / "tui.png")
                poster_saved = True
        output = output_dir / ("tui.gif" if gif else "tui.mp4")
        if gif:
            palette = samples.quantize(colors=256)
            indexed = []
            for i in range(len(frames)):
                with Image.open(folder / f"{i:05}.png") as image:
                    indexed.append(image.quantize(palette=palette, dither=Image.Dither.NONE))
            # Round cumulative timing, preserving the duration of native keystrokes.
            elapsed, previous, gif_durations = 0, 0, []
            for duration in durations:
                elapsed += duration
                rounded = round(elapsed / 10) * 10
                gif_durations.append(rounded - previous)
                previous = rounded
            indexed[0].save(output, save_all=True, append_images=indexed[1:],
                            duration=gif_durations, loop=0, disposal=1, optimize=True)
        if video:
            concat.append(f"file '{len(frames) - 1:05}.png'")
            (folder / "frames.txt").write_text("\n".join(concat) + "\n")
            subprocess.run([
                "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                "-f", "concat", "-safe", "0", "-i", str(folder / "frames.txt"),
                "-t", str(sum(durations) / 1000), "-vf", "fps=30",
                "-c:v", "libx264", "-preset", "veryfast" if not gif else "medium", "-crf", "12",
                "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(output_dir / "tui.mp4"),
            ], check=True)
            render_video_previews(output_dir)
    if gif:
        with Image.open(output) as encoded:
            assert encoded.n_frames > 1 and encoded.info["loop"] == 0
    print(f"{output}: {sum(durations) / 1000:.1f}s, {output.stat().st_size / 1024:.0f} KiB")


def render_video_previews(output_dir):
    """Prefilter for feed widths so browser video scaling preserves letter strokes."""
    command = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-i", str(output_dir / "tui.mp4"),
        "-filter_complex",
        "[0:v]split=2[desktop_in][mobile_in];"
        "[desktop_in]scale=1368:-2:flags=lanczos[desktop];"
        "[mobile_in]scale=768:-2:flags=lanczos[mobile]",
    ]
    for size in ("desktop", "mobile"):
        command.extend([
            "-map", f"[{size}]", "-c:v", "libx264", "-preset", "medium", "-crf", "12",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart",
            str(output_dir / f"tui-{size}.mp4"),
        ])
    subprocess.run(command, check=True)


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
    parser.add_argument("--output-dir", type=Path, default=REPO / "assets")
    parser.add_argument("--video", action="store_true", help="also export MP4 and a poster; requires FFmpeg")
    parser.add_argument("--video-only", action="store_true", help="update video previews without encoding the GIF")
    args = parser.parse_args()
    args.video = args.video or args.video_only
    if args.video and shutil.which("ffmpeg") is None:
        parser.error("--video requires ffmpeg")
    if args.render_from is not None:
        export(args.render_from.resolve(), args.output_dir.resolve(), args.video, args.video_only)
        return
    for name in ("claude", "codex"):
        binary = getattr(args, name)
        if binary is None or not binary.is_file():
            parser.error(f"{name} must be installed; pass --{name} /path/to/{name}")
    root = record(args.claude.resolve(), args.codex.resolve())
    export(root, args.output_dir.resolve(), args.video, args.video_only)


def record(claude, codex):
    """Record native interactions in a disposable fixture and return its path."""
    original_home = Path.home()
    # Codex's Unix-domain socket path must fit macOS's short sockaddr_un limit.
    root = Path(tempfile.mkdtemp(prefix="cones-readme-", dir="/tmp")).resolve()
    print(f"Capture: {root}", flush=True)
    with provider(root) as api_url:
        env = prepare(root, claude, codex, api_url, COLS, ROWS, FG, BG)
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
            (root / "recording-complete").touch()
            try:
                subprocess.run([str(codex), "app-server", "daemon", "stop"],
                               env=env, cwd=root, capture_output=True, timeout=15, check=False)
            except subprocess.TimeoutExpired:
                pass
            # A managed fixture daemon can outlive its clients. Only terminate
            # binaries installed inside this fixture, never the user's daemon.
            processes = subprocess.run(
                ["ps", "-axo", "pid=,command="], text=True, capture_output=True, check=True,
            ).stdout.splitlines()
            for line in processes:
                fields = line.strip().split(None, 1)
                if len(fields) == 2 and fields[1].startswith(str(root) + "/.codex/packages/"):
                    try:
                        os.kill(int(fields[0]), signal.SIGTERM)
                    except ProcessLookupError:
                        pass
    return root


def export(root, output_dir=None, video=False, video_only=False):
    output_dir = output_dir or REPO / "assets"
    output_dir.mkdir(parents=True, exist_ok=True)
    metadata = json.loads((root / "capture.json").read_text())
    assert (metadata["cols"], metadata["rows"]) == (COLS, ROWS), "capture dimensions do not match"
    frames = recorded_frames(root)
    cells = frames[0]["cells"]
    text = "\n".join(
        "".join(c["text"] for c in cells[row * COLS:(row + 1) * COLS])
        for row in range(ROWS)
    )
    for spec in metadata["viewers"]:
        title = spec["title"]
        assert title in text, f"session was clipped: {title}"
    sessions = json.loads((root / "sessions.json").read_text())
    assert {"claude", "codex"} <= {session["harness"] for session in sessions}
    assert all(value not in text for value in ("~/personal", "readme-check", "/private/", "cones-readme-"))
    (output_dir / "tui.svg").write_text(render(cells))
    render_gif(frames, output_dir, video, gif=not video_only)
    print(f"{output_dir / 'tui.svg'}: {COLS}x{ROWS} terminal cells; recorded cells in {root}")


if __name__ == "__main__":
    main()
