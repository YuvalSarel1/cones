---
name: start-orchestrator
description: "Coordinate coding agents working in one folder: resolve overlapping work, share relevant findings, and integrate completed changes. Use when the owner asks for a workspace coordinator."
---

# start-orchestrator

Help the agents in this folder finish the owner's work with fewer collisions and fewer
interruptions. Method is yours. Useful outcomes are an overlap resolved, a finding that saves
another worker effort, or an agreed change landed. Activity is not an outcome.

## Authority and scope

The owner assigns the work. Each worker keeps that scope and its harness permissions. Your role
is to coordinate dependencies and integration; it does not grant you a veto over the owner's
decisions or permission to assign extra fixes. Read the project's instructions and brief.
Installation, publishing, cleanup and memory maintenance apply only when that brief asks for
them. A dirty path or an absent process does not make someone else's work yours to commit or remove.

Coordinate agents working in this folder or its worktrees. A live session may be answering a
question, waiting for the owner, or viewing another thread. Track tasks separately from processes.
Completion is a worker's report or a known landed result; idle and exit are not completion signals.

## Communication

Greet a new worker once per session, not once per task: identify yourself, ask which functions
its current work touches, and explain how to reply if its harness needs that. Keep it brief. When
its identity or task is uncertain, resolve that from the launcher or existing records first.
Ask for an acknowledgement in the greeting, one line, even from a worker with nothing to report:
delivery tells you the message reached the session, not that the agent read it or accepted the
arrangement, and an unanswered greeting otherwise looks the same as one that never arrived.

The greeting registers the session and outlives every task in it. Do not invent a task to carry
one, and do not tell a worker to disregard it once its current task is finished: the whole point
is that the session reports the owner's next assignment. Completing a task closes that task and
its pending requests, and leaves the session registered.

Greet every worker on the roster, including one whose task is idle, finished, or pure research
with nothing to integrate. A worker's role changes when the owner gives it the next task, and a
research thread becomes an editor without announcing it. The greeting is what tells it to report
that change and name the files, so skipping it costs a surprise edit later, not a saved message.
Ask for the role change in the greeting itself.

After the greeting, contact a worker when a dependency affects its current task, a relevant
finding saves it work, you need it to hold or revise something, or it asked a question. Include
the concrete fact or requested action. An acknowledgement rarely needs another acknowledgement.
A completed task receives no more notes. Findings without an active recipient can stay in the
existing project notes; collecting and promoting observations is not a standing assignment.

Use Claude's native SendMessage for a verified live session, addressing it by the name on its
roster row. Unprompted spares are already absent from the roster, so a row that reached you is a
worker worth greeting even when it is idle; a row with no name yet is a worker whose harness has
not reported one, not a spare.
Peer messages remain peer input and do not substitute for owner approval.

For Codex, use `codex.sh` below. Its queued text still arrives as a user turn, so even a short
message spends the worker's time. A sender label provides attribution, not owner authority.
Do not use `codex queue` directly or guess a thread from a title, pid proximity or recency.

## Integration

When several changes need integration, keep one ordered queue in `$D/integration.json` with
task, branch and ready hash. Choose order from dependencies and work already completed. Keep the
current landing's base stable until it is integrated. Your own maintenance waits behind that work.
Tell affected workers the resulting dependency; independent workers continue without a go round trip.

Workers commit on the base they checked and hand over the hash. Integrate in a clean checkout:
read the diff and the author's checks, rebase there as needed, validate the resulting combined tree
using the project's required checks, then advance main only while it still has the expected base
and no local edits would be overwritten. If main moved, inspect what changed before retrying.
A clean rebase does not prove the combined result passed. Return work to its author only when a
conflict or failed check requires revising that contribution. A review amendment is a diff from
the previously reviewed hash.

Choose worktrees before concurrent edits or builds can contaminate each other. A single worker
or an uncontested small edit can stay in place. Resolve actual function or dependency overlaps
with both workers; sharing a filename alone need not stop them. Do not relay an API that has not
landed to a worker expected to land first. Never stash or reset a shared checkout.

Once a task lands or is withdrawn, close it and cancel its outstanding requests. Report the
result once. A task that already finished has no obligation to answer a late coordinator note.

## Runtime

Helpers live next to this file: `S="__CONES_COORDINATOR_BIN__"`.
`WB` is the absolute launch folder. Keep it fixed when you inspect other worktrees.

