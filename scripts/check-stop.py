#!/usr/bin/env python3
"""Check `cones stop` against a real background Claude session.

Usage: python3 scripts/check-stop.py CONES CLAUDE
The native home is disposable and non-default, so the check also proves the stop
addresses the home the row was discovered in. A loopback provider answers the
session's one turn; no real credentials or external model are used.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import uuid


def reply(handler, body):
    def event(kind, **value):
        handler.wfile.write(f"event: {kind}\ndata: {json.dumps(dict(type=kind, **value))}\n\n".encode())
        handler.wfile.flush()
    event("message_start", message={
        "id": "msg_" + uuid.uuid4().hex, "type": "message", "role": "assistant",
        "model": body["model"], "content": [], "stop_reason": None,
        "usage": {"input_tokens": 12, "output_tokens": 0},
    })
    event("content_block_start", index=0, content_block={"type": "text", "text": ""})
    event("content_block_delta", index=0, delta={"type": "text_delta", "text": "NATIVE_FIXTURE_REPLY"})
    event("content_block_stop", index=0)
    event("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None}, usage={"output_tokens": 4})
    event("message_stop")


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path.endswith("count_tokens"):
            payload = json.dumps({"input_tokens": 12}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        reply(self, body)


def until(what, predicate, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


def running(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def main(binary, claude):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="cones-stop-native-") as temporary:
        root = Path(temporary).resolve()
        # Not ~/.claude: a stop that fell back to the default home would find nothing here.
        native_home, state = root / "native-home", root / "state"
        for path in [native_home, state, root / ".local/bin"]:
            path.mkdir(parents=True)
        # cones looks up the CLI on its own fixed PATH, which starts at $HOME/.local/bin.
        (root / ".local/bin/claude").symlink_to(claude)
        endpoint = f"http://127.0.0.1:{server.server_port}"
        env = {
            "HOME": str(root), "CLAUDE_CONFIG_DIR": str(native_home),
            "PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
            "TERM": "xterm-256color", "LANG": "en_US.UTF-8",
            "ANTHROPIC_BASE_URL": endpoint, "ANTHROPIC_API_KEY": "fixture",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_AUTOUPDATER": "1",
            "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        }
        (native_home / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True, "theme": "dark",
            "customApiKeyResponses": {"approved": ["fixture"], "rejected": []},
            "projects": {str(root): {"hasTrustDialogAccepted": True}},
        }))
        (native_home / "settings.json").write_text(json.dumps({"env": env}))
        jobs = root / "jobs.yaml"
        jobs.write_text("version: 4\ndefaults:\n  codex_enabled: false\n  opencode_enabled: false\n"
                        "  gemini_enabled: false\n  cursor_enabled: false\n  copilot_enabled: false\n"
                        "  amp_enabled: false\n  droid_enabled: false\n  kimi_enabled: false\njobs: []\n")

        def native(*args):
            return subprocess.run([claude, *args], cwd=root, env=env,
                                  capture_output=True, text=True, timeout=60)

        def agents():
            listing = native("agents", "--json")
            assert listing.returncode == 0, listing.stderr
            return json.loads(listing.stdout)

        def cones(*args):
            return subprocess.run([binary, "--jobs", str(jobs), "--state-dir", str(state), *args],
                                  cwd=root, env=env, capture_output=True, text=True, timeout=60)

        try:
            started = native("--bg", "--", "Reply with the fixture text.")
            assert started.returncode == 0, started.stderr
            record = until("the background session to register",
                           lambda: next((a for a in agents()
                                         if a.get("pid") and a.get("sessionId")), None))
            session_id, pid = record["sessionId"], record["pid"]
            assert record["id"] in started.stdout, started.stdout
            print(f"PASS native background launch: {session_id} on pid {pid}", flush=True)

            listing = cones("ls", "--json")
            assert listing.returncode == 0, listing.stderr
            rows = [json.loads(line) for line in listing.stdout.splitlines()]
            row = next(r["session"] for r in rows
                       if r["kind"] == "session" and r["session"]["session_id"] == session_id)
            transcript = Path(row["transcript_path"])
            assert transcript.is_file(), row
            print("PASS cones ls names the stoppable id and its transcript", flush=True)

            stopped = cones("stop", session_id)
            assert stopped.returncode == 0, stopped.stderr
            assert session_id in stopped.stdout and str(native_home) in stopped.stdout, stopped.stdout
            assert until("the native session to end", lambda: not running(pid))
            print(f"PASS cones stop ended the session in {native_home}", flush=True)

            # Claude reports the stop itself: the session leaves the live listing, and the job
            # record it keeps for `attach` and `--resume` says stopped rather than disappearing.
            assert not [a for a in agents() if a["sessionId"] == session_id], "still listed as live"
            job = native_home / "jobs" / record["id"] / "state.json"
            assert job.is_file(), "the stop removed the job record; only `claude rm` may do that"
            assert json.loads(job.read_text())["state"] == "stopped", job.read_text()
            assert transcript.is_file(), "the stop discarded the conversation"
            assert "NATIVE_FIXTURE_REPLY" in transcript.read_text(), "the transcript lost its turn"
            row = next(json.loads(line)["session"] for line in cones("ls", "--json").stdout.splitlines()
                       if json.loads(line).get("session", {}).get("session_id") == session_id)
            assert row["state"] == "stopped" and row["transcript_path"] == str(transcript), row
            print("PASS the job record, the conversation and the cones row survived as stopped",
                  flush=True)

            # The installed `claude stop` is idempotent, so a repeat is the CLI's answer again and
            # must still leave the record and the conversation alone.
            again = cones("stop", session_id)
            assert again.returncode == 0, again.stderr
            assert job.is_file() and transcript.is_file(), "a repeated stop damaged the session"
            assert json.loads(job.read_text())["state"] == "stopped", job.read_text()
            print("PASS a repeated stop is the native answer and changes nothing", flush=True)

            unknown = cones("stop", str(uuid.uuid4()))
            assert unknown.returncode == 1 and "is not a live session" in unknown.stderr, unknown.stderr
            print("PASS an unknown id is refused", flush=True)
        finally:
            for agent in agents():
                native("rm", agent["id"])
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main(*(str(Path(p).resolve()) for p in sys.argv[1:3]))
