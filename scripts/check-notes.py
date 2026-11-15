#!/usr/bin/env python3
"""Check `cones comms send` against real pi and OpenCode CLIs and a loopback provider.

    python3 scripts/check-notes.py target/debug/cones /path/to/pi /path/to/opencode

Each client is launched detached by `cones launch` under a disposable HOME. The provider
holds every reply for a few seconds, so the first note arrives while a turn is running and
the second while the client is idle. No real credentials or model are used, and every host
this starts is stopped on the way out.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

REPLY = "NOTES_FIXTURE_REPLY"
HOLD = 4


def main(binary, pi, opencode):
    requests = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"])).decode()
            if self.path.endswith("count_tokens"):
                return self.reply("application/json", json.dumps({"input_tokens": 12}))
            requests.append(raw)
            time.sleep(HOLD)
            if self.path.endswith("/messages"):
                events = [
                    ("message_start", {"message": {
                        "id": "msg_" + uuid.uuid4().hex, "type": "message", "role": "assistant",
                        "model": "fixture", "content": [], "stop_reason": None,
                        "usage": {"input_tokens": 12, "output_tokens": 0}}}),
                    ("content_block_start", {"index": 0, "content_block": {"type": "text", "text": ""}}),
                    ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": REPLY}}),
                    ("content_block_stop", {"index": 0}),
                    ("message_delta", {"delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                       "usage": {"output_tokens": 4}}),
                    ("message_stop", {}),
                ]
                return self.reply("text/event-stream", "".join(
                    f"event: {kind}\ndata: {json.dumps(dict(type=kind, **value))}\n\n"
                    for kind, value in events))
            common = {"id": "chatcmpl_fixture", "created": int(time.time()), "model": "fixture",
                      "object": "chat.completion.chunk"}
            chunks = [
                {**common, "choices": [{"index": 0, "delta": {"role": "assistant", "content": REPLY},
                                        "finish_reason": None}]},
                {**common, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}},
            ]
            self.reply("text/event-stream",
                       "".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n")

        def reply(self, content_type, text):
            data = text.encode()
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{server.server_port}"
    # Short: the terminal host's socket path is bounded on macOS.
    root = Path(tempfile.mkdtemp(prefix="cones-notes-", dir="/tmp")).resolve()
    work, state, jobs = root / "work", root / "state", root / "jobs.yaml"
    for path in [work, state, root / ".pi", root / ".local/bin"]:
        path.mkdir(parents=True)
    (root / ".local/bin/pi").symlink_to(pi)
    (root / ".local/bin/opencode").symlink_to(opencode)
    (root / ".pi/models.json").write_text(json.dumps({"providers": {"fixture": {
        "baseUrl": endpoint, "api": "anthropic-messages", "apiKey": "fixture",
        "models": [{"id": "fixture", "name": "Fixture", "reasoning": False, "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 200000, "maxTokens": 4096}],
    }}}))
    jobs.write_text("version: 4\ndefaults:\n  pi_provider: fixture\n  pi_model: fixture\n"
                    "  opencode_model: fixture/fixture\n"
                    + "".join(f"  {name}_enabled: false\n" for name in
                              ("claude", "codex", "gemini", "cursor", "copilot", "amp", "droid", "kimi", "grok"))
                    + "jobs: []\n")
    env = {
        "HOME": str(root), "LANG": "en_US.UTF-8", "TERM": "xterm-256color", "COLORTERM": "truecolor",
        # pi is a Node script; nothing else on the machine's PATH is needed.
        "PATH": f"{Path(shutil.which('node')).parent}:/usr/bin:/bin:/usr/sbin:/sbin",
        "CLAUDE_CONFIG_DIR": str(root / ".claude"), "CODEX_HOME": str(root / ".codex"),
        "PI_CODING_AGENT_DIR": str(root / ".pi"), "PI_OFFLINE": "1",
        "XDG_DATA_HOME": str(root / "data"), "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "native-state"), "XDG_CACHE_HOME": str(root / "cache"),
        "OPENCODE_CONFIG_CONTENT": json.dumps({
            "enabled_providers": ["fixture"], "model": "fixture/fixture", "small_model": "fixture/fixture",
            "provider": {"fixture": {
                "npm": "@ai-sdk/openai-compatible", "name": "Local fixture",
                "options": {"baseURL": f"{endpoint}/v1", "apiKey": "fixture"},
                "models": {"fixture": {"name": "Fixture", "limit": {"context": 8192, "output": 1024}}}}},
        }),
        "OPENCODE_DISABLE_MODELS_FETCH": "1", "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
    }

    def cones(*args, check=True):
        out = subprocess.run([binary, "--jobs", str(jobs), "--state-dir", str(state), *args],
                             env=env, cwd=work, capture_output=True, text=True, timeout=60)
        assert not check or out.returncode == 0, f"cones {' '.join(args)}: {out.stderr}"
        return out

    def until(what, ready, timeout=40):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = ready()
            if value:
                return value
            time.sleep(0.1)
        raise AssertionError(f"timed out: {what}")

    def row(id_):
        rows = (json.loads(line).get("session", {})
                for line in cones("ls", "--dir", str(work), "--json").stdout.splitlines())
        return next((r for r in rows if r.get("session_id") == id_), {})

    def user_messages(id_):
        # The row is the client; its conversation is the native id the reporter gave it.
        native = row(id_).get("native_id")
        shown = native and cones("show", native, "--all", "--json", check=False)
        if not shown or shown.returncode:
            return []
        return [m["text"] for m in json.loads(shown.stdout)["messages"] if m["role"] == "user"]

    try:
        for harness in ("pi", "opencode"):
            version = subprocess.check_output(
                [str(root / ".local/bin" / harness), "--version"], env=env, text=True).strip()
            seen = len(requests)
            id_ = cones("launch", "--dir", str(work), "--harness", harness, "Reply briefly.").stdout.strip()
            until(f"{harness} asked the provider", lambda: len(requests) > seen)
            # The reply is still held, so this note meets a running turn.
            cones("comms", "--dir", str(work), "send", id_, "NOTE_WHILE_BUSY")
            assert not any("NOTE_WHILE_BUSY" in r for r in requests[seen:]), \
                "the note did not wait for the running turn"
            until(f"{harness} submitted the queued note after its turn",
                  lambda: any("NOTE_WHILE_BUSY" in r for r in requests[seen + 1:]))
            until(f"{harness} went idle", lambda: row(id_).get("state") == "idle")
            before = len(requests)
            cones("comms", "--dir", str(work), "send", id_, "NOTE_WHILE_IDLE")
            until(f"{harness} started a turn for the idle note",
                  lambda: any("NOTE_WHILE_IDLE" in r for r in requests[before:]))
            for note in ("NOTE_WHILE_BUSY", "NOTE_WHILE_IDLE"):
                until(f"{harness} saved {note} as a user message",
                      lambda: any(note in text for text in user_messages(id_)))
            assert "stopped" in cones("stop", id_).stdout
            print(f"PASS {harness} {version}: a note queued behind a running turn and a note to an idle "
                  "client each became one native user message", flush=True)
        print(f"PASS loopback provider handled {len(requests)} requests; no external model", flush=True)
    finally:
        for path in (state / "terminals").glob("*.json"):
            record = json.loads(path.read_text())
            cones("stop", record["id"], check=False)
            try:
                os.kill(record["session"]["pid"], signal.SIGKILL)
            except (KeyError, TypeError, ProcessLookupError):
                pass
        server.shutdown()
        server.server_close()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main(*(str(Path(p).resolve()) for p in sys.argv[1:4]))
