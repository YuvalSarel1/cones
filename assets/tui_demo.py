"""Disposable projects and loopback responses for the native README recording.

The CLIs execute every edit and command, write their own state, and render their
own interfaces. Only the example tasks and model responses are fixtures.
"""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import subprocess
import threading
import time
import uuid


CAST = [
    ("api", "claude", "retry", "Retry failed webhooks"),
    ("api", "codex", "events", "Deduplicate events"),
    ("api", "claude", "pagination", "Pagination cursors"),
    ("api", "claude", "rotation", "Token rotation"),
    ("web", "claude", "keyboard", "Keyboard navigation"),
    ("web", "claude", "timeout", "Session timeout policy"),
    ("web", "codex", "settings", "Settings page"),
    ("web", "claude", "virtualization", "Table virtualization"),
    ("infra", "codex", "cache", "CI cache keys"),
    ("infra", "claude", "health", "Container health checks"),
    ("infra", "claude", "failover", "Database failover"),
    ("infra", "codex", "images", "Build images"),
]

WORKING = {"retry", "events", "keyboard", "pagination", "settings", "health"}
PRELUDES = {
    "retry": "**Backoff is in place.** `retry.py` now doubles the delay on each attempt, "
             "up to a **60-second cap**.\n\nThe change is ready for the regression checks.",
    "keyboard": "**Keyboard focus now wraps.** `keyboard.py` returns to the first item "
                "at the end of the menu.\n\nThe change is ready for the regression checks.",
}
QUESTIONS = {
    "timeout": ("Timeout", "How long should an inactive session stay signed in?",
                [("15 minutes", "Require login again after a short idle period."),
                 ("60 minutes", "Keep longer sessions signed in.")]),
    "rotation": ("Rotation", "Should existing tokens remain valid during rotation?",
                 [("Keep a grace period", "Allow both keys for five minutes."),
                  ("Revoke immediately", "Require clients to fetch a new token.")]),
    "failover": ("Failover", "Should failover happen automatically?",
                 [("Automatic", "Promote the healthy replica after three failed probes."),
                  ("Manual approval", "Wait for an operator before promotion.")]),
}
FOLDERS = sorted({folder for folder, _, _, _ in CAST} | {"docs"})
TASKS = {task: (folder, harness, title) for folder, harness, task, title in CAST}

