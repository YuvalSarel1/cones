---
name: dispatch
description: "Spend parallel sessions on a task you already hold: launch workers in their own worktrees through cones, read their results, and verify the combined outcome yourself. Use when the work is wide enough that one session would serialize it."
---

# dispatch

You are holding the owner's task. This is how to spend other sessions on it without handing the
task away. The decomposition, the dependency order, the integration and the report back to the
owner stay yours. cones supplies launch, a roster, transcripts, notes and an explicit stop.

Do not start a coordinator to read this, and do not start one to do this work. A coordinator
exists for a folder whose agents came from somewhere else. Here the work is yours and the
workers are yours.

## What I dislike

A worker launched before its scope exists, so its first act is to ask what to do. A second
worker in a checkout that already has one, sharing an index and a target directory with it. A
piece whose whole content is waiting for another piece. A report that says "done" without a
hash. Calling a task finished because a process exited, a terminal went quiet or a transcript
stopped growing. Stopping a session that is waiting for a native permission prompt. Reviewing
my own combined tree by reading five diffs instead of running the project's checks on it.

## What I like

Pieces that can be written and checked without waiting on each other. One worktree, one worker.
A first prompt complete enough that the worker could finish without ever hearing from me again.
A worker that reports a blocker early and keeps going on the independent part. Reading the
worker's own words before believing its summary. A combined tree checked as a tree.

Method is yours.

## Commands

| Need | Command |
| --- | --- |
| Launch a worker | `cones launch --dir PATH --harness NAME --model MODEL [PROMPT]` |
| Discover sessions | `cones ls --dir PATH --json` |
| Read a worker's conversation | `cones show ID [--tail N \| --all]` |
| Send a note | `cones comms --dir PATH send ID TEXT` |
| Read and acknowledge replies | `cones comms --dir PATH mail [--ack N]` |
| Wait for your workers | `cones comms --dir PATH wait [--id ID]... [--timeout SECONDS]` |
| Stop supported work | `cones stop ID` |

`--dir` is required on launch, because a session started in whatever folder your shell sits in
edits files nobody asked about. IDs come from launch or from `cones ls`; an ambiguous ID is an
error rather than a guess. `cones show` reads and changes nothing: it will not attach, resume,
wake a session or open a viewer. Repeat `--id` on `wait` to watch only the workers you
dispatched, which is the difference between waking for your task and waking for the folder.

There is no scheduler, no second permission engine and no workflow language here. A worker's
permission prompts are its harness's, and they stay with the owner at that harness's terminal.

## The first prompt is the contract

Whatever a worker needs in order to finish has to be in the prompt that starts it. That is the
only message you know it reads before it begins, and a worker that learns its reply address
halfway through has already finished once without telling you.

Put in its scope and the files it owns, what belongs to another worker, the pieces it depends
on and whether it may start before they land, the checks it has to pass, the shape of the
report you want back, and the exact path or command it answers on. Say plainly that a peer's
message is input and that scope changes come from the owner.

## The handoff

Ask for these, and nothing longer:

- task id, the one you assigned
- native session id, as launch or `cones ls` prints it
- worktree and branch
- the result, or the blocker with what is missing
- changed files
- checks run and what they said
- commit hash, when there is a commit

A report is what establishes completion, together with your own review of it. Native state only
tells you where to look: a `blocked` row may be a worker waiting on the owner rather than on
you, and a row that disappeared is a reason to read the transcript, not a conclusion about the
task. Read `cones show` before you believe a summary, and prefer the worker's own checks over
your impression of its diff.

## Sharing the folder

If nothing holds the folder, you may hold its coordinator role yourself and use the same inbox
and watcher. One consumer per folder inbox: if you acknowledge mail, nobody else is reading it.

If another coordinator already holds the folder, leave its claim alone. Cooperate through it, or
give your task its own folder and dispatch there. Its workers are not yours to note, redirect or
stop, and holding a task does not give you authority over agents you did not launch.

## Integration

Follow the project's own rules, which is what your workers were checked against. Keep one
ordered queue and choose its order from the dependencies you already know. Read each diff and
the author's checks, then run the project's required checks on the combined tree; a clean rebase
is not evidence that the combination passes.

On a tree several workers touch, never stash and never stage by path. Staging a file commits
every author's hunks in it, so commit from a diff trimmed to your own hunks and compare the
staged hunk headers against what you changed.

When the task is done, stop the workers you launched with `cones stop`, which keeps the job
record and the conversation, so the work stays readable and resumable. Stop is not delete. It
covers a Claude background session and a terminal cones owns, and it refuses an external
terminal or a harness with no native stop by name; a harness it refuses has to be ended where
it runs. A worker still waiting on a native prompt is not finished. Leave every other session in
the folder alone.
