#!/usr/bin/env python3
"""What cones reports about this folder, in the two shapes the coordinator reads.

usage: fleet.py sweep D WB SELF   (stdin is `cones ls --dir WB --json`; writes D/roster.now,
                                   D/fleet.json's roster view and the dashboard status record)
       fleet.py budget D          (what a message costs each worker, from the saved read)

cones owns discovery. It drops unclaimed spare sessions, resolves a Codex thread to the client
that writes it, ignores app-server and viewer processes, and normalizes each harness's own report
into one state. Re-deriving any of that from the registries and the process table is what this
helper used to do, and every one of those cases had already been fixed once in cones.

A row is one worker: pid, run or session, harness, id, state, folder, title. `run` rows are
supervised cones runs, which take no messages; their outcome belongs to the ledger. Session rows
carry the native id the coordinator addresses: a Claude session id, or a Codex thread. The title is
the name a session shows, which is how a Claude worker is addressed and how a note about it reads;
it is empty until the harness reports one, and cones withholds a name that is only the job's id.

Prints nothing when the roster holds the same native identities in the same states, else the `new:`,
`gone:` and `state:` sections that changed. Which of those is worth waking a coordinator is
sweep.sh's decision, not this module's: it wakes for `new:` alone. The caller advances roster.prev
every pass, so a change nobody woke for cannot come back as news later.
"""

import hashlib
import json
import os
import sys
import tempfile


def rows(stream, self_pid):
    """One roster row per live worker, skipping the coordinator's own session."""
    out = []
    for line in stream:
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            # A warning on stdout is not a row; the caller reports the failing read itself.
            continue
        kind = row.get("kind")
        if kind == "session":
            record = row.get("session") or {}
            ident = record.get("session_id") or ""
        elif kind == "run" and row.get("status") == "started":
            # Only a started run is live. Every terminal status is history the ledger keeps.
            record = row.get("started") or {}
            ident = record.get("run_id") or ""
        else:
            continue
        pid = record.get("pid")
        if pid is not None and str(pid) == self_pid:
            continue
        # A Codex worker is its thread, and cones reports no pid for one. Requiring a pid here
        # dropped every Codex row from the roster, so arrivals were never reported.
        out.append(
            "\t".join(
                [
                    str(pid) if pid is not None else "-",
                    kind,
                    str(record.get("harness") or "?"),
                    str(ident),
                    str(row.get("status") or "-"),
                    str(record.get("cwd") or ""),
                    str(record.get("title") or ""),
                ]
            )
        )
    def order(row):
        pid = row.split("\t")[0]
        return (0, int(pid), "") if pid.isdigit() else (1, 0, row)

    out.sort(key=order)
    return out


def delta(previous, current):
    """The sections worth a model turn: arrivals, departures, and a state that changed."""
    # The native conversation/run is the worker's identity. A client can restart
    # or switch conversations without its PID describing the task transition.
    def identity(row):
        fields = row.split("\t")
        return tuple(fields[1:4])

    was = {identity(row): row for row in previous}
    now = {identity(row): row for row in current}
    sections = []
    arrived = [now[key] for key in now if key not in was]
    left = [was[key] for key in was if key not in now]
    moved = []
    for key, row in now.items():
        if key not in was:
            continue
        before, after = was[key].split("\t")[4], row.split("\t")[4]
        if before != after:
            fields = row.split("\t")
            moved.append(f"{fields[0]}\t{fields[1]}\t{fields[3]}\t{before} > {after}")
    for name, lines in (("new", arrived), ("gone", left), ("state", moved)):
        if lines:
            sections.append(f"{name}:\n" + "\n".join(lines))
    return sections


def write_status(workspace, self_pid):
    """The record cones matches by pid and folder to mark this session the coordinator.

    It reads those two fields and nothing else, so nothing else is written: a peers list or a
    held-decision mirror here would be state with no reader.
    """
    if not self_pid:
        return
    # Same home the roster read uses. cones resolves the Claude directory from
    # CLAUDE_CONFIG_DIR before the default, so a record written under a hard-coded
    # ~/.claude would be invisible to it under a custom home.
    home = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    directory = os.path.join(home, "orchestrator")
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, hashlib.sha1(workspace.encode()).hexdigest() + ".json")
    # A per-process temporary, not a fixed `.json.tmp`: a replacement coordinator overlapping the
    # one it takes over from, or a watcher left armed from an earlier arm, writes the same record.
    # With one shared name the second writer's rename destroys the first writer's source and that
    # process dies on os.replace, taking its watcher down with it.
    handle, temporary = tempfile.mkstemp(dir=directory, prefix=".status.", suffix=".tmp")
    try:
        with os.fdopen(handle, "w") as out:
            json.dump({"cwd": workspace, "pid": int(self_pid)}, out)
        os.replace(temporary, path)
    except BaseException:
        if os.path.exists(temporary):
            os.unlink(temporary)
        raise


def budget(path):
    """Context and cost per session, so a note is priced before it is sent.

    A window the harness never reported stays unknown. Treating it as room is how a coordinator
    sends a follow-up into a session that has none left.
    """
    out = []
    try:
        with open(path) as saved:
            lines = saved.readlines()
    except FileNotFoundError:
        return ["none reported"]
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("kind") != "session":
            continue
        record = row.get("session") or {}
        used, window = record.get("context_tokens"), record.get("context_window")
        cost = record.get("cost_usd")
        if used is None and window is None and cost is None:
            continue
        if used is not None and window:
            context = "{}/{} ({}%)".format(used, window, round(100 * used / window))
        elif used is not None:
            context = "{}/window unknown".format(used)
        else:
            context = "unknown"
        spent = "-" if cost is None else format(cost, ".4f")
        out.append("{}  {}  {}".format(record.get("session_id") or "?", context, spent))
    return out or ["none reported"]


def main():
    if sys.argv[1] == "budget":
        print("\n".join(budget(os.path.join(sys.argv[2], "fleet.json"))))
        return
    state_dir, workspace, self_pid = sys.argv[2], sys.argv[3], sys.argv[4]
    current = rows(sys.stdin, self_pid)
    now_path = os.path.join(state_dir, "roster.now")
    with open(now_path, "w") as out:
        out.write("".join(f"{row}\n" for row in current))
    try:
        with open(os.path.join(state_dir, "roster.prev")) as previous:
            was = [line.rstrip("\n") for line in previous if line.strip()]
    except FileNotFoundError:
        was = []
    write_status(workspace, self_pid)
    sections = delta(was, current)
    if sections:
        print("\n".join(sections))


if __name__ == "__main__":
    main()
