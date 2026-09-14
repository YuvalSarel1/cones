# Kinds of agent, and what cones may do with each

Back to the [README](../README.md). Where each column comes from is in [fleet.md](fleet.md); what a harness has to report is in [harness.md](harness.md).

Every row in `cones ls` and the dashboard is one of a few kinds of agent. The kind decides four things: whether the row is shown at all, whether `enter` opens it here, whether leaving it keeps it working, and how `x x` stops it. Each of those is decided by one fact the harness reports, never by a guess, and where the fact says no, cones refuses with the reason in the status line rather than running a command that will fail. The failure that fixed this rule: `enter` on an interactive Claude ran `claude attach`, which knows background jobs only, and Claude answered `No job matching`.

## The rule

The dashboard opens an agent only when leaving it keeps it working. An agent that is owned by a daemon (Claude's background daemon, Codex's app-server) can be joined and left: the viewer is a client, Ctrl+Z or Ctrl+C ends the client, the agent goes on. An agent that owns a terminal somewhere else (an interactive `claude`, a plain `codex` TUI) has no client protocol; joining it would mean stealing its tty or stopping it, and a stopped agent does no work, so cones does not open it. A headless run has no terminal at all; while it runs you follow its log, and once it has finished its session can be resumed as a background session and joined.

## The kinds

| Kind | Reported by | Shown | `enter` | Leaving it | `x x` | Row leaves when |
| --- | --- | --- | --- | --- | --- | --- |
| Claude background | registry entry, `kind: bg`; `claude --bg`, `claude agents`, the coordinator, a `cones tui` `n` launch | sessions table | `claude attach <short id>` in its cwd | Ctrl+Z detaches the viewer, the session keeps working | `claude stop <short id>`; a signal is not enough, the daemon respawns a killed worker | Claude removes the entry |
| Claude interactive | registry entry, `kind: interactive`; a `claude` typed in a terminal | sessions table, row and footer say `own terminal` | refused: "runs in its own terminal and cannot be joined from here"; `cones attach` refuses with the same reason while the pid is alive | n/a | SIGTERM on the registry pid, after `ps` confirms the pid still runs a `claude` binary | Claude removes the entry, or the pid is gone or reused |
| Claude spare | registry entry, `spare: true`; a warm worker the daemon keeps for the next `--bg` | no, as `claude agents` hides it | | | | |
| Claude crashed | registry entry whose pid is gone, or whose `ps` start time differs from the entry's `procStart` | no | | | | |
| Claude headless run, in flight | the cones ledger, `cones run`, status `started` | runs table | follows its log (`cones logs --follow`); Ctrl+C returns | n/a, nothing to leave | SIGTERM to the run's process group; the worker ends its tree the way the timeout does, SIGKILL after a grace period | it finishes |
| Claude headless run, finished | the cones ledger, a terminal status | runs table | `claude --bg --resume <session>` then `claude attach`, so the resumed session is a Claude background session and appears in the sessions table | Ctrl+Z detaches, the session keeps working | nothing to stop; the resumed session is stopped as a Claude background session | never; the ledger keeps it |
| Codex TUI | the process table: a live `codex` whose first argument is not a session-less subcommand | sessions table, row and footer say `own terminal` | refused, same message | n/a | SIGTERM on the pid, after `ps` confirms it still runs a `codex` binary | the process exits |
| Codex daemon thread | `~/.cones/codex-threads.json`, written when a `cones tui` `n` launch of Codex returns; `kind: daemon` | sessions table while no client process shows the same thread | `codex --remote unix://<socket> resume <thread id>` | Ctrl+Z kills the client, the thread keeps working in the daemon | forgets the record; the daemon has no stop, `codex resume` still has the thread | its rollout file is gone, or the record is forgotten |
| Codex service processes | `codex app-server`, `mcp-server`, `login`, `update`, `doctor` and the like | no; they run no session | | | | |
| Codex threads from other clients | the VS Code extension, another terminal's `--remote`, a thread picked in the dashboard's `h` resume picker and left | no; the daemon has no thread list yet | | | | |
| Subagents | the Agent tool inside a session | no; they run inside their parent's process and the registry has one entry per pid | | | | part of the parent row |

The dashboard's `h` key opens a harness's own agents view with no row picked: `claude agents`, or Codex's `resume` picker as a daemon client. What is opened from inside that view follows the harness's rules, not this table.

`cones attach <id>` at the shell follows the same table: a ledger run resumes when finished and refuses while in flight, a Claude background session attaches, a Claude interactive session or a Codex row is refused with the reason, and a session whose pid is gone but whose id is known resumes in the background and attaches.

## What decides, and what does not

| Decision | The fact | Not used |
| --- | --- | --- |
| Claude: join or refuse | registry `kind`, `bg` or `interactive` | whether the pid has a controlling tty, the parent process, the entry's `name` |
| Claude: `claude stop` or SIGTERM | registry `kind`; `bg` belongs to the daemon | the pid alone; a killed daemon worker is respawned |
| Claude: live or crashed | the pid runs and its `ps` start time equals `procStart` | the entry's `updatedAt`, the transcript's mtime |
| Codex: join or refuse | `kind: daemon` on a thread cones launched | the process command line; a `--remote` client started elsewhere is a TUI row and stays `own terminal` |
| Run: follow or resume | the ledger's terminal record | the registry; a headless run is not looked up there |

## Adding a kind

A new kind (a Claude worktree agent, a Codex `exec` run, another harness) needs an answer in every column above before it is a row. Each answer names a reported fact or is `-`, per [harness.md](harness.md). If the harness offers no way to join and leave the agent, the row is shown with `own terminal` and `enter` explains; if the harness offers no safe stop, `x x` says so. A best-effort join that sometimes works is not an option: it is the failure this document opened with.
