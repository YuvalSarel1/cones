#!/usr/bin/env python3
"""Task-scoped Codex delivery through the native app-server queue API."""

import argparse
import base64
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid


class DeliveryError(Exception):
    pass


def thread_id(value):
    try:
        return str(uuid.UUID(value))
    except ValueError:
        raise DeliveryError("use the exact thread UUID; names and guessed recipients are refused")


def resolve_pid(value):
    if not value.isdecimal():
        return thread_id(value)
    result = subprocess.run(
        ["ps", "-o", "command=", "-p", value], capture_output=True, text=True, check=True
    )
    ids = re.findall(r"\bresume\s+(?:--\s+)?([0-9a-fA-F-]{36})(?=\s|$)", result.stdout)
    if len(ids) != 1:
        raise DeliveryError("pid has no explicit resume UUID; obtain its thread ID from the launcher")
    return thread_id(ids[0])


class RPC:
    """Connect to the existing daemon. Never start a thread or a model turn."""

    def __enter__(self):
        result = subprocess.run(
            ["codex", "app-server", "daemon", "version"],
            capture_output=True, text=True, check=True, timeout=5,
        )
        info = json.loads(result.stdout)
        if info.get("status") != "running" or not info.get("socketPath"):
            raise DeliveryError("no running local Codex daemon")
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(5)
        self.buffer = b""
        self.sequence = 0
        try:
            self.socket.connect(info["socketPath"])
            key = base64.b64encode(os.urandom(16)).decode()
            self.socket.sendall((
                "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                f"Sec-WebSocket-Key: {key}\r\n\r\n"
            ).encode())
            while b"\r\n\r\n" not in self.buffer:
                self.receive()
                if len(self.buffer) > 16384:
                    raise DeliveryError("invalid daemon handshake")
            header, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
            ).digest()).decode()
            headers = {}
            for line in header.split(b"\r\n")[1:]:
                if b":" in line:
                    name, value = line.decode().split(":", 1)
                    headers[name.lower()] = value.strip()
            if (not header.startswith(b"HTTP/1.1 101 ")
                    or headers.get("sec-websocket-accept") != accept):
                raise DeliveryError("daemon rejected WebSocket handshake")
            self.call("initialize", {
                "clientInfo": {"name": "orchestrator", "version": "2"},
                "capabilities": {"experimentalApi": True},
            })
            self.write({"method": "initialized"})
        except Exception:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *_):
        self.socket.close()

    def frame(self, payload, opcode=1):
        mask = os.urandom(4)
        size = len(payload)
        length = (bytes([size | 128]) if size < 126 else
                  b"\xfe" + struct.pack("!H", size) if size < 65536 else
                  b"\xff" + struct.pack("!Q", size))
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.socket.sendall(bytes([128 | opcode]) + length + mask + masked)

    def write(self, message):
        self.frame(json.dumps(message).encode())

    def receive(self):
        chunk = self.socket.recv(65536)
        if not chunk:
            raise DeliveryError("daemon connection closed")
        self.buffer += chunk

    def take(self, size):
        while len(self.buffer) < size:
            self.receive()
        result, self.buffer = self.buffer[:size], self.buffer[size:]
        return result

    def read(self):
        payload = b""
        while True:
            first, second = self.take(2)
            opcode, length = first & 15, second & 127
            if second & 128 or first & 112:
                raise DeliveryError("unsupported daemon WebSocket frame")
            if length == 126:
                length = struct.unpack("!H", self.take(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self.take(8))[0]
            if len(payload) + length > 8 * 1024 * 1024:
                raise DeliveryError("daemon response exceeds 8 MiB")
            part = self.take(length)
            if opcode == 8:
                raise DeliveryError("daemon closed WebSocket")
            if opcode == 9:
                self.frame(part, opcode=10)
                continue
            if opcode == 10:
                continue
            if opcode not in (0, 1):
                raise DeliveryError("expected JSON text from daemon")
            payload += part
            if first & 128:
                return json.loads(payload)

    def call(self, method, params):
        self.sequence += 1
        request = self.sequence
        self.write({"id": request, "method": method, "params": params})
        deadline = time.monotonic() + 5
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise DeliveryError(f"{method}: daemon did not reply")
            self.socket.settimeout(remaining)
            message = self.read()
            if message.get("id") != request:
                if time.monotonic() >= deadline:
                    raise DeliveryError(f"{method}: daemon did not reply")
                continue
            if "error" in message:
                raise DeliveryError(f"{method}: {message['error']}")
            return message["result"]


def same_workspace(cwd, workspace):
    cwd, workspace = Path(cwd).resolve(), Path(workspace).resolve()
    if cwd == workspace or workspace in cwd.parents:
        return True
    # A worker can move into a worktree outside the launch directory.
    def common(path):
        result = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "--path-format=absolute", "--git-common-dir"],
            capture_output=True, text=True,
        )
        return result.stdout.strip() if result.returncode == 0 else None
    root = common(workspace)
    return bool(root and root == common(cwd))


