#!/usr/bin/env python3
"""Measure the actual TUI in an isolated tmux server, or summarize a --debug log.

    python3 scripts/bench-tui.py --log ~/.cones/tui-debug.log

summarizes a `cones tui --debug` log for intermittent delays. The timings separate harness
commands, data reads, discarded snapshots, row rebuilding, a viewer's spawn to its first text
and the first draw after input or returning from a viewer. Time spent inside a viewer is
excluded from return latency.

    python3 scripts/bench-tui.py target/release/cones --check --output /tmp/cones-bench.json

measures repeated transitions through the real TUI in an isolated tmux server with fixture
harnesses: delayed deletions, failures, navigation during a command, leaving a viewer with
Ctrl+Z, and a transcript read held across the transition. It reports p50, p95 and maximum
screen latency, with separate harness acknowledgement and rendering times; --check compares
them with LIMITS below. --transcript-mb 16 adds a large transcript; --runs and --sessions
change repetition and fleet size. These are local regression budgets, not guarantees for real
harness startup or filesystem performance.

Only fixture harnesses run. Their home, registry, ledger and tmux server are temporary.
Requires tmux; uses no Python packages and spends no model tokens. Nothing is installed into
user sessions.
"""
import argparse
import errno
import json
import math
import os
from pathlib import Path
import re
import shutil
import shlex
import statistics
import subprocess
import sys
import tempfile
import time
import uuid


LIMITS = {
    "delete_feedback": 200,
    "delete_row_removal": 200,
    "navigation_during_delete": 200,
    "detach_to_frame": 250,
    "detach_to_fresh_data": 500,
    "delete_failure_to_restore": 200,
}

FAKE_CLAUDE = r'''
import json, os, signal, sys, termios, time, tty
from pathlib import Path
root = Path(os.environ["CONES_BENCH_ROOT"])
control = json.loads((root / "control.json").read_text())
registry = root / ".claude" / "sessions"

def mark(name):
    (root / (name + ".json")).write_text(json.dumps(time.monotonic()))

if sys.argv[1] == "rm":
    time.sleep(control["delay"])
    if control["fail"]:
        mark("command_done")
        print("fixture refused deletion", file=sys.stderr)
        sys.exit(1)
    for p in registry.glob("*.json"):
        if json.loads(p.read_text())["sessionId"].startswith(sys.argv[2]):
            p.unlink()
    mark("command_done")
elif sys.argv[1] in ("attach", "agents"):
    saved = termios.tcgetattr(0)
    tty.setraw(0)
    print("\x1b[?1049h\x1b[2J\x1b[HCONES_FIXTURE_VIEWER", flush=True)
    # The dashboard keeps a left viewer alive off-screen and closes it by closing its pty,
    # so end of input is the exit; a 0x1a byte still is, for a client run by hand.
    while os.read(0, 1) not in (b"", b"\x1a"):
        pass
    if control["stop_viewer"]:
        os.kill(os.getpid(), signal.SIGTSTP)
    termios.tcsetattr(0, termios.TCSANOW, saved)
    print("\x1b[?1049l", end="", flush=True)
    time.sleep(control["exit_delay"])
else:
    print("unexpected fixture command", file=sys.stderr)
    sys.exit(2)
'''


def summarize(samples):
    result = {}
    for phase, values in sorted(samples.items()):
        ordered = sorted(values)
        result[phase] = {
            "count": len(values),
            "p50_ms": round(statistics.median(values), 2),
            "p95_ms": round(ordered[math.ceil(len(values) * .95) - 1], 2),
            "max_ms": round(max(values), 2),
        }
    return result


def log_samples(path):
    samples = {}
    for phase, ms in re.findall(r"timing (\w+) ms=([\d.]+)", path.read_text()):
        samples.setdefault(phase, []).append(float(ms))
    return samples