FILES = {
    "retry": ("retry.py", "def retry_delay(attempt):\n    return 2\n",
              "def retry_delay(attempt):\n    return min(2 ** attempt, 60)\n"),
    "keyboard": ("keyboard.py", "def next_item(current, count):\n    return current + 1\n",
                 "def next_item(current, count):\n    return (current + 1) % count\n"),
    "timeout": ("timeout.py", "IDLE_TIMEOUT_SECONDS = 3600\n", "IDLE_TIMEOUT_SECONDS = 900\n"),
    "pagination": ("pagination.py", "def page(items, start, size):\n    return items[:size]\n",
                   "def page(items, start, size):\n    return items[start:start + size]\n"),
    "virtualization": ("virtualization.py", "def visible_rows(total, start, limit):\n    return range(start, start + limit)\n",
                       "def visible_rows(total, start, limit):\n    return range(start, min(total, start + limit))\n"),
    "health": ("health.py", "def ready(probes):\n    return any(probes.values())\n",
               "def ready(probes):\n    return bool(probes) and all(probes.values())\n"),
}
CODEX_FILES = {
    "events": ("events.py", "def deduplicate(events):\n    return list(dict.fromkeys(events))\n"),
    "settings": ("settings.py", "def valid_settings(theme, page_size):\n    return theme in ('light', 'dark') and 10 <= page_size <= 100\n"),
    "cache": ("cache.py", "def cache_key(platform, runtime, lock):\n    return '|'.join((platform, runtime, lock))\n"),
    "images": ("images.py", "def image_ref(name, version):\n    return f'{name.lower()}:{version}'\n"),
}
CASES = {
    "retry": (
        "from retry import retry_delay",
        [
            ("first retry waits one second", "retry_delay(0) == 1"),
            ("second retry waits two seconds", "retry_delay(1) == 2"),
            ("third retry waits four seconds", "retry_delay(2) == 4"),
            ("delay doubles each attempt", "retry_delay(3) == 8"),
            ("fifth retry waits 16 seconds", "retry_delay(4) == 16"),
            ("backoff stops at 60 seconds", "retry_delay(6) == 60"),
            ("cap holds after 10 retries", "retry_delay(10) == 60"),
        ],
    ),
    "events": (
        "from events import deduplicate",
        [
            ("empty batch", "deduplicate([]) == []"),
            ("single event", "deduplicate(['a']) == ['a']"),
            ("unique events stay ordered", "deduplicate(['a', 'b']) == ['a', 'b']"),
            ("duplicate delivery is ignored", "deduplicate(['a', 'a']) == ['a']"),
            ("first occurrence wins", "deduplicate(['b', 'a', 'b']) == ['b', 'a']"),
            ("repeated batch is safe", "deduplicate(['a', 'b'] * 3) == ['a', 'b']"),
            ("interleaved duplicates", "deduplicate(['a', 'b', 'a', 'c', 'b']) == ['a', 'b', 'c']"),
        ],
    ),
    "keyboard": (
        "from keyboard import next_item",
        [
            ("moves to the next item", "next_item(0, 4) == 1"),
            ("middle items stay in order", "next_item(1, 4) == 2"),
            ("last item wraps to the first", "next_item(3, 4) == 0"),
            ("one item keeps focus", "next_item(0, 1) == 0"),
            ("two items wrap correctly", "next_item(1, 2) == 0"),
            ("large menus keep their order", "next_item(8, 10) == 9"),
            ("large menus wrap correctly", "next_item(9, 10) == 0"),
        ],
    ),
    "timeout": (
        "from timeout import IDLE_TIMEOUT_SECONDS",
        [
            ("idle timeout is 15 minutes", "IDLE_TIMEOUT_SECONDS == 15 * 60"),
            ("active session stays signed in", "899 < IDLE_TIMEOUT_SECONDS"),
            ("expired session requires login", "901 > IDLE_TIMEOUT_SECONDS"),
        ],
    ),
}
CASES.update({
    "pagination": ("from pagination import page", [
        ("first page", "page(list(range(12)), 0, 4) == [0, 1, 2, 3]"),
        ("second page", "page(list(range(12)), 4, 4) == [4, 5, 6, 7]"),
        ("last page", "page(list(range(12)), 8, 4) == [8, 9, 10, 11]"),
        ("partial page", "page(list(range(10)), 8, 4) == [8, 9]"),
        ("empty collection", "page([], 0, 4) == []"),
        ("past the end", "page([1, 2], 4, 4) == []"),
    ]),
    "virtualization": ("from virtualization import visible_rows", [
        ("first viewport", "list(visible_rows(100, 0, 3)) == [0, 1, 2]"),
        ("last viewport", "list(visible_rows(100, 98, 20)) == [98, 99]"),
        ("empty table", "list(visible_rows(0, 0, 20)) == []"),
    ]),
    "health": ("from health import ready", [
        ("all probes healthy", "ready({'db': True, 'cache': True})"),
        ("database failure", "not ready({'db': False, 'cache': True})"),
        ("cache failure", "not ready({'db': True, 'cache': False})"),
        ("both probes failed", "not ready({'db': False, 'cache': False})"),
        ("no probes configured", "not ready({})"),
        ("single healthy probe", "ready({'db': True})"),
    ]),
    "settings": ("from settings import valid_settings", [
        ("dark theme", "valid_settings('dark', 20)"),
        ("light theme", "valid_settings('light', 20)"),
        ("unknown theme", "not valid_settings('blue', 20)"),
        ("minimum page size", "valid_settings('dark', 10)"),
        ("maximum page size", "valid_settings('dark', 100)"),
        ("oversized page", "not valid_settings('dark', 101)"),
    ]),
    "cache": ("from cache import cache_key", [
        ("repeatable key", "cache_key('mac', '22', 'abc') == cache_key('mac', '22', 'abc')"),
        ("lockfile invalidation", "cache_key('mac', '22', 'abc') != cache_key('mac', '22', 'def')"),
        ("runtime invalidation", "cache_key('mac', '22', 'abc') != cache_key('mac', '24', 'abc')"),
    ]),
    "images": ("from images import image_ref", [
        ("versioned tag", "image_ref('api', '1.2.0') == 'api:1.2.0'"),
        ("lowercase names", "image_ref('API', '1.2.0') == 'api:1.2.0'"),
        ("preview tag", "image_ref('web', 'preview-42') == 'web:preview-42'"),
    ]),
})

