#!/usr/bin/env python3
"""Exercise cones with a real OpenCode CLI and a local OpenAI-compatible fixture.

    python3 scripts/check-opencode.py target/debug/cones /path/to/opencode

Requires tmux. Uses temporary native homes, a loopback provider and no real
credentials or model calls. Screens, diagnostics and the result stay in the
printed fixture directory. Never run this against a user's native home.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid


REPLY = "OPENCODE_E2E_REPLY"

def worker(root):
    """Own and reap the dashboard inside tmux's controlling terminal."""
    configuration = json.loads((root / "launch.json").read_text())
    child = None

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    try:
        child = subprocess.Popen(configuration["command"], cwd=root, env=configuration["env"])
        (root / "dashboard.pid").write_text(str(child.pid))
        child.wait()
    finally:
        for sig in (signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, signal.SIG_IGN)
        if child is not None:
            if child.poll() is None:
                child.kill()
            (root / "dashboard.exit").write_text(str(child.wait(timeout=5)))


class Dashboard:
    def __init__(self, root, controller):
        self.root, self.controller = root, controller
        self.pid = int((root / "dashboard.pid").read_text())

    def poll(self):
        path = self.root / "dashboard.exit"
        return int(path.read_text()) if path.exists() else None

    def kill(self):
        os.kill(self.controller, signal.SIGTERM)

    def wait(self, timeout):
        deadline = time.monotonic() + timeout
        while self.poll() is None:
            if time.monotonic() >= deadline:
                raise AssertionError("controller did not reap its dashboard")
            time.sleep(0.05)
        return self.poll()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cones", type=Path)
    parser.add_argument("opencode", type=Path)
    args = parser.parse_args()
    binary, native = args.cones.resolve(), args.opencode.resolve()
    tmux_binary = shutil.which("tmux")
    if not binary.is_file() or not native.is_file() or not tmux_binary:
        parser.error("both binaries and tmux must be available")
    root = Path(tempfile.mkdtemp(prefix="cones-opencode-e2e-")).resolve()
    print(f"Fixture: {root}", flush=True)
    requests = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append({"path": self.path, "model": body.get("model"), "stream": body.get("stream", False)})
            if self.path != "/v1/chat/completions":
                self.send_error(404)
                return
            # Keep the native busy state visible across a dashboard refresh.
            time.sleep(2)
            common = {"id": "chatcmpl_fixture", "created": int(time.time()), "model": "fixture"}
            usage = {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
            if body.get("stream"):
                chunks = [
                    {**common, "object": "chat.completion.chunk", "choices": [
                        {"index": 0, "delta": {"role": "assistant", "content": REPLY}, "finish_reason": None}]},
                    {**common, "object": "chat.completion.chunk", "choices": [
                        {"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage},
                ]
                data = ("".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks)
                        + "data: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                data = json.dumps({**common, "object": "chat.completion", "choices": [
                    {"index": 0, "message": {"role": "assistant", "content": REPLY}, "finish_reason": "stop"}],
                    "usage": usage}).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    config = {
        "enabled_providers": ["fixture"],
        "model": "fixture/fixture",
        "small_model": "fixture/fixture",
        "provider": {"fixture": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Local fixture",
            "options": {"baseURL": f"http://127.0.0.1:{provider.server_port}/v1", "apiKey": "fixture"},
            "models": {"fixture": {"name": "Fixture", "limit": {"context": 8192, "output": 1024}}},
        }},
    }
    env = {
        "HOME": str(root),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "LANG": "en_US.UTF-8",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "CLAUDE_CONFIG_DIR": str(root / ".claude"),
        "CODEX_HOME": str(root / ".codex"),
        "PI_CODING_AGENT_DIR": str(root / ".pi"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "native-state"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "OPENCODE_CONFIG_CONTENT": json.dumps(config),
        "OPENCODE_DISABLE_MODELS_FETCH": "1",
        "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
    }
    shim = root / ".local/bin/opencode"
    shim.parent.mkdir(parents=True)
    shim.symlink_to(native)
    jobs = root / "jobs.yaml"
    jobs.write_text("version: 3\nstart:\n  harness: opencode\n"
                    "columns: [state, model, last_active, cost]\njobs: []\n")
    state = root / "state"
    server = f"cones-opencode-{uuid.uuid4().hex[:10]}"
    child = None
    native_pids = set()
    checks = []

    def tmux(*arguments, check=True):
        return subprocess.run(
            [tmux_binary, "-L", server, "-f", "/dev/null", *arguments],
            check=check, capture_output=True, text=True,
        ).stdout

    def screen():
        return tmux("capture-pane", "-p", "-t", "check")

    def wait(label, predicate, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if child and child.poll() is not None:
                raise AssertionError(f"dashboard exited with {child.poll()}")
            value = predicate()
            if value:
                print(f"PASS {label}", flush=True)
                checks.append(label)
                (root / f"screen-{len(checks)}.txt").write_text(screen())
                return value
            time.sleep(0.1)
        (root / "failed-screen.txt").write_text(screen())
        raise AssertionError(f"timed out: {label}")

    def pids():
        output = subprocess.check_output(
            ["/bin/ps", "-axww", "-o", "pid=,ppid=,command="], text=True)
        found = set()
        for line in output.splitlines():
            words = line.split(None, 2)
            if len(words) == 3 and words[1] == str(child.pid):
                program = words[2].split()[0]
                if Path(program).name == "opencode":
                    found.add(int(words[0]))
        native_pids.update(found)
        return found

    def keys(*values):
        tmux("send-keys", "-t", "check", *values)

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    previous = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGTERM, signal.SIGHUP)}
    try:
        version = subprocess.check_output([str(native), "--version"], env=env, text=True).strip()
        (root / "launch.json").write_text(json.dumps({
            "command": [str(binary), "--jobs", str(jobs), "--state-dir", str(state), "--debug"],
            "env": env,
        }))
        tmux("new-session", "-d", "-s", "check", "-x", "220", "-y", "44",
             sys.executable, str(Path(__file__).resolve()), "--worker", str(root))
        controller = int(tmux("display-message", "-p", "-t", "check", "#{pane_pid}").strip())
        deadline = time.monotonic() + 10
        while not (root / "dashboard.pid").exists():
            if time.monotonic() >= deadline:
                raise AssertionError("dashboard controller did not start")
            time.sleep(0.05)
        child = Dashboard(root, controller)
        wait("dashboard ready", lambda: "folder" in screen())
        def folder_menu():
            if "← → pick" in screen().splitlines()[-1]:
                return True
            keys("Up")
            return False
        wait("folder menu selected", folder_menu)
        keys("Enter")
        wait("folder input opened", lambda: "enter add" in screen().splitlines()[-1])
        keys("-l", str(root))
        keys("Enter")
        keys("-l", "Reply briefly without using any tools.")
        wait("OpenCode composer selected", lambda: "opencode" in screen())
        keys("Enter")
        wait("native busy status shown", lambda: "1 working" in screen())
        wait("native response painted in dashboard", lambda: REPLY in screen() and bool(requests))
        initial = wait("one native OpenCode viewer", lambda: next(iter(pids())) if len(pids()) == 1 else None)
        wait("native status model activity and cost shown",
             lambda: any(REPLY in line[:110] and "idle" in line[:110]
                         and "fixture" in line[:110] and "$0" in line[:110]
                         for line in screen().splitlines()))
        keys("Enter")
        wait("empty native viewer focused", lambda: "← back" in screen())
        keys("Left")
        wait("Left returned to list", lambda: "enter" in screen().splitlines()[-1])
        keys("Enter")
        wait("same viewer reopened", lambda: "← back" in screen() and pids() == {initial})
        keys("Tab")
        wait("Tab stays in the native editor", lambda: "← back" in screen())
        keys("-l", "left draft")
        wait("draft keeps native input", lambda: "left draft" in screen() and "ctrl+z back" in screen())
        keys("Left")
        keys("-l", "X")
        wait("Left moves within a draft", lambda: "left drafXt" in screen() and "ctrl+z back" in screen())
        keys(*(["Left"] * 20))
        keys("-l", "Y")
        wait("Left at the start keeps the draft", lambda: "Yleft drafXt" in screen() and "ctrl+z back" in screen())
        keys(*(["Right"] * 20), "C-u")
        wait("cleared editor recognized", lambda: "← back" in screen())
        keys("C-p")
        wait("native menu focused", lambda: "Commands" in screen() and "ctrl+z back" in screen())
        keys("Left")
        keys("-l", "Switch")
        wait("Left stays in the native menu", lambda: "Switch" in screen() and "ctrl+z back" in screen())
        keys("Escape")
        wait("empty editor refocused", lambda: "← back" in screen())
        keys("C-z")
        wait("Ctrl+Z returned to list", lambda: "enter" in screen().splitlines()[-1])
        keys("C-x", "C-x")
        wait("native viewer stopped", lambda: not pids())
        keys("C-h")
        wait("saved transcript preview", lambda: REPLY in screen() and "history · read only" in screen())
        db = root / "data/opencode/opencode.db"
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
            ids = [row[0] for row in connection.execute("SELECT id FROM session WHERE parent_id IS NULL")]
            directories = [row[0] for row in connection.execute("SELECT directory FROM session WHERE parent_id IS NULL")]
        assert len(ids) == 1, ids
        assert directories == [str(root)], directories
        keys("Enter")
        resumed = wait("history opened a new native viewer",
                       lambda: next(iter(pids())) if len(pids()) == 1 and initial not in pids() else None)
        wait("resumed conversation painted", lambda: REPLY in screen() and "← back" in screen())
        command = subprocess.check_output(
            ["/bin/ps", "-ww", "-p", str(resumed), "-o", "command="], text=True)
        assert f"--session {ids[0]}" in command, command
        checks.append("resume uses the original native session id")
        result = {"opencode_version": version, "checks": checks, "provider": "loopback fixture",
                  "requests": requests, "session_id": ids[0]}
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2), flush=True)
    finally:
        for sig in previous:
            signal.signal(sig, signal.SIG_IGN)
        if child is not None:
            if child.poll() is None:
                pids()
                child.kill()
            child.wait(timeout=5)
        tmux("kill-server", check=False)
        provider.shutdown()
        provider.server_close()
        for pid in native_pids:
            # Closing the dashboard's PTYs should hang up all its native clients.
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                os.kill(pid, signal.SIGKILL)
                raise AssertionError(f"native viewer {pid} survived dashboard closure")
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--worker":
        worker(Path(sys.argv[2]))
    else:
        main()
