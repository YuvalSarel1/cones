#!/usr/bin/env python3
"""Check native pi and Claude fork persistence against a loopback fixture.

Usage: python3 scripts/check-owned-harnesses.py CONES CLAUDE PI
No real credentials or external model are used. Native homes and process hosts
are disposable; all launched hosts are explicitly stopped on failure.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid


def send(stream, value):
    body = json.dumps(value).encode()
    data = struct.pack(">I", len(body)) + body
    if hasattr(stream, "sendall"):
        stream.sendall(data)
    else:
        stream.write(data)
        stream.flush()


def receive(stream):
    read = stream.recv if hasattr(stream, "recv") else stream.read
    def exact(size):
        data = b""
        while len(data) < size:
            part = read(size - len(data))
            if not part:
                raise AssertionError("host disconnected")
            data += part
        return data
    size = struct.unpack(">I", exact(4))[0]
    assert size < 8 * 1024 * 1024
    return json.loads(exact(size))


def main(binary, claude, pi):
    requests = []

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
            requests.append(body.get("model"))
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            def event(kind, **value):
                self.wfile.write(f"event: {kind}\ndata: {json.dumps(dict(type=kind, **value))}\n\n".encode())
                self.wfile.flush()
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

    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="cones-native-host-") as temporary:
        root = Path(temporary).resolve()
        native_home, pi_home, state = root / ".claude", root / ".pi", root / "state"
        for path in [native_home, pi_home, state / "terminals"]:
            path.mkdir(parents=True)
        endpoint = f"http://127.0.0.1:{server.server_port}"
        env = {
            "HOME": str(root), "CLAUDE_CONFIG_DIR": str(native_home),
            "PI_CODING_AGENT_DIR": str(pi_home), "CODEX_HOME": str(root / ".codex"),
            "PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
            "TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8",
            "ANTHROPIC_BASE_URL": endpoint, "ANTHROPIC_API_KEY": "fixture",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_AUTOUPDATER": "1",
            "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1", "PI_OFFLINE": "1",
        }
        (native_home / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True, "theme": "dark",
            "customApiKeyResponses": {"approved": ["fixture"], "rejected": []},
            "projects": {str(root): {"hasTrustDialogAccepted": True}},
        }))
        (native_home / "settings.json").write_text(json.dumps({"viewMode": "focus", "env": env}))
        (pi_home / "models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": endpoint, "api": "anthropic-messages", "apiKey": "fixture",
            "models": [{"id": "fixture", "name": "Fixture", "reasoning": False,
                        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                        "contextWindow": 200000, "maxTokens": 4096}],
        }}}))
        jobs = root / "jobs.yaml"
        jobs.write_text("version: 4\ndefaults:\n  codex_enabled: false\n  opencode_enabled: false\n"
                        "  gemini_enabled: false\n  cursor_enabled: false\n  copilot_enabled: false\n"
                        "  amp_enabled: false\n  droid_enabled: false\n  kimi_enabled: false\njobs: []\n")
        owners = []

        def launch(kind, program, args):
            id_ = str(uuid.uuid4())
            # Endpoint length must stay below macOS's sockaddr_un limit.
            endpoint_path = f"/tmp/cones-native-{id_}"
            record = {"id": id_, "socket": endpoint_path, "what": kind,
                      "session": {"session_id": f"{kind}:start:{id_}", "harness": kind,
                                  "cwd": str(root), "state": "-", "kind": "interactive"}}
            child = subprocess.Popen([binary, "__terminal-host"], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            owners.append((child, record))
            send(child.stdin, {
                "program": list(program.encode()), "args": [list(a.encode()) for a in args],
                "env": [], "cwd": str(root), "rows": 32, "cols": 120,
                "colors": {"fg": "rgb:cccc/cccc/cccc", "bg": "rgb:1919/1a1a/1b1b"},
                "shell": False, "record": record, "state": str(state),
            })
            child.stdin.close()
            assert select.select([child.stdout], [], [], 8)[0], "host startup timed out"
            response = receive(child.stdout)
            assert "Ok" in response, response
            record.update(response["Ok"])
            return record

        def attach(record):
            stream = socket.socket(socket.AF_UNIX)
            stream.settimeout(3)
            stream.connect(record["socket"])
            send(stream, {"Attach": {"version": 1, "id": record["id"]}})
            assert "Attached" in receive(stream)
            return stream

        def until(stream, text):
            deadline = time.monotonic() + 20
            output = ""
            while time.monotonic() < deadline:
                message = receive(stream)
                screen = message.get("Screen", {})
                output += bytes(screen.get("bytes", [])).decode(errors="replace")
                if text in output:
                    return
                if screen.get("exit") is not None:
                    raise AssertionError(f"native client ended: {output[-3000:]}")
            raise AssertionError(f"missing {text}: {output[-3000:]}")

        def verify(record):
            with attach(record) as stream:
                until(stream, "NATIVE_FIXTURE_REPLY")
                if record["session"]["harness"] == "claude":
                    # A fresh native fork can display inherited history before
                    # recording its own model/usage. Exercise one explicit turn.
                    send(stream, {"Input": list(b"Reply with the fixture text again.\r")})
                    until(stream, "NATIVE_FIXTURE_REPLY")
                send(stream, {"Input": list(b"UNSENT_NATIVE_DRAFT")})
                until(stream, "UNSENT_NATIVE_DRAFT")
            time.sleep(.05)
            os.kill(record["session"]["pid"], 0)
            with attach(record) as stream:
                until(stream, "UNSENT_NATIVE_DRAFT")
                send(stream, {"Resize": [35, 130]})
            listing = subprocess.check_output([binary, "--jobs", str(jobs), "--state-dir", str(state), "ls", "--json"],
                                              env=env, cwd=root, text=True)
            rows = [json.loads(line)["session"] for line in listing.splitlines() if json.loads(line)["kind"] == "session"]
            row = next(row for row in rows if row.get("pid") == record["session"]["pid"])
            assert ":start:" not in row["session_id"], row
            assert row["harness"] == record["session"]["harness"], row
            assert row.get("model") == requests[-1], row
            transcript = Path(row.get("transcript_path", ""))
            assert transcript.is_file() and transcript.resolve().is_relative_to(root), row
            assert row["cwd"] == str(root), row
            assert row["state"] in ["idle", "done", "active"], row
            print(f"PASS {row['harness']}: native identity, model, transcript, same process and unsent draft after reconnect", flush=True)
            return row

        try:
            seed = subprocess.run([claude, "-p", "Reply with the fixture text.", "--output-format", "json"],
                                  cwd=root, env=env, capture_output=True, text=True, timeout=30)
            assert seed.returncode == 0, seed.stderr
            seed_id = json.loads(seed.stdout)["session_id"]
            fork = launch("claude", claude, ["--resume", seed_id, "--fork-session"])
            row = verify(fork)
            assert row["session_id"] != seed_id, "fork reused its source identity"
            session_id = str(uuid.uuid4())
            native_pi = launch("pi", pi, ["--provider", "fixture", "--model", "fixture", "--session-id", session_id,
                                          "--offline", "--no-extensions", "--no-skills", "--", "Reply with the fixture text."])
            row = verify(native_pi)
            assert row["session_id"] == session_id, row
            print(f"PASS loopback provider handled {len(requests)} native requests; no external model", flush=True)
        finally:
            for child, record in owners:
                try:
                    with socket.socket(socket.AF_UNIX) as stream:
                        stream.settimeout(3)
                        stream.connect(record["socket"])
                        send(stream, {"Stop": {"id": record["id"]}})
                        receive(stream)
                except (OSError, AssertionError):
                    pass
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main(*(str(Path(p).resolve()) for p in sys.argv[1:4]))
