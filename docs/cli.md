# Commands

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md)

Plain `cones` opens the dashboard. `launch` starts a session in a folder; `run` starts supervised work from the shell or launchd; `catchup` recovers missed schedules at login.

## Global flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--jobs PATH` | `~/.cones/jobs.yaml` | Configuration file. One per machine, like the state directory; the dashboard reads the same one from any folder. |
| `--state-dir PATH` | `~/.cones` | Relocate cones state, including stored runs and dashboard records. |
| `--debug` | off | Append [diagnostics](#diagnostics) to the state directory. |
| `--trace` | off | Enable debug diagnostics plus input text, commands and individual timing samples. |

## Native CLI lookup

Install and authenticate each CLI separately. cones searches `~/.local/bin`, `~/.cargo/bin`, `~/.opencode/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin`, in that order. Shell aliases and additional shell PATH entries are not used. Make executables installed elsewhere available in one of these directories. Config's connectivity check reports missing binaries or required flags.

## Commands

| Command | Effect |
| --- | --- |
| `cones` | Open the dashboard; requires a terminal. |
| `cones run JOB [--trigger manual\|schedule]` | Run a configured job. Trigger defaults to `manual`; launchd passes `schedule`. |
| `cones run --prompt "..." [JOB]` | Run a [one-off task](#one-off-tasks). |
| `cones launch --dir PATH [PROMPT] [--harness NAME] [--model ID] [--effort E] [--print-command]` | [Start a session](#starting-a-session) the way the dashboard's composer does. Supported harnesses detach and print an identifier; Codex stays in the terminal. |
| `cones catchup [--dry-run]` | Recover [missed schedules](jobs.md#sleep-login-and-reboot). `--dry-run` prints `name missed <local time>` for each candidate and starts nothing. |
| `cones ls [--dir PATH] [--job NAME] [--status S] [--json]` | [Read runs and live sessions](#reading-runs-and-sessions). |
| `cones show ID [--tail N] [--all]` | [Read a session's conversation](#reading-a-conversation). |
| `cones stop ID` | [Stop a session](#stopping-a-session) and keep its conversation. |
| `cones comms [--dir PATH] send\|mail\|wait ...` | [Write to the agents in a folder, read their replies and wait for one](#comms). |
| `cones skill [NAME]` | Print a [bundled skill](#dispatching-your-own-workers) for a session that is already running; no name lists them. |

### Reading runs and sessions

`cones ls` prints the dashboard's rows for a script: the ledger's runs, then the live sessions no run owns. Text timestamps use your local timezone and include its UTC offset. `--job` and `--status` narrow the read; naming a job leaves the sessions out, since a session belongs to no job.

`--dir` keeps the rows whose folder is that path or sits under it. This includes worktrees stored inside the directory; linked worktrees elsewhere are not included merely because they share a repository. Both sides are resolved first, so a folder reached through a symlink still matches. A row that reports no folder is not in any folder, so a scoped read leaves it out.

`--json` writes one object per line. `kind` is `run` or `session` and says which of the two shapes follows: `status`, `started` and `terminal` for a run; `status` and `session` for a session. Timestamps stay UTC and native model ids are preserved.

```sh
cones ls --dir ~/src/app --json
```

The [coordinator](#coordinator) reads its folder this way instead of walking the harness registries itself.

### Reading a conversation

```sh
cones show 4f0c2b1e-8d31-4a55-9f0c-6b2a17e4d900
cones show 4f0c2b1e --tail 5
cones show 4f0c2b1e --all
```

`cones show` prints a session's conversation as text. Each message is labelled `user`, `assistant` or `output`, followed by the harness's own timestamp in your local timezone, then the text, then the tool calls that turn recorded. Terminal control sequences are stripped: a transcript is data, never something your terminal runs.

The default is the last 40 messages. `--tail N` asks for a different count and `--all` for the whole conversation the harness kept. A read that left messages out starts with `[N earlier messages omitted; --all exports the whole conversation]`, so a bounded read is never mistaken for a full one. No individual message is shortened, which is where this differs from the dashboard's preview pane.

`ID` is the session id `cones ls` prints, or an unambiguous prefix of at least four characters. An exact id wins over a prefix that another session also starts with, and a prefix two sessions share is an error naming both. Sessions come from the same discovery the dashboard's history uses, so a finished session reads like a live one, and `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR` and the OpenCode home select which sessions exist.

Claude, Codex, pi and OpenCode keep conversations cones can read. A harness whose session lives only in its own terminal keeps none, so there is nothing to show and naming one is an error rather than an empty export.

Reading opens files for reading. It attaches to nothing, resumes nothing, starts no viewer and writes no native state, so a worker cannot tell that its conversation was read. A record the harness has not finished writing is left out, and the note saying so goes to stderr, so the conversation on stdout stays pipeable.

### Starting a session

```sh
cones launch --dir ~/src/app "fix the flaky test"
cones launch --dir ~/src/app --harness codex "rebase onto main"
cones launch --dir ~/src/app --harness claude --model opus --effort high
```

`--dir` is required. An explicit harness must be enabled. Otherwise cones uses enabled
`defaults.harness`, then the first enabled launchable harness; none is an error. Dashboard
`start.harness` and `_in_picker` settings do not affect CLI selection.

`--model` and `--effort` override defaults for this session only; unsupported flags fail.
Other unset settings use [configured defaults](jobs.md#composer-harnesses). Omitting the prompt
opens the session waiting for input. `--print-command` prints folder, environment and command
without launching or recording anything.

Claude launches into its daemon; OpenCode, pi and experimental launchers use persistent hosts.
They print a discovered identifier on stdout, with its kind on stderr, and survive the launching
shell. Codex stays in the current terminal and prints no id. See [detached identities and
verification limits](harness.md#detached-launch) before using the returned id with other commands.

An unnamed launch exits nonzero without printing an id or stopping the started session.
Concurrent identical prompts receive separate owned identities. The [launch ledger](#diagnostics)
retains submissions and identification outcomes, including with debug off.

### One-off tasks

```sh
cones run --prompt "fix the flaky test"
cones run nightly-triage --prompt "summarize the failures"
```

The task uses the current directory and the named job's policy, otherwise the first job's.
Without a valid readable file or template job, it uses [built-in defaults](jobs.md#job-fields-and-defaults).
An unknown explicit job is an error. Each task gets an `adhoc-<8 hex>` name and independent
overlap admission.

### Stopping a session

```sh
cones stop 5bf8392e-17cb-405d-a400-22dfbda13472
```

Use the id from `cones ls --json`. Two targets are supported:

| Target | Operation |
| --- | --- |
| Claude background | Native `claude stop <short id>` in its recorded home. Repeated stop succeeds; job record and transcript remain. |
| Owned persistent terminal | Host stop, acknowledged after the client exits. Saved host ids and current discovered ids resolve by harness and live client PID. |

Ambiguous or unknown ids, unrelated external terminals and unsupported harness operations fail.
Codex has no supported per-thread stop; stopping its daemon would affect every thread.
The dashboard has separate [stop/removal controls](harness.md#native-actions).

## Coordinator

The optional [coordinator skill](../assets/coordinator/skills/start-coordinator/SKILL.md) handles
overlaps, relevant findings and integration. Task scope stays with the owner and workers.

### Launch

```sh
cones coordinator --dir ~/src/app start
```

The folder defaults to cwd; dashboard Ctrl+D uses the selection's folder. An existing live
coordinator is refused. Otherwise cones refreshes its embedded plugin in
`STATE_DIR/coordinator/plugin`, removes obsolete plugin files and starts background Claude
with `/cones:start-coordinator`. It installs nothing in the user's plugin directory.

Tell the coordinator `stop coordinator` to release its role. Its native session remains until
separately stopped.

### Commands

```sh
cones coordinator --dir ~/src/app claim [--release]
cones coordinator --dir ~/src/app tick
```

`claim` identifies the caller through its process chain and folder roster. Claiming another
live holder's folder or releasing somebody else's claim fails. `tick` prints HEAD, tree status,
roster context/cost and pending mail. Neither command calls a model. Legacy `coordinator send`,
`mail` and `wait` are aliases for `comms`.

## Comms

These commands also serve dispatchers without a coordinator. `--dir` defaults to cwd and
covers descendants under the same [folder filtering](#reading-runs-and-sessions) as `ls`.
Commands make no model calls, though delivery can cause a recipient's native turn.

```sh
cones comms --dir ~/src/app send SESSION_ID "text" [--greet]
cones comms --dir ~/src/app mail [--ack N]
cones comms --dir ~/src/app wait [--id SESSION_ID]... [--timeout SECONDS]
```

| Command | Behavior |
| --- | --- |
| `send` | Deliver through the roster recipient's native [message operation](harness.md#message-delivery). Missing operations fail; `--greet` sends at most once per session. Notes identify their sender. |
| `mail` | Read pending replies. Only `--ack N` marks them handled. |
| `wait` | Block for new roster sessions or mail. With repeated `--id`, watch named workers for native input, failure or departure, plus mail. Unknown worker ids fail. |

Wait events are announced once per condition. A worker event prompts inspection; its result
report establishes task completion. Timeout only ends the wait. Exit 2 means timeout, exit 3
means a competing watcher and permits retry; other refusals exit 1.

One live claim holder consumes a folder's inbox and only one watcher may be armed. Other
agents can send into it. Use that holder or a separate task folder instead of competing for
the inbox. [The wake loop](architecture.md#the-wake-loop) explains baseline and replacement behavior.

### State

`STATE_DIR/coordinator/folders/<sha256 of absolute folder>` holds `status.json`, `inbox.jsonl`,
`inbox.ack`, `wait.json`, `greeted.json`, `watcher.json` and `folder.lock`. State survives the
requesting session. Codex delivery requires the recipient home's daemon with `queue --thread`.

## Dispatching your own workers

```sh
cones skill
cones skill dispatch
```

Without a name, `skill` lists bundled skills; with one, it prints the embedded `SKILL.md`.
A running session can read these instructions without restarting. Refreshing a plugin does
not load it into existing sessions; `coordinator start` loads it only into the session it starts.
The skills contain prose and no helpers.

The [dispatch skill](../assets/coordinator/skills/dispatch/SKILL.md) covers launching owned
workers, collecting results, checking integration and stopping them. Decomposition remains
with the dispatcher. It may claim an unowned task folder; an existing coordinator requires
cooperation or a separate folder. Dispatch grants no authority over unrelated sessions.

## Diagnostics

`--debug` writes `STATE_DIR/tui-debug.log` as JSONL. Records carry schema `v`, UTC `timestamp`,
`pid`, `dashboard_id`, `level`, `event` and `data`. Related events share `operation_id`;
row events include native identity and discovery source when known.

| Events | Contents |
| --- | --- |
| `dashboard.started`, `dashboard.stopped` | Build identity, configuration, terminal state and exit reason. |
| `row.*`, `view.changed` | Row identity, selection, focus and status changes. |
| `input.*`, `terminal.*` | Shortcut routes, paste sizes and terminal dimensions. |
| `viewer.*` | Preparation, spawn, first output, focus, closure and failures with PID/timing. |
| `launch.*`, `action.*` | Launch/stop/removal requests and outcomes. |
| `refresh.*`, `load.failed`, `discovery.failed`, `configuration.*` | Read errors, stale snapshots and recovery. |
| `history.*`, `transcript.*` | Worker duration, reads, bytes, caches and discarded results. |
| `timing`, `timing.summary` | Slow operations and counts/mean/max every 30 seconds and on exit. Thresholds: 16 ms for drawing/input/pumping, 250 ms otherwise. |

`--trace` implies debug and adds every timing sample, ordinary input text, mouse events and
viewer commands. Both flags affect dashboard diagnostics only.

The log caps at 10 MiB and compacts to roughly the newest 5 MiB of complete lines. Oversized
records retain identity and a marked preview. File locking coordinates concurrent writers.
To read errors while skipping older text records:

```sh
jq -R 'fromjson? | select(.level == "error")' ~/.cones/tui-debug.log
```

`STATE_DIR/launches.jsonl` uses the same bound and records these requests even with debug off:

| Event | Data |
| --- | --- |
| `launch.submitted` | Dashboard/detached-CLI `operation_id`, harness, cwd and prompt, before launch. |
| `launch.identified` / `launch.unnamed` | Detached CLI resolved `session_id` or error; no `dashboard_id`. |
| `resume.submitted` | History resume request: source id in `data.operation_id`, saved title in `data.prompt`. The title is not sent as an instruction. |

Submissions do not prove success. Foreground Codex CLI launches write no recovery record.

## Internal commands

These implementation commands are hidden from help and may change with their callers.

| Command | Purpose |
| --- | --- |
| `__logs ID [--follow] [--raw]` | Captured output; this renderer does not handle pi message entries. |
| `__attach ID [--print-command]` | Attach or resume a finished run. |
| `__install [--dry-run]` | Install schedules; dry run prints plist XML including imported credentials. |
| `__list` | Render rows for subprocess callers. |
| `__worker --run-id ID` | Run the supervised worker. |
| `__terminal-host` | Own an interactive PTY through a private local socket. Arguments/environment arrive through an anonymous pipe. |

`STATE_DIR/terminals/` stores owned identities, row metadata and host locks, including detached
OpenCode reports. `attention.json` and `attention.lock` hold shared completion/read markers.
Neither stores raw terminal screens or launch environments.

Session JSON includes [cost_info](harness.md#cost-estimates) for reported or estimated cost,
with its source, coverage and pricing snapshot metadata.
