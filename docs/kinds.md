# Kinds of agent, and what cones may do with each

Back to the [README](../README.md). Where each column comes from is in [fleet.md](fleet.md); what a harness has to report is in [harness.md](harness.md).

Every row in `cones ls` and the dashboard is one of a few kinds of agent. The kind decides four things: whether the row is shown at all, whether `enter` opens it here, whether leaving it keeps it working, and how `ctrl+x` stops it. Each of those is decided by one fact the harness reports, never by a guess, and where the fact says no, cones refuses with the reason in the status line rather than running a command that will fail. The failure that fixed this rule: `enter` on an interactive Claude ran `claude attach`, which knows background jobs only, and Claude answered `No job matching`.

## The rule

The dashboard opens an agent only when leaving it keeps it working. An agent that is owned by a daemon (Claude's background daemon, Codex's app-server) can be joined and left: the viewer is a client the dashboard keeps running off-screen when Ctrl+Z leaves it, Ctrl+C ends it, and the agent goes on either way. An agent that owns a terminal somewhere else (an interactive `claude`, a plain `codex` TUI) has no client protocol; joining it would mean stealing its tty or stopping it, and a stopped agent does no work, so cones does not open it. A headless run has no terminal at all; while it runs you follow its log, and once it has finished its session can be resumed as a background session and joined.

## The kinds

| Kind | Reported by | Shown | `enter` | Leaving it | `ctrl+x` | Row leaves when |
| --- | --- | --- | --- | --- | --- | --- |
| Claude background | registry entry, `kind: bg`; `claude --bg`, `claude agents`, the coordinator, a session started from the `cones tui` composer | sessions table | `claude attach <short id>` in its cwd | Ctrl+Z leaves the client running inside the dashboard and `enter` returns to it; the client closes when a fourth viewer opens or the dashboard quits, the session keeps working either way | `claude rm <short id>`; a signal is not enough, the daemon respawns a killed worker, and `claude stop` leaves a stopped record in `claude agents` | Claude removes the entry; `claude rm` drops the job record too, the transcript stays |
| Claude interactive | registry entry, `kind: interactive`; a `claude` typed in a terminal | sessions table, row and footer say `own terminal` | refused: "runs in its own terminal and cannot be joined from here"; `cones attach` refuses with the same reason while the pid is alive | n/a | SIGTERM on the registry pid, after `ps` confirms the pid still runs a `claude` binary | Claude removes the entry, or the pid is gone or reused |
| Claude spare | registry entry, `spare: true`; a warm worker the daemon keeps for the next `--bg` | no, as `claude agents` hides it | | | | |
| Claude crashed | registry entry whose pid is gone, or whose `ps` start time differs from the entry's `procStart` | no | | | | |
| Claude headless run, in flight | the cones ledger, `cones run`, status `started` | runs table | follows its log (`cones logs --follow`); Ctrl+C returns | n/a, nothing to leave | SIGTERM to the run's process group; the worker ends its tree the way the timeout does, SIGKILL after a grace period | it finishes |
| Claude headless run, finished | the cones ledger, a terminal status | runs table | `claude --bg --resume <session>` then `claude attach`, so the resumed session is a Claude background session and appears in the sessions table | Ctrl+Z leaves the client running inside the dashboard and `enter` returns to it; the client closes when a fourth viewer opens or the dashboard quits, the session keeps working either way | twice hides the row for good, the first press marks the row; the ledger, output and transcript stay, and `cones ls` and `cones attach` still have the run | hidden with `ctrl+x`; its id is a line in `~/.cones/hidden`, delete the line to bring it back |
| Codex TUI | the process table: a live `codex` whose first argument is not a session-less subcommand. A `--remote … resume <thread id>` client whose thread the daemon does not hold is this kind too; the `resume` argument names its thread, so the row carries that thread's title, state and tokens | sessions table, row and footer say `own terminal` | refused, same message | n/a | SIGTERM on the pid, after `ps` confirms it still runs a `codex` binary | the process exits |
| Codex daemon thread | `~/.codex/thread-writer-locks/<thread id>.lock` open, per the kernel's file table, in the pid named by `~/.codex/app-server-daemon/app-server.pid`, whoever opened the thread: the `cones tui` composer, the VS Code extension, another terminal's `--remote`, the `ctrl+o` picker. For a Codex without lock files (before 0.154), `~/.cones/codex-threads.json`, written when a composer launch returns. `kind: daemon` either way, including a live `--remote … resume <thread id>` client of it, in any terminal: Codex lets several clients share one thread | sessions table; the row carries the client's pid while one is attached | `codex --remote unix://<socket> resume <thread id>` | Ctrl+Z leaves the client running inside the dashboard and `enter` returns to it; the client closes when a fourth viewer opens or the dashboard quits, and the thread keeps working in the daemon | forgets the record when there is one; the daemon has no stop, so a lock-held thread's row stays while the daemon holds it and `codex resume` still has the thread | the daemon releases the lock (the thread is closed or archived) or exits; a recorded thread also leaves when its rollout is gone or the record is forgotten |
| Codex service processes | `codex app-server`, `mcp-server`, `login`, `update`, `doctor` and the like | no; they run no session | | | | |
| Subagents | the Agent tool inside a session | no; they run inside their parent's process and the registry has one entry per pid | | | | part of the parent row |
| Pinned folder | `~/.cones/folders`, one path per line, written when the menu's `folder` prompt takes a directory | sessions table, as its own directory group with one dim row while nothing runs there; a session in the folder takes the group over and the row returns when it leaves | explains: type an instruction, `enter` starts a session there | n/a, nothing runs | twice removes the folder from the dashboard, the first press marks the row; the directory itself is untouched | removed with `ctrl+x`; its line leaves `~/.cones/folders` |