def read_thread(rpc, identity, workspace):
    identity = thread_id(identity)
    thread = rpc.call("thread/read", {"threadId": identity, "includeTurns": False})["thread"]
    if thread.get("id") != identity or not same_workspace(thread["cwd"], workspace):
        raise DeliveryError("thread identity or workspace does not match")
    return thread


def delivery_dir(workspace):
    home = Path(os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude"))
    digest = hashlib.sha1(str(Path(workspace).resolve()).encode()).hexdigest()
    return home / "orchestrator" / digest


@contextmanager
def state_file(directory):
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / "delivery.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = directory / "delivery.json"
        state = json.loads(path.read_text()) if path.exists() else {"tasks": {}, "requests": {}}
        def save():
            with tempfile.NamedTemporaryFile(mode="w", dir=directory, delete=False) as output:
                json.dump(state, output, indent=2)
                output.flush()
                os.fsync(output.fileno())
                temporary = output.name
            os.replace(temporary, path)
        try:
            yield state, save
        finally:
            save()


class Delivery:
    def __init__(self, state, rpc, workspace, now=None, save=lambda: None):
        self.state, self.rpc = state, rpc
        self.state.setdefault("briefed", [])
        self.workspace = str(Path(workspace).resolve())
        self.now = time.time() if now is None else now
        self.save = save

    def task_key(self, identity, task):
        return json.dumps([thread_id(identity), task])

    def asked_us(self, identity, task):
        """True once that thread wrote to this folder under this task.

        An agent that opened a task with the coordinator is owed the answer it asked
        for, and it usually goes idle while waiting. Registering that exchange is not
        starting work: the worker chose both the task and the question.
        """
        inbox = delivery_dir(self.workspace) / "inbox.jsonl"
        if not inbox.is_file():
            return False
        sender = f"codex:{identity}"
        for line in inbox.read_text(errors="ignore").splitlines():
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if entry.get("from") == sender and entry.get("task") == task:
                return True
        return False

    def begin(self, identity, task):
        key = self.task_key(identity, task)
        if key in self.state["tasks"]:
            if self.state["tasks"][key]["done"]:
                raise DeliveryError("task is finished; use a new task ID for new owner-directed work")
            return
        thread = read_thread(self.rpc, identity, self.workspace)
        if thread["status"]["type"] != "active" and not self.asked_us(identity, task):
            raise DeliveryError("register only observed active work; an idle session is not a new task")
        self.state["tasks"][key] = {"thread": identity, "task": task, "done": False}

    def withdraw(self, request):
        cursor = None
        matches = []
        while True:
            result = self.rpc.call("thread/queue/list", {
                "threadId": request["thread"], "cursor": cursor, "limit": 100,
            })
            for item in result["data"]:
                # IDs are recorded before enqueueing, so a lost response is recoverable.
                # A user edit changes input and takes ownership of that queued item.
                if (item["clientUserMessageId"] == request["id"]
                        and item["input"] == request["input"]):
                    matches.append(item["id"])
            cursor = result.get("nextCursor")
            if not cursor:
                break
        for identifier in matches:
            self.rpc.call("thread/queue/delete", {
                "threadId": request["thread"], "queuedSubmissionId": identifier,
            })

    def cancel(self, identity, task, key=None):
        for request in self.state["requests"].values():
            if (request["thread"] == identity and request["task"] == task
                    and (key is None or request["key"] == key) and not request["closed"]):
                request["expires"] = 0
                self.withdraw(request)
                request["closed"] = True

    def finish(self, identity, task):
        entry = self.state["tasks"].get(self.task_key(identity, task))
        if entry is None:
            raise DeliveryError("unknown task")
        entry["done"] = True
        self.cancel(identity, task)

    def sweep(self, reset=False):
        count = 0
        if reset:
            for task in self.state["tasks"].values():
                task["done"] = True
            for request in self.state["requests"].values():
                request["expires"] = 0
            self.save()
        for request in self.state["requests"].values():
            task = self.state["tasks"][self.task_key(request["thread"], request["task"])]
            if not request["closed"] and (reset or task["done"] or request["expires"] <= self.now):
                self.withdraw(request)
                request["closed"] = True
                count += 1
        return count

    def send(self, identity, task, key, text, ttl):
        entry = self.state["tasks"].get(self.task_key(identity, task))
        if entry is None or entry["done"]:
            raise DeliveryError("delivery requires a registered, unfinished task")
        self.sweep()
        request_key = json.dumps([identity, task, key])
        previous = self.state["requests"].get(request_key)
        if previous:
            if previous["text"] != text:
                raise DeliveryError("request key already used; cancel it and give the revision a new key")
            if previous["closed"]:
                raise DeliveryError("request is already cancelled or expired")
            if not previous.get("submitted"):
                raise DeliveryError("delivery was not confirmed; cancel this request before revising it")
            return previous["id"]  # Retrying never wakes the worker twice.
        thread = read_thread(self.rpc, identity, self.workspace)
        if thread["status"]["type"] not in ("active", "idle") or thread.get("canAcceptDirectInput") is False:
            raise DeliveryError("thread cannot receive input; nothing queued for a future client")
        identifier = str(uuid.uuid4())
        until = datetime.fromtimestamp(self.now + ttl, timezone.utc).isoformat(timespec="seconds")
        message = f"[orchestrator, not the owner; task={task}; request={key}; expires={until}] {text}"
        if identity not in self.state["briefed"]:
            inbox = delivery_dir(self.workspace) / "inbox.jsonl"
            message += (
                "\nReply only if needed, by appending one JSON line to "
                f"{inbox}: " + json.dumps({"from": f"codex:{identity}", "task": task, "text": "<reply>"})
                + ". This coordination request does not expand your owner's task. "
                "If it has expired or the task is finished, disregard it."
            )
        request = {
            "id": identifier, "thread": identity, "task": task, "key": key,
            "text": text, "input": [{"type": "text", "text": message, "text_elements": []}],
            "expires": self.now + ttl, "closed": False, "submitted": False,
        }
        self.state["requests"][request_key] = request
        self.save()
        self.rpc.call("thread/queue/add", {
            "threadId": identity, "clientUserMessageId": identifier, "input": request["input"],
        })
        request["submitted"] = True
        if identity not in self.state["briefed"]:
            self.state["briefed"].append(identity)
        return identifier


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", default=os.environ.get("WB", os.getcwd()))
    commands = parser.add_subparsers(dest="command", required=True)
    lookup = commands.add_parser("thread", help="resolve an explicit UUID or a resume PID")
    lookup.add_argument("identity")
    for name in ("begin", "send", "finish", "cancel"):
        command = commands.add_parser(name)
        command.add_argument("identity", type=thread_id)
        command.add_argument("task")
        if name in ("send", "cancel"):
            command.add_argument("key")
        if name == "send":
            command.add_argument("text")
            command.add_argument("--ttl", type=int, default=300)
    commands.add_parser("sweep", help="withdraw this helper's expired pending requests")
    commands.add_parser("reset", help="withdraw previous requests when starting a fresh coordinator")
    args = parser.parse_args()
    if args.command == "thread":
        identity = resolve_pid(args.identity)
        with RPC() as rpc:
            thread = read_thread(rpc, identity, args.workspace)
            print(f"{identity}\t{thread.get('path') or ''}")
        return
    if args.command == "send" and not 1 <= args.ttl <= 3600:
        raise DeliveryError("ttl must be between 1 and 3600 seconds")
    directory = delivery_dir(args.workspace)
    if args.command in ("sweep", "reset") and not (directory / "delivery.json").exists():
        return
    with state_file(directory) as (state, save):
        # Polling a quiet folder does not need a connection to Codex.
        if args.command == "sweep" and not any(
            not item["closed"] and item["expires"] <= time.time()
            for item in state["requests"].values()
        ):
            return
        with RPC() as rpc:
            delivery = Delivery(state, rpc, args.workspace, save=save)
            if args.command == "begin":
                delivery.begin(args.identity, args.task)
            elif args.command == "send":
                print(delivery.send(args.identity, args.task, args.key, args.text, args.ttl))
            elif args.command == "finish":
                delivery.finish(args.identity, args.task)
            elif args.command == "cancel":
                delivery.cancel(args.identity, args.task, args.key)
            else:
                count = delivery.sweep(reset=args.command == "reset")
                if count:
                    print(f"withdrew {count} obsolete request(s)")


if __name__ == "__main__":
    try:
        main()
    except (DeliveryError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(f"codex delivery: {error}")
