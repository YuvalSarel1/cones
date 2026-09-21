#!/usr/bin/env python3
"""Exercise Ctrl+N with installed Claude and Codex against a loopback model."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "assets"))
from tui_demo import prepare


class Provider(BaseHTTPRequestHandler):
    requests = []
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append({
            "path": self.path, "format": body.get("text", {}).get("format"),
        })
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

        def event(kind, **data):
            payload = json.dumps({"type": kind, **data})
            self.wfile.write(f"event: {kind}\ndata: {payload}\n\n".encode())
            self.wfile.flush()

        text = "Fixture title"
        if "/claude/" in self.path:
            if "Generate a short kebab-case name" in json.dumps(body):
                text = json.dumps({"name": "fixture-title"})
            event("message_start", message={
                "id": "msg_" + uuid.uuid4().hex, "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "usage": {"input_tokens": 12, "output_tokens": 0},
            })
            event("content_block_start", index=0, content_block={"type": "text", "text": ""})
            event("content_block_delta", index=0, delta={"type": "text_delta", "text": text})
            event("content_block_stop", index=0)
            event("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None},
                  usage={"output_tokens": 4})
            event("message_stop")
        else:
            schema = body.get("text", {}).get("format", {}).get("schema", {})
            if "title" in schema.get("properties", {}):
                text = json.dumps({"title": text})
            response = {
                "id": "resp_" + uuid.uuid4().hex, "object": "response",
                "created_at": int(time.time()), "status": "in_progress",
                "model": body["model"], "output": [],
            }
            item = {
                "type": "message", "id": "msg_" + uuid.uuid4().hex,
                "role": "assistant", "phase": "final_answer", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }
            event("response.created", response=response)
            event("response.output_item.added", output_index=0, item={**item, "content": []})
            event("response.content_part.added", item_id=item["id"], output_index=0,
                  content_index=0, part={"type": "output_text", "text": "", "annotations": []})
            event("response.output_text.delta", item_id=item["id"], output_index=0,
                  content_index=0, delta=text)
            event("response.output_text.done", item_id=item["id"], output_index=0,
                  content_index=0, text=text)
            event("response.output_item.done", output_index=0, item=item)
            event("response.completed", response={
                **response, "status": "completed", "output": [item],
                "usage": {"input_tokens": 12, "output_tokens": 4, "total_tokens": 16,
                          "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens_details": {"reasoning_tokens": 0}},
            })


def main():
    claude, codex = (Path(shutil.which(name)).resolve() for name in ("claude", "codex"))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    # Keep the native daemon's Unix socket path below the macOS limit.
    with tempfile.TemporaryDirectory(prefix="cn-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        env = prepare(root, claude, codex, f"http://127.0.0.1:{server.server_port}",
                      120, 32, "#cccccc", "#191a1b")
        env["OPENAI_API_KEY"] = "fixture"
        (root / "env.json").write_text(json.dumps(env))
        try:
            result = subprocess.call(
                [str(REPO / "scripts/check"), "test", "--lib", "native_rename_round_trip",
                 "--", "--ignored", "--nocapture"],
                cwd=REPO, env={**os.environ, "CONES_RENAME_FIXTURE": str(root)})
            if result:
                print("Loopback requests:", Provider.requests, flush=True)
                for log in (root / ".codex" / "log").glob("*.log"):
                    print(log.name, log.read_text(errors="replace")[-4000:], flush=True)
            return result
        finally:
            subprocess.run([str(codex), "app-server", "daemon", "stop"], env=env, cwd=root,
                           capture_output=True, timeout=25, check=False)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    sys.exit(main())