The folder's coordinator is one of the Claude kinds above: background when `cones coordinator start` launched it, interactive when `/start-orchestrator` was typed in a terminal. Its row says `orchestrator`, in orange, because the skill's status file names its pid and folder (see [fleet.md](fleet.md#the-coordinator-one-session-per-folder)); everything else about it follows its kind's row.

The dashboard's `ctrl+o` key opens a harness's own agents view with no row picked: `claude agents`, or Codex's `resume` picker as a daemon client. What is opened from inside that view follows the harness's rules, not this table.

`cones attach <id>` at the shell follows the same table: a ledger run resumes when finished and refuses while in flight, a Claude background session attaches, a Claude interactive session or a Codex row is refused with the reason, and a session whose pid is gone but whose id is known resumes in the background and attaches.

## What decides, and what does not

| Decision | The fact | Not used |
| --- | --- | --- |
| Claude: join or refuse | registry `kind`, `bg` or `interactive` | whether the pid has a controlling tty, the parent process, the entry's `name` |
| Claude: `claude rm` or SIGTERM | registry `kind`; `bg` belongs to the daemon | the pid alone; a killed daemon worker is respawned |
| Claude: live or crashed | the pid runs and its `ps` start time equals `procStart` | the entry's `updatedAt`, the transcript's mtime |
| Codex: join or refuse | the thread's writer lock is held by the daemon's pid, or the thread is one cones recorded | the process command line; a `--remote` client shown in another terminal is joined, not refused, as the daemon takes a second client |
| Codex: which thread a process runs | the lock it holds, else its `resume <thread id>` argument | the cwd-and-start match, used only when the process states neither |
| Codex: daemon thread live or gone | the lock is open in the daemon's file table | the lock file's existence or mtime, the rollout's mtime, `updated_at` in the threads table |
| Run: follow or resume | the ledger's terminal record | the registry; a headless run is not looked up there |

## Adding a kind

A new kind (a Claude worktree agent, a Codex `exec` run, another harness) needs an answer in every column above before it is a row. Each answer names a reported fact or is `-`, per [harness.md](harness.md). If the harness offers no way to join and leave the agent, the row is shown with `own terminal` and `enter` explains; if the harness offers no safe stop, `ctrl+x` says so. A best-effort join that sometimes works is not an option: it is the failure this document opened with.