def measure(args):
    binary = args.binary.resolve()
    if not binary.is_file():
        raise RuntimeError(f"binary not found: {binary}")
    tmux_binary = shutil.which("tmux")
    if not tmux_binary:
        raise RuntimeError("tmux is required")
    server = f"cones-bench-{os.getpid()}-{uuid.uuid4().hex[:8]}"
    samples, timings = {}, {}
    failures = []
    completed = 0
    with tempfile.TemporaryDirectory(prefix="cones-bench-") as directory:
        root = Path(directory)
        env = dict(os.environ)
        # HOME is the subprocess's actual fixture home, never the user's real home.
        env.update(HOME=str(root), CLAUDE_CONFIG_DIR=str(root / ".claude"),
                   CONES_BENCH_ROOT=str(root), TERM="xterm-256color")
        env.pop("CODEX_HOME", None)
        shim = root / ".local/bin/claude"
        shim.parent.mkdir(parents=True)
        shim.write_text(f"#!{sys.executable}\n{FAKE_CLAUDE}")
        shim.chmod(0o755)
        registry = root / ".claude/sessions"
        registry.mkdir(parents=True)
        state = root / "state"
        control = {"delay": args.delay_ms / 1000, "fail": True,
                   "next_title": "", "stop_viewer": False,
                   "exit_delay": args.exit_delay_ms / 1000}
        alpha_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        alpha = {"pid": os.getpid(), "sessionId": alpha_id, "cwd": str(root),
                 "kind": "bg", "status": "idle", "name": "Bench Alpha"}
        for n in range(1, args.sessions):
            row = dict(alpha, sessionId=f"{n + 0xb0000000:08x}-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                       name="Bench Beta" if n == 1 else f"Bench extra {n:03}")
            (registry / f"{n}.json").write_text(json.dumps(row))
        transcript = None
        if args.transcript_mb:
            project = re.sub(r"[^a-zA-Z0-9]", "-", str(root))
            transcript = root / ".claude/projects" / project / f"{alpha_id}.jsonl"
            transcript.parent.mkdir(parents=True)
            line = json.dumps({"type": "progress", "data": "x" * 1000}) + "\n"
            transcript.write_text(line * (args.transcript_mb * 1024 * 1024 // len(line)))

        def tmux(*command, check=True):
            return subprocess.run([tmux_binary, "-L", server, "-f", "/dev/null", *command],
                                  env=env, cwd=root, text=True, capture_output=True, check=check).stdout

        def screen():
            return tmux("capture-pane", "-p", "-t", "bench:0.0")

        def wait(check, timeout=5):
            deadline = time.monotonic() + timeout
            last = ""
            while time.monotonic() < deadline:
                last = screen()
                if check(last):
                    return time.monotonic()
                time.sleep(.01)
            raise RuntimeError(f"screen condition timed out:\n{last}")

        def keys(*names):
            tmux("send-keys", "-t", "bench:0.0", *names)

        def sample(phase, start, end):
            samples.setdefault(phase, []).append((end - start) * 1000)

        def configure(**changes):
            control.update(changes)
            (root / "control.json").write_text(json.dumps(control))

        def selected(text, title):
            return any("▌" in line and title in line for line in text.splitlines())

        def listed(text, title):
            return any(title in line and re.match(r"^[ ▌]+\S+\s+claude\s+", line)
                       for line in text.splitlines())

        pipe_writer = None
        try:
            for n in range(args.runs):
                (registry / "alpha.json").write_text(json.dumps(alpha))
                for name in ("command_done", "viewer_done"):
                    (root / f"{name}.json").unlink(missing_ok=True)
                configure(fail=True, next_title=f"Bench Returned {n}", stop_viewer=bool(n % 2))
                start = time.monotonic()
                tmux("new-session", "-d", "-s", "bench", "-x", "160", "-y", "40",
                     str(binary), "--jobs", str(root / "none.yaml"),
                     "--state-dir", str(state), "tui", "--debug")
                tmux("set-option", "-s", "exit-empty", "off")
                sample("startup", start, wait(lambda s: selected(s, "Bench Alpha")))
                # 160 columns draws the viewer beside the list, where enter and ctrl+z move
                # only the keys. ctrl+\ once runs full screen, so the transitions below are
                # the ones this bench has always timed; the rule leaving says it took.
                keys("C-\\")
                wait(lambda s: "│" not in s)
                # A FIFO holds a real background transcript read until after the viewer
                # returns. Its registry values were already captured, so the result is stale.
                # This exercises the race without timing guesses or production test hooks.
                if transcript:
                    transcript.rename(transcript.with_suffix(".saved"))
                pipe = root / f"read-{n}.fifo"
                os.mkfifo(pipe)
                job = root / ".claude/jobs/bench"
                job.mkdir(parents=True, exist_ok=True)
                (job / "state.json").write_text(json.dumps({"linkScanPath": str(pipe)}))
                (registry / "alpha.json").write_text(json.dumps(dict(alpha, jobId="bench")))
                deadline = time.monotonic() + 5
                while pipe_writer is None:
                    try:
                        pipe_writer = os.open(pipe, os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno != errno.ENXIO or time.monotonic() >= deadline:
                            raise
                        time.sleep(.01)
                terminal_log = root / f"terminal-{n}.raw"
                tmux("pipe-pane", "-o", "-t", "bench:0.0",
                     "cat > " + shlex.quote(str(terminal_log)))
                start = time.monotonic()
                keys("Enter")
                sample("enter_to_viewer", start,
                       wait(lambda s: "CONES_FIXTURE_VIEWER" in s))
                # The background agent's state changes independently of its viewer.
                entry = json.loads((registry / "alpha.json").read_text())
                entry.update(name=control["next_title"], status="busy")
                (registry / "alpha.tmp").write_text(json.dumps(entry))
                (registry / "alpha.tmp").replace(registry / "alpha.json")
                returned = time.monotonic()
                keys("C-z")
                frame = wait(lambda s: "Type an instruction…" in s)
                sample("detach_to_frame", returned, frame)
                (job / "state.json").unlink()
                if transcript:
                    transcript.with_suffix(".saved").rename(transcript)
                os.close(pipe_writer)
                pipe_writer = None
                fresh = wait(lambda s: listed(s, control["next_title"]))
                sample("detach_to_fresh_data", returned, fresh)
                start = time.monotonic()
                keys("C-x", "C-x")
                feedback = wait(lambda s: "deleting" in s or "delete failed:" in s)
                sample("delete_feedback", start, feedback)
                wait(lambda s: "fixture refused deletion" in s)
                if not listed(screen(), control["next_title"]):
                    raise RuntimeError("failed deletion removed its row")
                sample("delete_failure_to_restore",
                       json.loads((root / "command_done.json").read_text()), time.monotonic())
                keys("Up")
                wait(lambda s: selected(s, control["next_title"]))
                configure(fail=False)
                (root / "command_done.json").unlink(missing_ok=True)
                start = time.monotonic()
                keys("C-x", "C-x")
                sample("delete_row_removal", start, wait(lambda s: not listed(s, control["next_title"])))
                time.sleep(.02)
                start = time.monotonic()
                keys("Down", "Up")
                sample("navigation_during_delete", start,
                       wait(lambda s: selected(s, "Bench Beta")))
                wait(lambda s: "claude --resume still has it" in s)
                if not selected(screen(), "Bench Beta"):
                    raise RuntimeError("deletion moved the cursor away from its selected neighbor")
                tmux("pipe-pane", "-t", "bench:0.0")
                raw = terminal_log.read_bytes()
                if b"CONES_FIXTURE_VIEWER" not in raw:
                    raise RuntimeError("terminal capture did not contain the viewer")
                if any(s in raw for s in (b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l")):
                    raise RuntimeError("a transition exposed the original terminal")
                keys("C-c")
                tmux("kill-session", "-t", "bench", check=False)
                completed += 1
                print(f"sample {n + 1}/{args.runs}", file=sys.stderr)
        except (RuntimeError, OSError, subprocess.CalledProcessError) as error:
            failures.append(f"sample {completed + 1}: {error}")
        finally:
            if pipe_writer is not None:
                os.close(pipe_writer)
            subprocess.run([tmux_binary, "-L", server, "kill-server"],
                           capture_output=True, check=False)
            log = state / "tui-debug.log"
            if log.exists():
                timings = log_samples(log)
                if args.output:
                    args.output.with_suffix(".log").write_text(log.read_text())
    result = {"binary": str(binary), "runs": args.runs, "completed_runs": completed,
              "failures": failures, "sessions": args.sessions,
              "command_delay_ms": args.delay_ms, "transcript_mb": args.transcript_mb,
              "viewer_exit_delay_ms": args.exit_delay_ms,
              "screen_timings": summarize(samples), "debug_timings": summarize(timings)}
    errors = []
    for phase, limit in LIMITS.items():
        worst = result["screen_timings"].get(phase, {}).get("max_ms")
        if worst is None or worst > limit:
            errors.append(f"{phase}: {worst} ms, limit {limit} ms")
    result["budget_exceeded"] = errors
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, nargs="?")
    parser.add_argument("--log", type=Path, help="summarize an existing --debug log")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--sessions", type=int, default=16)
    parser.add_argument("--delay-ms", type=int, default=500)
    parser.add_argument("--exit-delay-ms", type=int, default=500)
    parser.add_argument("--transcript-mb", type=int, default=0)
    parser.add_argument("--output", type=Path, help="save JSON results and a sibling .log")
    parser.add_argument("--check", action="store_true", help="fail when a screen latency budget is exceeded")
    args = parser.parse_args()
    if args.log:
        result = summarize(log_samples(args.log))
    elif not args.binary or args.runs < 1 or args.sessions < 2 or min(args.delay_ms, args.exit_delay_ms, args.transcript_mb) < 0:
        parser.error("provide a binary, positive runs, at least two sessions and nonnegative delays/sizes")
    else:
        result = measure(args)
    text = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(text)
    print(text, end="")
    return int(bool(result.get("failures")) or
               (args.check and bool(result.get("budget_exceeded"))))


if __name__ == "__main__":
    sys.exit(main())
