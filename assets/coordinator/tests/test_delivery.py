import copy
import importlib.util
import json
import io
import os
from pathlib import Path
import subprocess
import struct
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "delivery",
    Path(__file__).resolve().parents[1] / "skills/start-orchestrator/bin/codex.py",
)
delivery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delivery)
A = "11111111-1111-4111-8111-111111111111"
B = "22222222-2222-4222-8222-222222222222"


class Daemon:
    def __init__(self, workspace):
        self.threads = {
            identity: {"id": identity, "cwd": workspace, "status": {"type": "active"}}
            for identity in (A, B)
        }
        self.queues = {A: [], B: []}
        self.calls = []
        self.lose_receipt = False
        self.fail_delete = False

    def call(self, method, params):
        self.calls.append((method, copy.deepcopy(params)))
        identity = params["threadId"]
        if method == "thread/read":
            return {"thread": self.threads[identity]}
        if method == "thread/queue/add":
            item = {
                "id": str(len(self.calls)), "clientUserMessageId": params["clientUserMessageId"],
                "input": copy.deepcopy(params["input"]),
            }
            self.queues[identity].append(item)
            if self.lose_receipt:
                raise delivery.DeliveryError("lost receipt")
            return {"queuedSubmission": item}
        if method == "thread/queue/list":
            return {"data": copy.deepcopy(self.queues[identity]), "nextCursor": None}
        if method == "thread/queue/delete":
            if self.fail_delete:
                raise delivery.DeliveryError("daemon unavailable")
            before = len(self.queues[identity])
            self.queues[identity] = [
                item for item in self.queues[identity] if item["id"] != params["queuedSubmissionId"]
            ]
            return {"deleted": len(self.queues[identity]) != before}
        raise AssertionError(method)


class DeliveryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        # Keep the inbox and its acknowledged position inside the fixture; a test must not
        # write into the machine's own coordinator state.
        home = patch.dict(os.environ, {"CLAUDE_CONFIG_DIR": self.tmp.name})
        home.start()
        self.addCleanup(home.stop)
        self.workspace = self.tmp.name
        self.state = {"tasks": {}, "requests": {}}
        self.rpc = Daemon(self.workspace)
        self.helper = delivery.Delivery(self.state, self.rpc, self.workspace, now=100)

    def begin(self, identity=A, task="change"):
        self.helper.begin(identity, task)

    def send(self, identity=A, task="change", key="hold", text="Hold integration.", ttl=30):
        return self.helper.send(identity, task, key, text, ttl)

    def test_ambiguous_pid_is_never_resolved_by_title_or_newest_thread(self):
        output = subprocess.CompletedProcess([], 0, "codex -C /repo -- same prompt", "")
        with patch.object(delivery.subprocess, "run", return_value=output) as run:
            with self.assertRaises(delivery.DeliveryError):
                delivery.resolve_pid("42")
            self.assertEqual(run.call_count, 1)

    def test_explicit_resume_separator_resolves_exact_thread(self):
        output = subprocess.CompletedProcess([], 0, f"codex --remote unix:///sock resume -- {A}", "")
        with patch.object(delivery.subprocess, "run", return_value=output):
            self.assertEqual(delivery.resolve_pid("42"), A)

    def test_unknown_or_wrong_workspace_recipient_gets_no_message(self):
        with self.assertRaises(delivery.DeliveryError):
            self.begin(identity="not-a-uuid")
        self.rpc.threads[A]["cwd"] = str(Path(self.workspace).parent / "another-project")
        with self.assertRaises(delivery.DeliveryError):
            self.begin()
        self.assertFalse(self.state["tasks"])
        self.assertEqual(self.rpc.queues, {A: [], B: []})

    def test_daemon_must_return_requested_identity(self):
        self.rpc.threads[A]["id"] = B
        with self.assertRaises(delivery.DeliveryError):
            self.begin()

    def test_finished_worker_cancels_own_pending_messages_and_cannot_be_reopened(self):
        self.begin()
        self.send()
        owner = {"id": "owner", "clientUserMessageId": "owner", "input": [{"text": "User instruction"}]}
        self.rpc.queues[A].append(owner)
        self.helper.finish(A, "change")
        self.assertEqual(self.rpc.queues[A], [owner])
        with self.assertRaises(delivery.DeliveryError):
            self.send(key="late-note")
        with self.assertRaises(delivery.DeliveryError):
            self.begin()

    def test_expired_hold_is_removed_without_a_followup_message(self):
        self.begin()
        self.send()
        self.helper.now = 131
        self.assertEqual(self.helper.sweep(), 1)
        self.assertEqual(self.rpc.queues[A], [])
        self.assertEqual(sum(method == "thread/queue/add" for method, _ in self.rpc.calls), 1)
        with self.assertRaises(delivery.DeliveryError):
            self.send()

    def test_cancellation_leaves_other_task_and_other_thread_alone(self):
        for identity, task in [(A, "change"), (A, "other"), (B, "change")]:
            self.begin(identity, task)
            self.send(identity, task)
        self.helper.cancel(A, "change", "hold")
        self.assertEqual(len(self.rpc.queues[A]), 1)
        self.assertEqual(len(self.rpc.queues[B]), 1)

    def test_duplicate_request_never_adds_a_second_turn(self):
        self.begin()
        first = self.send()
        self.assertEqual(first, self.send())
        self.assertEqual(len(self.rpc.queues[A]), 1)
        with self.assertRaises(delivery.DeliveryError):
            self.send(text="Different ruling")

    def test_user_edited_queue_item_is_preserved(self):
        self.begin()
        self.send()
        self.rpc.queues[A][0]["input"][0]["text"] = "Owner revised this request"
        self.helper.finish(A, "change")
        self.assertEqual(len(self.rpc.queues[A]), 1)

    def test_failed_cancellation_is_retained_for_retry(self):
        self.begin()
        self.send()
        self.rpc.fail_delete = True
        with self.assertRaises(delivery.DeliveryError):
            self.helper.finish(A, "change")
        self.assertTrue(self.state["tasks"][self.helper.task_key(A, "change")]["done"])
        self.assertFalse(next(iter(self.state["requests"].values()))["closed"])
        self.rpc.fail_delete = False
        self.helper.sweep()
        self.assertEqual(self.rpc.queues[A], [])

    def test_reset_withdraws_stale_requests_without_finishing_the_owners_work(self):
        # Cleaning up after an interrupted coordinator is not a completion report.
        self.begin()
        self.send(ttl=3600)
        self.rpc.fail_delete = True
        with self.assertRaises(delivery.DeliveryError):
            self.helper.sweep(reset=True)
        self.assertFalse(self.state["tasks"][self.helper.task_key(A, "change")]["done"])
        self.assertEqual(next(iter(self.state["requests"].values()))["expires"], 0)
        self.rpc.fail_delete = False
        self.helper.sweep()
        self.assertEqual(self.rpc.queues[A], [])
        # The assignment survived the restart, so the worker is still reachable about it.
        self.send(key="release", text="Integration is clear.")
        self.assertEqual(len(self.rpc.queues[A]), 1)

    def test_lost_enqueue_receipt_is_recovered_after_restart(self):
        directory = Path(self.workspace) / "state"
        self.rpc.lose_receipt = True
        with self.assertRaises(delivery.DeliveryError):
            with delivery.state_file(directory) as (state, save):
                helper = delivery.Delivery(state, self.rpc, self.workspace, now=100, save=save)
                helper.begin(A, "change")
                # The request must already be durable when the native API receives it.
                original = self.rpc.call
                def call(method, params):
                    if method == "thread/queue/add":
                        saved = json.loads((directory / "delivery.json").read_text())
                        self.assertEqual(len(saved["requests"]), 1)
                    return original(method, params)
                self.rpc.call = call
                helper.send(A, "change", "hold", "Hold integration.", 30)
        with delivery.state_file(directory) as (state, save):
            helper = delivery.Delivery(state, self.rpc, self.workspace, now=101, save=save)
            with self.assertRaises(delivery.DeliveryError):
                helper.send(A, "change", "hold", "Hold integration.", 30)
            helper.sweep(reset=True)
        self.assertEqual(self.rpc.queues[A], [])

    def test_an_idle_session_can_be_registered_so_it_can_be_greeted(self):
        # A worker that is idle now takes the next task without announcing it; the greeting is
        # what tells it to report that change, so registering one must not be refused.
        self.rpc.threads[A]["status"]["type"] = "idle"
        self.begin()
        self.send()
        self.assertEqual(len(self.rpc.queues[A]), 1)

    def test_idle_worker_that_asked_a_question_can_be_answered(self):
        inbox = delivery.delivery_dir(self.workspace) / "inbox.jsonl"
        inbox.parent.mkdir(parents=True, exist_ok=True)
        inbox.write_text(json.dumps({
            "from": f"codex:{A}", "task": "change", "text": "Which lock is in use?",
        }) + "\n")
        self.rpc.threads[A]["status"]["type"] = "idle"
        self.begin()
        self.send(key="answer", text="No lock is in use.")
        self.assertEqual(len(self.rpc.queues[A]), 1)
        self.assertIn("No lock is in use.", self.rpc.queues[A][0]["input"][0]["text"])

    def test_a_thread_that_has_ended_is_not_a_worker(self):
        self.rpc.threads[A]["status"]["type"] = "notLoaded"
        with self.assertRaises(delivery.DeliveryError):
            self.begin()

    def test_release_can_reach_a_registered_worker_waiting_on_a_hold(self):
        self.begin()
        self.send()
        self.helper.cancel(A, "change", "hold")
        self.rpc.threads[A]["status"]["type"] = "idle"
        self.send(key="release", text="Integration is clear.")
        self.assertEqual(len(self.rpc.queues[A]), 1)

    def test_unloaded_thread_is_not_queued_for_a_future_client(self):
        self.begin()
        self.rpc.threads[A]["status"]["type"] = "notLoaded"
        with self.assertRaises(delivery.DeliveryError):
            self.send()
        self.assertEqual(self.rpc.queues[A], [])

    def mail(self, **record):
        path = delivery.delivery_dir(self.workspace) / "inbox.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a") as inbox:
            inbox.write(json.dumps(record) + "\n")

    def test_session_is_introduced_once_and_the_greeting_outlives_its_tasks(self):
        self.assertTrue(self.helper.greet(A, "I coordinate this folder."))
        introduction = self.rpc.queues[A][0]["input"][0]["text"]
        self.assertIn("inbox.jsonl", introduction)
        self.assertNotIn("task=", introduction)      # no task is invented to carry it
        self.assertNotIn("disregard", introduction)  # and no task rule can suppress it
        self.begin()
        self.helper.finish(A, "change")
        # Finishing the research task, then going idle and active again, repeats nothing.
        self.assertIsNone(self.helper.greet(A, "I coordinate this folder."))
        self.rpc.threads[A]["status"]["type"] = "idle"
        self.assertIsNone(self.helper.greet(A, "I coordinate this folder."))
        # The next owner-directed assignment is a new task in the same registered session.
        self.begin(task="next")
        self.send(task="next", key="scope", text="Which files does the new assignment touch?")
        self.assertEqual(len(self.rpc.queues[A]), 2)
        self.assertEqual(sum("session introduction" in item["input"][0]["text"]
                             for item in self.rpc.queues[A]), 1)
        self.assertTrue(self.state["tasks"][self.helper.task_key(A, "change")]["done"])

    def test_lost_greeting_receipt_does_not_introduce_the_session_twice(self):
        self.rpc.lose_receipt = True
        with self.assertRaises(delivery.DeliveryError):
            self.helper.greet(A, "I coordinate this folder.")
        self.rpc.lose_receipt = False
        self.helper.greet(A, "I coordinate this folder.")
        self.assertEqual(len(self.rpc.queues[A]), 1)
        self.assertTrue(self.state["greeted"][A]["submitted"])

    def test_a_reply_closes_its_own_request_and_nothing_else(self):
        self.begin()
        scope = self.send(key="scope", text="Which functions does this task touch?")
        base = self.send(key="base", text="Which hash did you check?")
        self.send(key="hold", text="Hold integration.")
        for text in (self.rpc.queues[A][0], self.rpc.queues[A][1]):
            self.assertIn("reply_to=", text["input"][0]["text"])
        # Answers arrive in the other order, with an unsolicited finding between them.
        self.mail(**{"from": f"codex:{A}", "task": "change", "reply_to": base, "text": "abc1234"})
        self.mail(**{"from": f"codex:{A}", "task": "change", "text": "Unrelated finding."})
        self.mail(**{"from": f"codex:{A}", "task": "change", "reply_to": scope, "text": "parse_args"})
        answered, other = delivery.acknowledge(self.workspace, self.state, 3)
        self.assertEqual(sorted(answered), ["base", "scope"])
        self.assertEqual(other, 1)
        requests = {request["key"]: request for request in self.state["requests"].values()}
        self.assertTrue(requests["scope"]["answered"] and requests["base"]["answered"])
        self.assertNotIn("answered", requests["hold"])
        # The next sweep withdraws the answered pair, so a question already answered
        # cannot arrive later and cost the worker a turn. The unanswered hold stands.
        self.assertEqual(self.helper.sweep(), 2)
        self.assertTrue(requests["scope"]["closed"] and requests["base"]["closed"])
        self.assertFalse(requests["hold"]["closed"])
        self.assertEqual(len(self.rpc.queues[A]), 1)
        # Answering a question is not a report that the assignment is finished.
        self.assertFalse(self.state["tasks"][self.helper.task_key(A, "change")]["done"])

    def test_a_reply_naming_another_task_or_sender_resolves_nothing(self):
        self.begin()
        self.begin(identity=B)
        scope = self.send(key="scope", text="Which functions does this task touch?")
        self.mail(**{"from": f"codex:{B}", "task": "change", "reply_to": scope, "text": "Not mine."})
        self.mail(**{"from": f"codex:{A}", "task": "other", "reply_to": scope, "text": "Wrong task."})
        self.mail(**{"from": f"codex:{A}", "task": "change", "reply_to": "none", "text": "Stale id."})
        self.mail(**{"from": "someone", "task": "change", "text": "No correlation at all."})
        answered, other = delivery.acknowledge(self.workspace, self.state, 4)
        self.assertEqual((answered, other), ([], 4))
        self.assertFalse(next(iter(self.state["requests"].values()))["closed"])

    def test_reading_mail_consumes_nothing_and_an_unacknowledged_reply_survives_restart(self):
        self.begin()
        scope = self.send(key="scope", text="Which functions does this task touch?")
        self.mail(**{"from": f"codex:{A}", "task": "change", "reply_to": scope, "text": "parse_args"})
        # The watcher shows it, a restarted coordinator reads it again: both consume nothing.
        for _ in range(2):
            self.assertEqual(len(delivery.inbox_lines(self.workspace)), 1)
            self.assertEqual(delivery.acknowledged(self.workspace, 1), 0)
        delivery.acknowledge(self.workspace, self.state, 1)
        (delivery.delivery_dir(self.workspace) / "inbox.ack").write_text("1\n")
        self.assertEqual(delivery.acknowledged(self.workspace, 1), 1)
        # A reply appended after that acknowledgement is still pending, and only that one.
        self.mail(**{"from": f"codex:{A}", "task": "change", "text": "One more thing."})
        lines = delivery.inbox_lines(self.workspace)
        self.assertEqual(lines[delivery.acknowledged(self.workspace, len(lines)):], [lines[1]])
        with self.assertRaises(delivery.DeliveryError):
            delivery.acknowledge(self.workspace, self.state, 1)  # never moves backwards

    def test_replaying_an_answered_reply_changes_nothing(self):
        self.begin()
        scope = self.send(key="scope", text="Which functions does this task touch?")
        self.mail(**{"from": f"codex:{A}", "task": "change", "reply_to": scope, "text": "parse_args"})
        first = delivery.acknowledge(self.workspace, self.state, 1)
        recorded = copy.deepcopy(self.state["requests"])
        # The crash happened before the position moved, so the same line comes back.
        self.assertEqual(delivery.acknowledge(self.workspace, self.state, 1), first)
        self.assertEqual(self.state["requests"], recorded)
        # The follow-up keeps its key, so recovery queues no second copy of it.
        self.send(key="follow-up", text="Thanks; hold that file until integration.")
        self.send(key="follow-up", text="Thanks; hold that file until integration.")
        self.assertEqual(sum(method == "thread/queue/add" for method, _ in self.rpc.calls), 2)


