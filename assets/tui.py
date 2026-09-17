#!/usr/bin/env python3
"""Generate assets/tui.svg with the dashboard renderer and a real Claude Code viewer.

Run from the checkout: python3 assets/tui.py [--claude /path/to/claude]

The fixed CAST enters App at its session-data boundary. Its normal draw method and
Viewer render every cell, without discovering this machine's agents. Claude Code
reopens a sample transcript in an isolated home, with a dummy key and a closed
loopback API endpoint. No prompt is submitted and no model call is made.
The preview's command output comes from running the example project below.
"""
import argparse
from datetime import datetime, timedelta, timezone
import html
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import uuid


REPO = Path(__file__).resolve().parent.parent
COLS, ROWS = 146, 33
BG, FG = "#0d1117", "#e6edf3"
CW, LH, PAD, FS = 8.43, 20, 16, 14

# folder, harness, state, title, reported context, reported window.
# OpenCode has no native live-state report, so its rows retain "-".
CAST = [
    ("api", "claude", "idle", "Retry failed webhooks", 28_400, 200_000),
    ("api", "codex", "active", "Deduplicate events", 61_200, 258_400),
    ("api", "claude", "blocked", "Token expiry policy", 47_600, 200_000),
    ("api", "opencode", "-", "Pagination cursors", 19_300, None),
    ("web", "codex", "active", "Settings page", 73_900, 258_400),
    ("web", "claude", "active", "Keyboard navigation", 35_100, 200_000),
    ("web", "pi", "idle", "Dark mode contrast", 22_800, None),
    ("web", "opencode", "-", "Table virtualization", 42_700, None),
    ("infra", "claude", "blocked", "Database failover", 83_200, 200_000),
    ("infra", "codex", "done", "CI cache keys", 31_500, 258_400),
    ("infra", "pi", "active", "Container health checks", 16_900, None),
]

BEFORE = '''def retry_delay(attempt):
    return 2
'''
AFTER = '''def retry_delay(attempt):
    return min(2 ** attempt, 60)
'''
TESTS = '''from retry import retry_delay

cases = [
    ("first retry waits 1 second", 0, 1),
    ("delay doubles each attempt", 3, 8),
    ("backoff is capped at 60s", 6, 60),
    ("cap holds after 10 retries", 10, 60),
]
for name, attempt, expected in cases:
    actual = retry_delay(attempt)
    assert actual == expected, f"{name}: {actual} != {expected}"
    print(f"PASS  {name}")
print("\\n4 passed")
'''


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def prepare(root, native):
    cwd = root / "projects/api"
    cwd.mkdir(parents=True)
    (cwd / "retry.py").write_text(AFTER)
    (cwd / "test_retry.py").write_text(TESTS)
    result = subprocess.run(
        ["python3", "test_retry.py"], cwd=cwd, check=True,
        capture_output=True, text=True,
    ).stdout.rstrip()
    now = datetime.now(timezone.utc)
    stamp = lambda at: at.isoformat(timespec="milliseconds").replace("+00:00", "Z")
    sessions = []
    for i, (folder, harness, state, title, context, window) in enumerate(CAST):
        directory = root / "projects" / folder
        directory.mkdir(exist_ok=True)
        sessions.append({
            "session_id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"cones-readme/{title}")),
            "harness": harness, "kind": "bg" if harness == "claude" else None,
            "cwd": str(directory), "state": state, "title": title,
            "started": stamp(now - timedelta(minutes=45 - i * 3)),
            "last_activity": stamp(now - timedelta(seconds=10 + i * 19)),
            "context_tokens": context, "context_window": window,
        })
    selected = sessions[0]["session_id"]
    config = root / ".claude"
    project = config / "projects" / re.sub(r"[^A-Za-z0-9]", "-", str(cwd))
    project.mkdir(parents=True)
    transcript = project / f"{selected}.jsonl"
    records, parent = [], None

    def message(kind, content, **extra):
        nonlocal parent
        uid = str(uuid.uuid4())
        records.append({
            "parentUuid": parent, "isSidechain": False, "userType": "external",
            "cwd": str(cwd), "sessionId": selected, "version": "2.1.274",
            "gitBranch": "main", "type": kind, "message": content, "uuid": uid,
            "timestamp": stamp(now - timedelta(seconds=40 - len(records))),
            **extra,
        })
        parent = uid

    def assistant(content):
        message("assistant", {
            "id": f"msg_{len(records)}", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-6", "content": content,
            "stop_reason": "tool_use" if content[-1]["type"] == "tool_use" else "end_turn",
            "usage": {"input_tokens": 28_400, "output_tokens": 145},
        })

    message("user", {"role": "user", "content": "Add exponential backoff to webhook retries.\nCap the delay at 60 seconds and test it."})
    assistant([{"type": "tool_use", "id": "edit_retry", "name": "Edit", "input": {
        "file_path": "retry.py", "old_string": BEFORE, "new_string": AFTER,
    }}])
    message("user", {"role": "user", "content": [{
        "type": "tool_result", "tool_use_id": "edit_retry",
        "content": f"The file {cwd / 'retry.py'} has been updated successfully.",
    }]}, toolUseResult={
        "filePath": "retry.py", "oldString": BEFORE, "newString": AFTER,
        "originalFile": BEFORE, "userModified": False, "replaceAll": False,
        "structuredPatch": [{
            "oldStart": 1, "oldLines": 2, "newStart": 1, "newLines": 2,
            "lines": [" def retry_delay(attempt):", "-    return 2", "+    return min(2 ** attempt, 60)"],
        }],
    })
    assistant([{"type": "tool_use", "id": "test_retry", "name": "Bash", "input": {
        "command": "python3 test_retry.py", "description": "Check webhook retry delays",
    }}])
    message("user", {"role": "user", "content": [{
        "type": "tool_result", "tool_use_id": "test_retry", "content": result,
    }]}, toolUseResult={"stdout": result, "stderr": "", "interrupted": False, "isImage": False})
    assistant([{"type": "text", "text": "All 4 tests passed. Retries now back off from\n1 second to a maximum of 60 seconds."}])
    transcript.write_text("\n".join(json.dumps(r) for r in records) + "\n")
    write_json(config / ".claude.json", {
        "hasCompletedOnboarding": True, "theme": "dark",
        "customApiKeyResponses": {"approved": ["fixture"], "rejected": []},
        "projects": {str(cwd): {"hasTrustDialogAccepted": True}},
    })
    write_json(config / "settings.json", {"autoUpdatesChannel": "stable"})
    env = {
        "HOME": str(root), "CLAUDE_CONFIG_DIR": str(config),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color",
        "LANG": "en_US.UTF-8", "ANTHROPIC_API_KEY": "fixture",
        "ANTHROPIC_BASE_URL": "http://127.0.0.1:9",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
    }
    write_json(root / "capture.json", {
        "cols": COLS, "rows": ROWS, "cwd": str(cwd), "sessions": sessions,
        "selected": selected,
        "command": [str(native), "--resume", str(transcript), "--model", "claude-sonnet-4-6", "--verbose"],
        "env": env,
    })
    (root / "jobs.yaml").write_text(
        "version: 3\ncolumns: [harness, state, context]\n"
        "pane:\n  at: right\n  ratio: 47\njobs: []\n"
    )
    return sessions