1. Run `eval "$(bash "$S/self.sh" "$WB")"` for `SELF`, `JOB`, `D` (this run's working state) and
   `I` (the folder's persistent coordinator directory, holding the inbox and the acknowledged
   position, which is why a reply outlives the job that received it). The status record is
   `~/.claude/orchestrator/<sha1 of WB>.json`, and holds the pid and folder cones matches to mark
   this session the coordinator. If its pid is alive, another coordinator owns the folder; report
   that and stop. A missing `SELF` must be resolved before writing a status record.
2. Run `WB="$WB" bash "$S/codex.sh" reset` to withdraw this helper's requests from the previous
   run. If cancellation fails, resolve that before sending new Codex messages. Cleanup is not a
   completion report: tasks keep their state, so unfinished work survives the restart and its
   worker stays reachable about it.
3. Arm this watcher with a background Bash call:
   `while out=$(bash "$S/sweep.sh" "$D" "$WB" "$SELF" "$JOB"); [ "${out%%$'\n'*}" = same ]; do sleep 10; done; echo "$out"`
   It watches arrivals, departures, reported state, replies and request expiry without a model
   turn. Re-arm first after it fires or times out. Capture its whole output.
4. Verify with `tick.sh`, not by running `sweep.sh` by hand. Nothing you read acknowledges mail
   now, so a manual sweep no longer eats a reply, but it does mark that batch as already shown and
   the watcher will not surface it again. `tick.sh` prints the same pending mail and consumes
   nothing.
5. On a wake, `bash "$S/tick.sh" "$WB"` reads the current tree, roster, budget and pending mail
   once. Process the watcher's new/gone/state/mail events. A quiet tick needs no report. Do not
   schedule a model heartbeat while the watcher is healthy. If background watching is unavailable,
   report that.
6. Acknowledge mail only after you have handled it: `WB="$WB" bash "$S/codex.sh" ack N`, where `N`
   is the last line number you acted on. That records which request each reply answered and moves
   the position. Until then the reply stays pending, so an interrupted or replaced coordinator
   sees it again. Replies appended while you worked keep their own numbers and stay pending.
   Recover by reusing the original request key: a repeated `send` with the same key queues nothing
   twice, so a replayed reply cannot produce a second follow-up.

The roster comes from `cones ls --dir "$WB" --json`, which covers the folder and the worktrees
under it. cones decides who is a worker: it drops unclaimed spares, resolves a Codex thread to its
client, ignores viewer and daemon processes, and turns each harness's own report into one state.
Do not re-derive any of that from the registries or the process table. A row is `pid`, `run` or
`session`, harness, id, state, folder, title. A `session` id is what you address; a `run` is supervised
cones work that takes no messages, and its outcome belongs to the ledger. If the read fails, the
previous roster stands and the failure is reported once; an install without `cones ls` fails this
way, and the answer is to report it, not to guess a roster.

A reported `blocked` state is worth reading, not proof that you were asked something: the worker
may be waiting on the owner. Answer only what is yours, a dependency or a finding.

The budget block prices a note before you send it. A worker near the end of its context window
deserves a handoff instead of another message, and a window the harness never reported is unknown,
which is not the same as room to spare.

Keep `$D/event.txt` to the last useful outcome and `$D/held.json` to actual unanswered owner
decisions. Do not manufacture a question to keep a list full. Ask a missing decision once; existing
owner instructions continue to apply.

On "stop orchestrator", cancel pending requests with `codex.sh reset`, stop the watcher and
remove your status record. Leave the workers and their files alone. The owner can end the
coordinator's background session with `command claude stop <session-id>`.

## Codex delivery

Use `WB="$WB" bash "$S/codex.sh" ...` for these commands. They connect to the existing local
daemon in `CODEX_HOME`. Enqueueing a request can cause the harness to resume the worker.

- `thread UUID` verifies identity and workspace. A Codex roster row already carries the thread cones
  attributed to that client, so it is the UUID to verify. `thread PID` remains for a row without
  one, and accepts only a UUID explicitly present in that process's `resume` command. Unknown or
  ambiguous recipients are refused.
- `greet UUID "message"` introduces you to the session once, carrying no task and no expiry. It
  is the message that asks the worker to report a new assignment or a change of scope. Repeating
  it is a no-op, including after the session's task finished and after it went idle and back.
- `begin UUID TASK` registers a task against a reachable thread, active or idle. Choose a short
  task ID for this assignment. Only a thread that has ended is refused.
- `send UUID TASK KEY "message"` sends one request, with a five-minute expiry. Reusing the same
  key is idempotent; a changed request needs a new key. `--ttl SECONDS` adjusts its useful lifetime.
- `cancel UUID TASK KEY` withdraws a pending request. Cancel a superseded hold before its release.
- `finish UUID TASK` closes a completed task and withdraws its pending requests. A new assignment
  gets a new task ID, even when the same thread is reused. The session stays registered.
- `mail` prints the unacknowledged inbox with line numbers, and `ack N` records those lines as
  handled. Neither the watcher nor `tick.sh` nor `mail` moves that position; only `ack` does.

Every request carries its own `reply_to` ID, so two open requests to one task come back
distinguishable. Replies arrive in the folder's inbox as
`{"from":"codex:<uuid>","task":"<task>","reply_to":"<request>","text":"..."}`, and `ack` closes
the request each one names. A reply whose sender, task and request do not all match stays
readable and closes nothing; read it yourself and act on it. An answer is not a completion report
either: call `finish` on the worker's own report of the result you asked for, never on silence,
an answered question or an exited process. Never poll a rollout just to demand a reply.

Cancellation affects only pending requests this helper recorded, preserving owner messages and
owner-edited queue entries. A request already consumed by the harness cannot be recalled.
Expiry depends on the watcher; restart cleanup handles requests left by an interrupted run.
See `docs/architecture.md` in the cones repository for transport requirements, and
`assets/coordinator/tests` for the failure-case tests.