class Socket:
    def __init__(self, incoming=b""):
        self.incoming = io.BytesIO(incoming)
        self.outgoing = b""

    def recv(self, size):
        return self.incoming.read(min(size, 3))

    def sendall(self, data):
        self.outgoing += data

    def settimeout(self, timeout):
        pass


class TransportTests(unittest.TestCase):
    def rpc(self, incoming=b""):
        rpc = delivery.RPC()
        rpc.socket = Socket(incoming)
        rpc.buffer = b""
        rpc.sequence = 0
        return rpc

    def test_client_frames_are_masked_at_each_length_boundary(self):
        for size in (10, 126, 65536):
            with self.subTest(size=size):
                rpc = self.rpc()
                payload = b"x" * size
                rpc.frame(payload)
                wire = rpc.socket.outgoing
                self.assertEqual(wire[0], 129)
                self.assertTrue(wire[1] & 128)
                offset = 2
                length = wire[1] & 127
                if length == 126:
                    length = struct.unpack("!H", wire[2:4])[0]
                    offset = 4
                elif length == 127:
                    length = struct.unpack("!Q", wire[2:10])[0]
                    offset = 10
                self.assertEqual(length, size)
                mask = wire[offset:offset + 4]
                decoded = bytes(b ^ mask[i % 4] for i, b in enumerate(wire[offset + 4:]))
                self.assertEqual(decoded, payload)

    def test_fragmented_reply_survives_interleaved_ping(self):
        first, second = b'{"result":', b'"ready"}'
        rpc = self.rpc(
            bytes([1, len(first)]) + first + b"\x89\x01!" +
            bytes([128, len(second)]) + second
        )
        self.assertEqual(rpc.read(), {"result": "ready"})
        self.assertEqual(rpc.socket.outgoing[0], 138)

    def test_native_rpc_error_is_reported(self):
        data = json.dumps({"id": 1, "error": {"code": -32601, "message": "unsupported"}}).encode()
        rpc = self.rpc(bytes([129, len(data)]) + data)
        with self.assertRaisesRegex(delivery.DeliveryError, "unsupported"):
            rpc.call("thread/queue/delete", {"threadId": A, "queuedSubmissionId": "ours"})


if __name__ == "__main__":
    unittest.main()