ANSI16 = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
]
NAMED = dict(zip(
    ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "Gray",
     "DarkGray", "LightRed", "LightGreen", "LightYellow", "LightBlue", "LightMagenta", "LightCyan", "White"],
    ANSI16,
))


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
        '<title>cones: eleven sessions across Claude Code, Codex, pi and OpenCode</title>',
        '<desc>Sample API, web and infrastructure projects with the native Claude Code terminal showing a retry fix and passing tests.</desc>',
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
        if text in ("▀", "▄", "█"):
            top, tall = (y + LH / 2 if text == "▄" else y), LH if text == "█" else LH / 2
            svg.append(f'<rect x="{x:.2f}" y="{top}" width="{CW}" height="{tall}" fill="{fg}"/>')
        else:
            style = (' font-weight="700"' if cell["bold"] else "") + (
                ' text-decoration="underline"' if cell["underline"] else "")
            svg.append(f'<text x="{x:.2f}" y="{y + FS + 1}" fill="{fg}"{style}>{html.escape(text)}</text>')
    svg.append("</svg>")
    return "\n".join(svg) + "\n"


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
    args = parser.parse_args()
    if args.claude is None or not args.claude.is_file():
        parser.error("Claude Code must be installed; pass --claude /path/to/claude")
    original_home = Path.home()
    root = Path(tempfile.mkdtemp(prefix="cones-readme-")).resolve()
    print(f"Capture: {root}", flush=True)
    sessions = prepare(root, args.claude.resolve())
    env = os.environ.copy()
    env.update({
        "HOME": str(root),
        "CARGO_HOME": os.environ.get("CARGO_HOME", str(original_home / ".cargo")),
        "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(original_home / ".rustup")),
        "CODEX_HOME": str(root / "discovery/missing-codex"),
        "PI_CODING_AGENT_DIR": str(root / "discovery/missing-pi"),
        "XDG_DATA_HOME": str(root / "discovery/missing-xdg"),
        "CONES_README_FIXTURE": str(root),
    })
    run_capture(
        [str(REPO / "scripts/check"), "test", "--lib", "tui::readme_capture::capture",
         "--", "--ignored", "--exact", "--nocapture"],
        env,
    )
    cells = json.loads((root / "cells.json").read_text())
    assert len(cells) == COLS * ROWS
    text = "\n".join(
        "".join(c["text"] for c in cells[row * COLS:(row + 1) * COLS])
        for row in range(ROWS)
    )
    for session in sessions:
        assert session["title"] in text, f"session was clipped: {session['title']}"
    for harness in ("claude", "codex", "pi", "opencode"):
        assert harness in text, f"harness label missing: {harness}"
    assert all(value not in text for value in ("~/personal", "readme-check", "/private/", "cones-readme-"))
    (root / "dashboard.txt").write_text(text + "\n")
    (REPO / "assets/tui.svg").write_text(render(cells))
    print(f"assets/tui.svg: {COLS}x{ROWS} terminal cells; native preview and dashboard saved in {root}")


if __name__ == "__main__":
    main()
