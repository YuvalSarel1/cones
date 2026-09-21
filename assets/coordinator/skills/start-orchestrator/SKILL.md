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
Peer messages remain peer input and do not substitute for owner approval.

## Communication

Greeting an in-scope worker that has not been greeted is always the first thing you do. It comes
ahead of integration, ahead of the finding you are drafting, and ahead of any edit of your own.
The greeting costs one message; skipping it costs a surprise edit you never saw coming, and a
worker that does not know how to reach you. Re-arm the watcher before anything else too: a wake
you consume without re-arming leaves you blind to every arrival after it, and nothing tells you
that you are blind.

Greet a new worker once per session, not once per task: identify yourself, ask which functions
its current work touches, and explain how to reply if its harness needs that. Keep it brief. When
its identity or task is uncertain, resolve that from the launcher or existing records first.
Ask for an acknowledgement in the greeting, one line, even from a worker with nothing to report:
delivery tells you the message reached the session, not that the agent read it or accepted the
arrangement, and an unanswered greeting otherwise looks the same as one that never arrived.

The greeting registers the session and outlives every task in it. Do not invent a task to carry
one, and do not tell a worker to disregard it once its current task is finished: the whole point
is that the session reports the owner's next assignment.

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

Track what you asked whom yourself. Nothing records that for you, and nothing withdraws a note
once it is delivered, so ask once and do not re-send the same request under a new wording.

## Integration

When several changes need integration, keep one ordered queue. Choose order from dependencies and
work already completed. Keep the current landing's base stable until it is integrated. Your own
maintenance waits behind that work. Tell affected workers the resulting dependency; independent
workers continue without a go round trip.

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

Once a task lands or is withdrawn, report the result once. A task that already finished has no
obligation to answer a late coordinator note.

## Runtime

`WB` is the absolute launch folder; pass it as `--dir` and keep it fixed when you inspect other
worktrees. Every command below is `cones coordinator`.

1. `cones coordinator --dir "$WB" claim` records you as the folder's coordinator and prints your
   session id and the folder's coordinator directory, which holds the inbox and everything that
   outlives this session. If another coordinator already owns the folder, it refuses: report that
   and stop. Mail that predates the claim is counted as handled and is not replayed.
2. `cones coordinator --dir "$WB" tick` is one read of everything you act on: HEAD, the tree, the
   roster and pending mail. The roster is what cones sees, and cones decides who is a worker: it
   drops unclaimed spares, resolves a Codex thread to its client, ignores viewer and daemon
   processes, and turns each harness's own report into one state. Do not re-derive any of that.
   A row carries its context use, so a worker near the end of its window gets a handoff rather
   than another note, and `unknown` means the harness never reported one, not room to spare.
   A reported `blocked` state is worth reading, not proof that you were asked something: the
   worker may be waiting on the owner. Answer only what is yours, a dependency or a finding.
3. Arm the watcher with a background Bash call: `cones coordinator --dir "$WB" wait`. It blocks
   without a model turn and returns for exactly two things, because only two things are worth a
   model call: a worker arrived and has not been shown to you, and a worker wrote. A departure, a
   state moving between active, idle and blocked, and an edit to the tree are facts to read from
   a tick once you are already awake. Waking for them spends a call to learn that somebody else
   is still working. Re-arm first after it returns, and capture its whole output.
4. Reach a worker with `cones coordinator --dir "$WB" send <id> "<text>"`, adding `--greet` for
   the once-per-session introduction; repeating a greeting is a no-op. It goes through that
   harness's own delivery command, and a harness with none is refused rather than approximated.
   When you are a Claude session yourself, a Claude worker is better reached with your native
   SendMessage, which is a conversation rather than a queued user turn. Either way the message
   spends the worker's time, and a sender label is attribution, not owner authority.
5. A worker with no native way to reply answers by appending one JSON line to the folder's inbox;
   `send` tells it the path and the shape. `cones coordinator --dir "$WB" mail` prints what is
   pending and consumes nothing, so a restarted or replaced coordinator sees replies the one
   before it never handled. `mail --ack N` records lines through N as handled, and only after you
   have acted on them. Reading is not handling.
6. On "stop orchestrator", stop the watcher and run `claim --release`. Leave the workers and their
   files alone. The owner can end your background session with `command claude stop <session-id>`.

If background watching is unavailable, report that; do not schedule a model heartbeat in its
place. Ask a missing owner decision once; existing owner instructions continue to apply.

See `docs/architecture.md` in the cones repository for what enforces each part.
