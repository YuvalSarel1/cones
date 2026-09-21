"""The roster the coordinator acts on, built only from what cones reported."""

import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "fleet",
    Path(__file__).resolve().parents[1] / "skills/start-orchestrator/bin/fleet.py",
)
fleet = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fleet)


def ls(*rows):
    """A `cones ls --json` stream: one object per line."""
    return io.StringIO("".join(json.dumps(row) + "\n" for row in rows))


def session(pid, ident, state, cwd="/wb", harness="claude", title="", **extra):
    return {
        "kind": "session",
        "status": state,
        "session": {
            "pid": pid,
            "session_id": ident,
            "harness": harness,
            "cwd": cwd,
            "title": title,
            **extra,
        },
    }


def run(pid, run_id, status, cwd="/wb"):
    return {
        "kind": "run",
        "status": status,
        "started": {"pid": pid, "run_id": run_id, "harness": "codex", "cwd": cwd},
    }


class Roster(unittest.TestCase):
    def test_a_session_row_keeps_the_id_the_coordinator_addresses(self):
        stream = ls(session(11, "abc", "active", harness="codex", title="parser work"))
        rows = fleet.rows(stream, "99")

        self.assertEqual(rows, ["11\tsession\tcodex\tabc\tactive\t/wb\tparser work"])

    def test_only_a_started_run_is_live_work(self):
        stream = ls(run(12, "r-1", "started"), run(13, "r-2", "ok"))

        self.assertEqual(fleet.rows(stream, "99"), ["12\trun\tcodex\tr-1\tstarted\t/wb\t"])

    def test_a_codex_worker_without_a_pid_stays_on_the_roster(self):
        # cones reports no pid for a Codex thread, and dropping those rows hid every arrival.
        stream = ls(session(None, "01a0-thread", "active", harness="codex", title="bindings"))

        self.assertEqual(fleet.rows(stream, "99"), ["-\tsession\tcodex\t01a0-thread\tactive\t/wb\tbindings"])

    def test_a_pidless_arrival_is_reported(self):
        was = fleet.rows(ls(session(11, "abc", "active")), "99")
        now = fleet.rows(ls(session(11, "abc", "active"), session(None, "thr", "active", harness="codex")), "99")

        self.assertEqual(fleet.delta(was, now), ["new:\n-\tsession\tcodex\tthr\tactive\t/wb\t"])

    def test_the_coordinator_is_not_one_of_its_own_workers(self):
        rows = fleet.rows(ls(session(99, "self", "active"), session(11, "abc", "idle")), "99")

        self.assertEqual([row.split("\t")[0] for row in rows], ["11"])

    def test_a_line_that_is_not_json_is_not_a_worker_but_a_pidless_session_is(self):
        stream = io.StringIO(
            "cones: warning\n\n"
            + json.dumps({"kind": "session", "status": "idle", "session": {"session_id": "x"}})
            + "\n"
            + json.dumps(session(11, "abc", "idle"))
            + "\n"
        )

        self.assertEqual([row.split("\t")[3] for row in fleet.rows(stream, "99")], ["abc", "x"])

    def test_rows_are_ordered_by_pid_so_an_unchanged_roster_compares_equal(self):
        stream = ls(session(30, "c", "idle"), session(4, "a", "idle"), session(12, "b", "idle"))

        self.assertEqual([row.split("\t")[0] for row in fleet.rows(stream, "99")], ["4", "12", "30"])


class Delta(unittest.TestCase):
    def test_an_unchanged_roster_reports_nothing(self):
        rows = fleet.rows(ls(session(11, "abc", "active")), "99")

        self.assertEqual(fleet.delta(rows, rows), [])

    def test_arrivals_and_departures_are_named(self):
        was = fleet.rows(ls(session(11, "abc", "active")), "99")
        now = fleet.rows(ls(session(12, "def", "idle")), "99")

        sections = fleet.delta(was, now)

        self.assertEqual(len(sections), 2)
        self.assertTrue(sections[0].startswith("new:\n12\t"))
        self.assertTrue(sections[1].startswith("gone:\n11\t"))

    def test_a_reported_state_change_is_worth_a_turn(self):
        was = fleet.rows(ls(session(11, "abc", "active")), "99")
        now = fleet.rows(ls(session(11, "abc", "blocked")), "99")

        self.assertEqual(fleet.delta(was, now), ["state:\n11\tsession\tabc\tactive > blocked"])

    def test_identity_survives_client_restart_but_not_a_conversation_switch(self):
        was = fleet.rows(ls(session(11, "abc", "idle", harness="codex")), "99")
        restarted = fleet.rows(ls(session(12, "abc", "idle", harness="codex")), "99")
        self.assertEqual(fleet.delta(was, restarted), [])
        switched = fleet.rows(ls(session(11, "def", "idle", harness="codex")), "99")
        self.assertEqual(fleet.delta(was, switched), [
            "new:\n" + switched[0], "gone:\n" + was[0],
        ])
        changed = fleet.rows(ls(session(12, "abc", "active", harness="codex")), "99")
        self.assertEqual(fleet.delta(was, changed), ["state:\n12\tsession\tabc\tidle > active"])
        other_harness = fleet.rows(ls(session(11, "abc", "idle", harness="pi")), "99")
        self.assertEqual(fleet.delta(was, other_harness), [
            "new:\n" + other_harness[0], "gone:\n" + was[0],
        ])

    def test_a_worker_that_only_renamed_itself_or_moved_folder_is_not_a_state_change(self):
        was = fleet.rows(ls(session(11, "abc", "active")), "99")
        now = fleet.rows(
            ls(session(11, "abc", "active", cwd="/wb/.worktrees/feat", title="renamed")), "99"
        )

        self.assertEqual(fleet.delta(was, now), [])


class Budget(unittest.TestCase):
    def read(self, *rows):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fleet.json"
            path.write_text("".join(json.dumps(row) + "\n" for row in rows))
            return fleet.budget(str(path))

    def test_a_reported_window_is_priced_as_a_share_of_itself(self):
        lines = self.read(session(11, "abc", "active", context_tokens=90_000, context_window=200_000))

        self.assertEqual(lines, ["abc  90000/200000 (45%)  -"])

    def test_an_unreported_window_stays_unknown_rather_than_room(self):
        lines = self.read(session(11, "abc", "active", context_tokens=90_000, cost_usd=1.5))

        self.assertEqual(lines, ["abc  90000/window unknown  1.5000"])

    def test_a_worker_the_harness_never_priced_is_left_out(self):
        lines = self.read(session(11, "abc", "active"), run(12, "r-1", "started"))

        self.assertEqual(lines, ["none reported"])

    def test_a_read_that_never_happened_reports_nothing_it_cannot_know(self):
        self.assertEqual(fleet.budget("/no/such/fleet.json"), ["none reported"])


class Status(unittest.TestCase):
    def test_the_record_carries_the_two_fields_cones_reads(self):
        with tempfile.TemporaryDirectory() as home:
            with mock.patch.dict("os.environ", {"HOME": home}):
                fleet.write_status("/wb", "4242")
                written = list((Path(home) / ".claude/orchestrator").glob("*.json"))

            self.assertEqual(len(written), 1)
            self.assertEqual(json.loads(written[0].read_text()), {"cwd": "/wb", "pid": 4242})

    def test_without_a_resolved_pid_nothing_claims_the_folder(self):
        with tempfile.TemporaryDirectory() as home:
            with mock.patch.dict("os.environ", {"HOME": home}):
                fleet.write_status("/wb", "")

            self.assertFalse((Path(home) / ".claude/orchestrator").exists())


if __name__ == "__main__":
    unittest.main()