INTRO = {
    "retry": "**Retry policy:** double the delay on each attempt, with a **60-second cap**.\n\nI'll update `retry.py` and check the edge cases.",
    "keyboard": "**Keyboard focus:** wrap from the last item to the first.\n\nI'll update `keyboard.py` and check short and long menus.",
    "timeout": "**15-minute timeout.** I'll update `timeout.py` and verify expiry and reauthentication.",
    "pagination": "I'll make `pagination.py` respect the cursor offset and check partial pages.",
    "virtualization": "I'll clamp `visible_rows()` to the table bounds in `virtualization.py`.",
    "health": "Readiness should require **every probe** to pass. I'll update `health.py` and check partial failures.",
}


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.{uuid.uuid4().hex}.tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def prepare(root, claude, codex, api_url, cols, rows, foreground, background):
    native_bin = root / ".local/bin"
    native_bin.mkdir(parents=True)
    for name, binary in (("claude", claude), ("codex", codex)):
        (native_bin / name).symlink_to(binary)
    for folder in FOLDERS:
        (root / "projects" / folder).mkdir(parents=True)
    for folder, _, task, _ in CAST:
        cwd = root / "projects" / folder
        if task not in CASES:
            continue
        if task in FILES:
            filename, before, _ = FILES[task]
            (cwd / filename).write_text(before)
        else:
            filename, source = CODEX_FILES[task]
            (cwd / filename).write_text(source)
        imports, cases = CASES[task]
        # The delay lets the recording browse several concurrent, real commands.
        (cwd / f"test_{task}.py").write_text(
            f"{imports}\nfrom pathlib import Path\nimport time\n"
            f"root = Path({str(root)!r})\n"
            f"cases = {cases!r}\n"
            "for i, (name, expression) in enumerate(cases):\n"
            "    assert eval(expression), name\n"
            '    print(f"PASS  {name}", flush=True)\n'
            f"    if i == 1 and {task in WORKING!r}:\n"
            f"        Path('.ready-{task}').touch()\n"
            "        while not (root / 'rolling').exists():\n"
            "            time.sleep(0.1)\n"
            f"    time.sleep({6.0 if task in {'pagination', 'settings', 'health'} else 3.0 if task in WORKING else 1.8 if task == 'timeout' else 0.08})\n"
            f'print("\\n{len(cases)} passed", flush=True)\n'
            f"Path('.done-{task}').touch()\n"
        )
    docs = root / "projects/docs"
    (docs / "write_guide.py").write_text(
        "from pathlib import Path\nimport time\n"
        'print("Reading example commands...", flush=True)\ntime.sleep(1)\n'
        'print("Writing the installation steps...", flush=True)\ntime.sleep(1)\n'
        'print("Adding a first-session example...", flush=True)\ntime.sleep(1)\n'
        'Path("QUICKSTART.md").write_text("# Quick start\\n\\nRun `cones`, add a folder, '
        'and type an instruction to start a session.\\n")\n'
        'print("Created QUICKSTART.md", flush=True)\n'
    )
    native_home = root / ".claude"
    native_home.mkdir()
    write_json(native_home / ".claude.json", {
        "hasCompletedOnboarding": True, "theme": "dark",
        "customApiKeyResponses": {"approved": ["fixture"], "rejected": []},
        "projects": {
            str(root / "projects" / folder): {"hasTrustDialogAccepted": True}
            for folder in FOLDERS
        },
    })
    write_json(native_home / "settings.json", {
        "autoUpdatesChannel": "stable",
        "verbose": False,
        "viewMode": "focus",
        "env": {
            "ANTHROPIC_API_KEY": "fixture",
            "ANTHROPIC_BASE_URL": f"{api_url}/claude/retry",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "CLAUDE_CODE_NO_FLICKER": "1",
            "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        },
        "permissions": {
            "allow": [f"Bash(python3 -u test_{task}.py)" for _, _, task, _ in CAST],
        },
    })
    codex_home = root / ".codex"
    codex_home.mkdir()
    # Native daemon management resolves its binary through this installation path.
    standalone = codex_home / "packages/standalone"
    standalone.mkdir(parents=True)
    (standalone / "current").symlink_to(codex.parent, target_is_directory=True)
    (codex_home / "config.toml").write_text(
        'model_provider = "demo"\napproval_policy = "never"\nsandbox_mode = "workspace-write"\n'
        'check_for_update_on_startup = false\n'
        '[model_providers.demo]\nname = "OpenAI"\nwire_api = "responses"\n'
        f'base_url = "{api_url}/codex/v1"\nsupports_websockets = false\n'
        + "".join(
            f'[projects.{json.dumps(str(root / "projects" / folder))}]\ntrust_level = "trusted"\n'
            for folder in FOLDERS
        )
    )
    env = {
        "HOME": str(root), "CLAUDE_CONFIG_DIR": str(native_home), "CODEX_HOME": str(codex_home),
        "PATH": f"{native_bin}:/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color",
        "COLORTERM": "truecolor", "FORCE_COLOR": "3",
        "LANG": "en_US.UTF-8", "SHELL": "/bin/bash",
        "ANTHROPIC_API_KEY": "fixture", "ANTHROPIC_BASE_URL": f"{api_url}/claude/timeout",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "CLAUDE_CODE_NO_FLICKER": "1",
        "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        "PI_CODING_AGENT_DIR": str(root / "missing-pi"),
        "XDG_DATA_HOME": str(root / "missing-xdg"),
    }
    daemon = subprocess.run(
        [str(codex), "app-server", "daemon", "start"], env=env, cwd=root / "projects/api",
        capture_output=True, text=True,
    )
    if daemon.returncode:
        raise RuntimeError(f"starting the demo Codex daemon: {daemon.stderr.strip()}")
    address = next(
        json.loads(line)["socketPath"] for line in daemon.stdout.splitlines()
        if line.startswith("{") and "socketPath" in json.loads(line)
    )
    viewers = []
    for folder, harness, task, title in CAST:
        session = str(uuid.uuid5(uuid.NAMESPACE_URL, f"cones-readme/{task}"))
        command = (
            [str(claude), *(["--bare"] if task not in QUESTIONS else []),
             "--session-id", session, "--name", title,
             "--permission-mode", "acceptEdits", "--model", "claude-sonnet-4-6", title]
            if harness == "claude" else [
                str(codex), "--no-alt-screen", "--remote", f"unix://{address}",
                "-C", str(root / "projects" / folder), title,
            ]
        )
        viewers.append({
            "session": session, "task": task, "title": title, "harness": harness,
            "initial_state": "active" if task in WORKING else "blocked" if task in QUESTIONS
            else "done" if harness == "codex" else "idle",
            "cwd": str(root / "projects" / folder), "command": command,
            "env": {**env, "ANTHROPIC_BASE_URL": f"{api_url}/claude/{task}"},
        })
    write_json(root / "capture.json", {
        "cols": cols, "rows": rows, "cwd": str(root / "projects/api"), "viewers": viewers,
        "colors": {
            name: "rgb:" + "/".join(value[i:i + 2] * 2 for i in (1, 3, 5))
            for name, value in (("fg", foreground), ("bg", background))
        },
    })
    (root / "jobs.yaml").write_text(
        "version: 3\ncolumns: [harness, state, context]\n"
        "defaults:\n  gemini_enabled: false\n  cursor_enabled: false\n"
        "  copilot_enabled: false\n  amp_enabled: false\n  droid_enabled: false\n  kimi_enabled: false\n"
        "pane:\n  at: right\n  ratio: 47\njobs: []\n"
    )
    return env


@contextmanager
def provider(root):
    """Supply deterministic responses using each CLI's normal provider protocol."""
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def event(self, kind, **data):
            data = {"type": kind, **data}
            self.wfile.write(f"event: {kind}\ndata: {json.dumps(data)}\n\n".encode())
            self.wfile.flush()

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            write_json(root / "requests" / f"{time.time_ns()}.json", {"path": self.path, "body": body})
            try:
                if "/claude/" in self.path:
                    self.claude(body)
                elif self.path.endswith("/responses"):
                    self.codex(body)
                else:
                    self.send_error(404)
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as error:
                (root / "provider-error.txt").write_text(repr(error))
                self.send_error(400, "Invalid demo response; see provider-error.txt")

        def claude(self, body):
            if self.path.endswith("/count_tokens"):
                data = json.dumps({"input_tokens": 2048}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            history = json.dumps(body.get("messages", []))
            task = next(
                (key for _, harness, key, title in CAST if harness == "claude" and title in history),
                self.path.split("/")[2],
            )
            tools = {tool["name"] for tool in body.get("tools", [])}
            question_id, edit_id, test_id = (f"demo_{task}_{step}" for step in ("question", "edit", "test"))
            results = {
                block["tool_use_id"]: block for message in body.get("messages", [])
                for block in message.get("content", []) if isinstance(block, dict)
                and block.get("type") == "tool_result"
            }
            if task == "timeout" and question_id in results:
                assert "15 minutes" in json.dumps(results[question_id]), "expected the 15-minute answer"
            content = []
            if task in QUESTIONS and question_id not in results:
                assert "AskUserQuestion" in tools, tools
                header, question, options = QUESTIONS[task]
                content = [
                    {"type": "text", "text": f"**One decision before I change this policy.** {question}"},
                    {"type": "tool_use", "id": question_id, "name": "AskUserQuestion", "input": {
                        "questions": [{
                            "question": question,
                            "header": header,
                            "options": [
                                {"label": label, "description": description}
                                for label, description in options
                            ], "multiSelect": False,
                        }],
                    }},
                ]
            elif task not in FILES:
                content = [{"type": "text", "text": "I'll leave the current policy unchanged."}]
            elif edit_id not in results:
                filename, before, after = FILES[task]
                content = [
                    {"type": "text", "text": INTRO[task]},
                    {"type": "tool_use", "id": edit_id, "name": "Edit", "input": {
                        "file_path": filename, "old_string": before, "new_string": after,
                    }},
                ]
            elif task in PRELUDES and "Run the regression checks." not in history:
                content = [{"type": "text", "text": PRELUDES[task]}]
            elif test_id not in results:
                content = [
                    {"type": "text", "text": f"**Change applied** in `{FILES[task][0]}`. "
                     f"Running the **{len(CASES[task][1])} checks** in `test_{task}.py` now."},
                    {"type": "tool_use", "id": test_id, "name": "Bash", "input": {
                        "command": f"python3 -u test_{task}.py",
                        "description": f"Check {TASKS[task][2].lower()}",
                    }},
                ]
            else:
                folder = TASKS[task][0]
                assert (root / "projects" / folder / f".done-{task}").is_file(), "Claude tests did not pass"
                content = [{"type": "text", "text": f"**All {len(CASES[task][1])} checks passed.** "
                            f"The change in `{FILES[task][0]}` is ready for review."}]
            stop = "tool_use" if content[-1]["type"] == "tool_use" else "end_turn"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.event("message_start", message={
                "id": f"msg_{uuid.uuid4().hex}", "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "usage": {"input_tokens": 2048, "output_tokens": 32},
            })
            for i, block in enumerate(content):
                if block["type"] == "text":
                    self.event("content_block_start", index=i, content_block={"type": "text", "text": ""})
                    for part in re.findall(r"\S+\s*", block["text"]):
                        self.event("content_block_delta", index=i, delta={"type": "text_delta", "text": part})
                        time.sleep(0.018)
                else:
                    self.event("content_block_start", index=i, content_block={**block, "input": {}})
                    self.event("content_block_delta", index=i,
                               delta={"type": "input_json_delta", "partial_json": json.dumps(block["input"])})
                self.event("content_block_stop", index=i)
            self.event("message_delta", delta={"stop_reason": stop, "stop_sequence": None},
                       usage={"output_tokens": 32})
            self.event("message_stop")

        def codex(self, body):
            history = json.dumps(body.get("input", []))
            docs = "Write a quick-start guide" in history
            task = "docs" if docs else next(
                (key for _, harness, key, title in CAST if harness == "codex" and title in history),
                "events",
            )
            title = "Write a quick-start guide" if docs else TASKS[task][2]
            metadata = body.get("client_metadata", {})
            turn = json.loads(metadata.get("x-codex-turn-metadata", "{}"))
            title_request = (
                "Generate a concise, single-line task title" in history
                or turn.get("thread_source") == "system"
            )
            if not title_request:
                write_json(root / "threads" / f"{task}.json", {"id": metadata["thread_id"]})
            call = f"demo_{task}"
            outputs = {
                item["call_id"]: item for item in body.get("input", [])
                if item.get("type") in ("function_call_output", "custom_tool_call_output")
            }
            inspected = f"{call}_read" in outputs
            results = [value for key, value in outputs.items()
                       if key == call or key.startswith(f"{call}_wait_")]
            latest = results[-1].get("output", "") if results else ""
            latest = latest if isinstance(latest, str) else "\n".join(part.get("text", "") for part in latest)
            pending = re.search(r"Script running with cell ID (\S+)", latest)
            complete = bool(results) and pending is None
            definitions = list(body.get("tools", []))
            for value in body.get("input", []):
                if value.get("type") == "additional_tools":
                    definitions.extend(value.get("tools", []))
            tool_names = {tool.get("name") for tool in definitions}
            output = []
            if pending:
                output.append({
                    "type": "function_call", "id": f"fc_{uuid.uuid4().hex}",
                    "call_id": f"{call}_wait_{len(outputs)}", "name": "wait", "namespace": "functions",
                    "arguments": json.dumps({"cell_id": pending[1], "yield_time_ms": 10000, "max_tokens": 2000}),
                })
            else:
                if title_request:
                    text = title
                elif complete:
                    completed_path = root / "projects" / (
                        "docs/QUICKSTART.md" if docs else f"{TASKS[task][0]}/.done-{task}")
                    assert completed_path.is_file(), f"Codex command failed: {latest[-1500:]}"
                    text = ("Created QUICKSTART.md with installation steps and a first-session example."
                            if docs else f"All {len(CASES[task][1])} checks passed. `{CODEX_FILES[task][0]}` is ready for review.")
                elif docs:
                    text = "I'll write a short guide with installation steps and a first-session example."
                elif inspected:
                    text = ("I'll check event ordering, duplicate deliveries and repeated batches."
                            if task == "events" else f"I'll run the checks for **{title.lower()}** now.")
                else:
                    text = f"I'll read `{CODEX_FILES[task][0]}` before checking **{title.lower()}**."
                output.append({
                    "type": "message", "id": f"msg_{uuid.uuid4().hex}", "role": "assistant",
                    "phase": "final_answer" if complete or title_request else "commentary", "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                })
                if not complete and not title_request:
                    arguments = {
                        "cmd": "python3 write_guide.py" if docs else f"python3 -u test_{task}.py"
                        if inspected else f"cat {CODEX_FILES[task][0]}",
                        "yield_time_ms": 30000, "max_output_tokens": 2000,
                    }
                    if not docs and not inspected:
                        call = f"{call}_read"
                    if "functions" in tool_names:
                        output.append({
                            "type": "custom_tool_call", "id": f"ct_{uuid.uuid4().hex}",
                            "call_id": call, "name": "exec", "namespace": "functions",
                            "input": f"let r = await tools.exec_command({json.dumps(arguments)}); text(r.output);"
                            " while (r.session_id && r.exit_code === undefined) {"
                            " r = await tools.write_stdin({session_id: r.session_id, chars: '',"
                            " yield_time_ms: 1000, max_output_tokens: 2000}); text(r.output); }",
                        })
                    else:
                        assert "exec_command" in tool_names, tool_names
                        output.append({
                            "type": "function_call", "id": f"fc_{uuid.uuid4().hex}", "call_id": call,
                            "name": "exec_command", "arguments": json.dumps(arguments),
                        })
            response = {
                "id": f"resp_{uuid.uuid4().hex}", "object": "response",
                "created_at": int(time.time()), "status": "in_progress",
                "model": body["model"], "output": [],
            }
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            seq = 0

            def event(kind, **data):
                nonlocal seq
                self.event(kind, sequence_number=seq, **data)
                seq += 1

            event("response.created", response=response)
            for i, value in enumerate(output):
                event("response.output_item.added", output_index=i,
                      item={**value, "content": []} if value["type"] == "message" else value)
                if value["type"] == "message":
                    event("response.content_part.added", item_id=value["id"], output_index=i,
                          content_index=0, part={"type": "output_text", "text": "", "annotations": []})
                    for part in re.findall(r"\S+\s*", text):
                        event("response.output_text.delta", item_id=value["id"], output_index=i,
                              content_index=0, delta=part)
                        time.sleep(0.018)
                    event("response.output_text.done", item_id=value["id"], output_index=i,
                          content_index=0, text=text)
                event("response.output_item.done", output_index=i, item=value)
            event("response.completed", response={
                **response, "status": "completed", "output": output,
                "usage": {"input_tokens": 2048, "output_tokens": 32, "total_tokens": 2080,
                          "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens_details": {"reasoning_tokens": 0}},
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
