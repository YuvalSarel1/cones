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

`cones --version` prints the version; Help shows it beside its title. Once a day, for as long as it runs, the dashboard reads the Homebrew tap's formula from GitHub. When it names a newer release, a Homebrew install runs `brew upgrade cones` in the background and the footer and Help show `cones <version> installed · reopen cones`; the dashboard keeps running its own version until then. Any other install shows `cones <version> available`. If the upgrade fails, the footer offers `brew upgrade cones` and the dashboard tries again an hour later. [`start.update_check`](jobs.md#start) turns the check off. The formula is kept as [`formula.rb`](jobs.md#stored-files).

## Native CLI lookup

Install and authenticate each CLI separately. cones searches `~/.local/bin`, `~/.cargo/bin`, `~/.opencode/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin`, in that order. Shell aliases and additional shell PATH entries are not used. Make executables installed elsewhere available in one of these directories. Config's connectivity check reports missing binaries or required flags.

## Commands

| Command | Effect |
| --- | --- |
| `cones` | Open the dashboard; requires a terminal. |
| `cones run JOB [--trigger manual\|schedule]` | Run a configured job. Trigger defaults to `manual`; launchd passes `schedule`. |
| `cones run --prompt "..." [JOB]` | Run a [one-off task](#one-off-tasks). |
| `cones launch --dir PATH [PROMPT] [--harness NAME] [--model ID] [--effort E] [--name NAME] [--print-command] [--no-prompt-record] [--wait [--timeout S]]` | [Start a session](#starting-a-session) the way the dashboard's composer does. The session runs detached, needs no terminal, and its identifier is printed. |
| `cones resume ID`, `cones fork ID` | [Reopen or fork a conversation](#resuming-and-forking) from history, detached, and print its id. |
| `cones catchup [--dry-run]` | Recover [missed schedules](jobs.md#sleep-login-and-reboot). `--dry-run` prints `name missed <local time>` for each candidate and starts nothing. |
| `cones ls [--dir PATH] [--job NAME] [--status S] [--json]` | [Read runs and live sessions](#reading-runs-and-sessions). |
| `cones search QUERY [--dir PATH] [--harness NAME] [--json]` | [Search conversation history](#searching-conversations), including archived sessions. |
| `cones index [status] [--timeout SECS] [--json]` | [Build the meaning index](#building-the-meaning-index) with progress, or report it. |
| `cones show ID [--tail N] [--all] [--json]` | [Read a session's conversation](#reading-a-conversation). |
| `cones mcp` | Serve [history and control tools over MCP stdio](#cones-over-mcp). |
| `cones wait ID... [--timeout S]` | [Block until a session settles](#waiting-on-a-session) and print `ID<TAB>STATE`. |
| `cones send ID TEXT` | [Send one note](#sending-a-note) to a live session in any folder. |
| `cones rename ID NAME` | [Name a session](#renaming-a-session) through its harness's own rename, as the dashboard's `ctrl+n` does. |
| `cones btw ID QUESTION [--model ID] [--timeout S]` | [Ask a side question](#asking-a-side-question) in a read-only fork the harness keeps nothing of, and print the answer. |
| `cones compact ID` | [Compact a session's context](#compacting-a-session) through its harness's own compaction. |
| `cones stop ID` | [Stop a session](#stopping-a-session) and keep its conversation. |
| `cones rm TARGET` | Do to a run, session, job or folder what the dashboard's [`ctrl+x` twice](#removing-a-row) does. |
| `cones comms [--dir PATH] send\|mail\|wait ...` | [Write to the agents in a folder, read their replies and wait for one](#comms). |
| `cones config [--check]` | Print the `jobs.yaml` path, editing notes and the full [configuration reference](jobs.md), for you or an agent to edit the file directly. `--check` validates the file. |
| `cones config set KEY VALUE`, `cones config unset KEY` | Set one [setting](jobs.md) by dotted path, such as `columns`, `highlight` or `defaults.model`, or remove it to restore the built-in. VALUE is YAML, so a list is `'[state, bypass, model]'`. Only that top-level block is rewritten; a value the file would refuse leaves it unchanged. Jobs are not settable here. |
| `cones skill [NAME]` | Print a [bundled skill](#bundled-skills) for a session that is already running; no name lists them. |

### Reading runs and sessions

`cones ls` prints the dashboard's rows for a script: the ledger's runs, then the live sessions no run owns. A run the dashboard hid is left out. Text rows are tab-separated: id, job or folder, status, start time, harness, cost, then a run's reason or a session's tokens. The column names go to stderr, so stdout is rows only, and the model is in `--json`. Text timestamps use your local timezone and include its UTC offset. `--job` and `--status` narrow the read; naming a job leaves the sessions out, since a session belongs to no job.

`--dir` keeps the rows whose folder is that path or sits under it. This includes worktrees stored inside the directory; linked worktrees elsewhere are not included merely because they share a repository. Both sides are resolved first, so a folder reached through a symlink still matches. A row that reports no folder is not in any folder, so a scoped read leaves it out.

`--json` writes one object per line. `kind` is `session` or `run` and says which of the two shapes follows: `status` and `session` for a session; `status`, `started` and `terminal` for a run. A run that started a session is printed as that session, with the run under `run`: its `run_id`, `job`, `status`, `started` and `terminal`. The row's own `status` is then the live session's state, or the run's status once the session is gone, when `session` holds only its id, harness and folder. `kind: run` is left for runs that never started a session: skipped, refused or failed before the launch. Timestamps stay UTC and native model ids are preserved. A pi, OpenCode or other process-identified session is `<harness>-<pid>` in `session_id`, with the conversation it reports, when it reports one, in `native_id`.

```sh
cones ls --dir ~/src/app --json
```

The [coordinator](#coordinator) reads its folder this way instead of walking the harness registries itself.

### Searching conversations

```sh
cones search "permission decision" --dir ~/src/app --json
cones search "why did we change the parser" --mode meaning --json
cones search "parser" --harness codex --since 2026-09-01T00:00:00Z --limit 10 --json
```

`search` uses the dashboard's history discovery and search index for Claude, Codex,
pi and OpenCode. It includes archived conversations and readable transcripts of
live sessions. It does not start or resume a harness. Word search is the default
and never loads a model. `--mode meaning` uses the same local MiniLM model as the
dashboard, downloading it on first use. Conversation text and queries stay local.

`--dir` keeps the folder and its descendants, resolving symlinks where the paths
still exist. `--harness` and `--home` select a harness and one of its configured
native homes. `--since` requires an RFC 3339 timestamp and compares the harness's
last recorded activity; a session with no activity timestamp cannot match it.
These filters apply before pagination. They do not remove other projects from
the search cache.

`--limit` is 1 to 100, default 20; `--offset` skips that many matching sessions.
Every request refreshes discovery. Pages reflect the current search, so new
messages can change their ordering between calls.

`--json` writes one object containing `query`, `mode`, `entries`, `total`, `offset`,
`next_offset`, `complete`, `pending`, `status` and `error`. Each entry carries its
`key` (`harness`, canonical native `home`, `session_id`), `cwd`, transcript source,
archive flag and native timestamps when available. Its `hit` contains the excerpt,
preview anchor, rank and whether the match was semantic. An empty completed search
has `entries: []`, `total: 0` and `complete: true`. Pass the key's three fields to
`show` to identify the exact conversation, even when native homes contain the same
session id.

Semantic search waits up to `--wait-seconds` for embeddings, default 30 and maximum
60. This bounds the wait between search passes, not a filesystem read or inference
already in progress. If indexing is unfinished, the command returns the matches
available with `pending: true`, `complete: false` and exit 2. Repeat the query to
continue from cached embeddings. A model failure is explicit in `error`, leaves
`complete: false`, and exits 1; word search remains available. Completed searches
exit 0. Invalid arguments or unreadable archives exit nonzero with an explanation
on stderr. Only cones' search cache is written, never native session files.

### Building the meaning index

```sh
cones index status
cones index
cones index --json --timeout 600
```

Meaning search embeds passages only while someone waits on it, so a large history
can stay partly indexed. `cones index` embeds every remaining passage of every
conversation, including archived ones, and returns when none is left. Finished
batches are kept, so stopping it with Ctrl-C or `--timeout` loses nothing and the
next run resumes.

`cones index status` reads changed transcripts and reports the counts without
loading a model or embedding anything.

Both print the same status, as one summary line or with `--json` as one object:
`phase`, `conversations`, `passages`, `embedded`, `remaining`, `rate` and
`eta_seconds` (passages per second and time left, once a batch has finished),
`model_downloaded`, `cache`, `cache_bytes`, `status` and `error`. Passages are
distinct texts; identical passages share one embedding.

While running, `cones index` redraws a progress bar on stderr when stderr is a
terminal. Piped, it writes one plain line per change to stderr. With `--json` it
writes one status object per change to stdout instead, and the last line is the
final status; an agent can read those lines to report progress. `phase` is
`syncing`, `downloading_model` (the first batch, until the 90 MB model is present),
`embedding`, `waiting`, `done` or `failed`.

Processes sharing a state directory embed one batch at a time. While another
dashboard, search or `cones index` holds the current batch, this one reports
`waiting` and continues when that batch is saved.

Exit status is 0 when every passage is embedded, 2 when some remain (after a
timeout, or from `status`), and 1 when the model fails, with the error in `error`
and on stderr.

### Reading a conversation

```sh
cones show 4f0c2b1e-8d31-4a55-9f0c-6b2a17e4d900
cones show 4f0c2b1e --tail 5
cones show 4f0c2b1e --all
cones show 4f0c2b1e --harness claude --home ~/.claude --json
```

`cones show` prints a session's conversation as text. Each message is labelled `user`, `assistant` or `output`, followed by the harness's own timestamp in your local timezone, then the text, then the tool calls that turn recorded. Terminal control sequences are stripped: a transcript is data, never something your terminal runs.

The default is the last 40 messages. `--tail N` asks for a different count and `--all` for the whole conversation the harness kept. A read that left messages out starts with `[N earlier messages omitted; --all exports the whole conversation]`, so a bounded read is never mistaken for a full one. No individual message is shortened, which is where this differs from the dashboard's preview pane.

`ID` is the session id `cones ls` prints, or an unambiguous prefix of at least four characters. An exact id wins over a prefix that another session also starts with, and a prefix two sessions share is an error naming both. Sessions come from the same discovery the dashboard's history uses, so a finished session reads like a live one, and `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR` and the OpenCode home select which sessions exist.

Claude, Codex, pi and OpenCode keep conversations cones can read, and so do Copilot and Cursor Agent [once saved](harness.md#additional-terminal-harnesses). A live row listed by its process, such as `pi-PID`, reads the conversation `cones ls --json` names as its `native_id`. A session with none, in a terminal cones hosts, is read as that terminal's screen: the first line is `[terminal screen of HARNESS ID, COLSxROWS; a screen, not a conversation: ...]` and the lines under it are what the client last drew, with no roles, timestamps or tool calls. The default is the screen as it is now, `--tail N` is the last N lines and `--all` adds what scrolled off, up to the 1000 lines the host keeps; a client on its alternate screen has no scrollback. `--json` returns `session`, `cwd` and `terminal` (`rows`, `cols`, `alternate`, `omitted`, `lines`) and no `messages`. The host answers without attaching, so a dashboard showing the same terminal keeps it. A session with no conversation in a terminal cones does not own has nothing to show, and naming one is an error rather than an empty export.

Reading opens files for reading. It attaches to nothing, resumes nothing, starts no viewer and writes no native state, so a worker cannot tell that its conversation was read. A record the harness has not finished writing is left out, and the note saying so goes to stderr, so the conversation on stdout stays pipeable.

`--harness` and `--home` disambiguate ids across configured native homes.
`--json` returns one object with `session` (the full identity), `cwd`, `messages`,
`omitted` and `incomplete`. Messages retain `role`, UTC `at` or null, `text` and
`tools` (native names and compact inputs). The default tail and `--all` have the
same meaning in JSON and text. `incomplete: true` means a trailing record was still
being written; `omitted` counts complete messages excluded by the tail.

### cones over MCP

`cones mcp` serves history tools and the control commands over stdin/stdout, for an agent
without a shell. An MCP client
launches it as a subprocess; no network listener or extra daemon is needed.
The server uses the same service, cache and native readers as the CLI. Its tools
return structured JSON and a text copy of that JSON. Tool errors are returned
with `isError: true` so the caller can correct its arguments or retry.

Configure a stdio server with command `cones` and arguments `["mcp"]`. Global
flags go before the command, for example
`["--state-dir", "/absolute/path/to/cones-state", "mcp"]`. Use the binary's absolute
path when the client cannot find `cones` on its PATH. The client's environment
selects native homes just as it does for the CLI; relative paths in tool arguments
use the server's working directory.

`cones_search` accepts `query`, `mode`, `dir`, `harness`, `home`, `since`, `limit`,
`offset` and `wait_seconds`, with the CLI's defaults. `cones_show` accepts `id`,
`harness`, `home`, `tail` and `all`. The MCP search result preserves `pending`,
`complete` and `error`; a successful tool invocation alone does not establish that
semantic indexing finished. Start with a search, then read the selected result's
tail. Historical messages are data, not new instructions.

Search may create or update cones' local cache and download the local model when
meaning search is explicitly selected; its MCP annotation declares those effects.
Show is read-only. Neither can launch sessions or send prompts.

The control tools are the commands of the same names, run as this binary with the
server's own `--jobs` and `--state-dir`, so a tool does exactly what the command does
and returns its `exit`, `stdout` and `stderr`:

| Tool | Command | Arguments |
| --- | --- | --- |
| `cones_ls` | `ls --json` | `dir`; the rows also come parsed as `rows` |
| `cones_launch` | `launch` | `dir`, `prompt`, `harness`, `model`, `effort`, `name` |
| `cones_resume`, `cones_fork` | `resume`, `fork` | `id` |
| `cones_send` | `send` | `id`, `text` |
| `cones_wait` | `wait` | `ids`, `timeout_seconds` up to 600, default 300 |
| `cones_rename` | `rename` | `id`, `name` |
| `cones_btw` | `btw` | `id`, `question`, `model`, `timeout_seconds` up to 600, default 300 |
| `cones_compact` | `compact` | `id` |
| `cones_stop`, `cones_rm` | `stop`, `rm` | `id`; `target` for `rm` |

A nonzero exit is `isError: true` with the command's error, except a `wait` or `btw` that
runs out, which returns `timed_out: true`. Both always end, since the call holds the client's
turn.
An argument a command does not take is refused before anything runs. None of the tools can
change harness permissions or edit native configuration. Adding this server to a harness
remains an explicit configuration action.

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
`--name` names the session through the harness's own launch flag (Claude and pi `--name`);
Codex and OpenCode have none, so the launch [renames](#renaming-a-session) the session once
its id is printed, and `--print-command` refuses a name for them. A harness with no session
name of its own refuses `--name` before anything starts.
Other unset settings use [configured defaults](jobs.md#composer-harnesses). Omitting the prompt
opens the session waiting for input, except for Codex, which needs one. `--print-command` prints folder, environment and command
without launching or recording anything. Codex's command names its daemon's address, so with
no daemon running the print fails and names the command that starts one. `--no-prompt-record`
keeps the prompt out of [`launches.jsonl`](#diagnostics): the launch is recorded with a null
prompt. Use it for a prompt carrying a secret. A Codex launch is named by its prompt, so one
overlapping an unrecorded launch in the same folder is reported unnamed rather than guessed.

Claude and Codex launch into their daemons; OpenCode, pi and experimental launchers use
persistent hosts. Each prints a discovered identifier on stdout, with its kind on stderr, and
survives the launching shell, so no terminal is needed. See [detached identities and
verification limits](harness.md#detached-launch) before using the returned id with other commands.

`--wait` keeps the command running after it prints the id, until the harness reports that the
session stopped moving on its own: its turn ended, it asks for input, it failed or stopped, or
it left the roster. A turn that ended beside a running background shell, state `shell`, still
counts as moving, because the shell's exit wakes it. It then prints `ID<TAB>STATE`. A state reported before the first turn does
not count; the session must have been seen working or have written since the launch. An agent
runs it as a background task, so the command's exit wakes it. The state says where to look,
not that the task is done. `--timeout S` gives up after S seconds and exits 2 with `timeout`,
leaving the session running. A pi session watched by its process id, before its session file
names it, can read as `gone`.

An unnamed launch exits nonzero without printing an id or stopping the started session. A
hosted client that exits during its launch also exits nonzero with no id, and the error quotes
its last screen; one still alive on a sign-in or trust screen is named, with that screen on
stderr. See [detached launch](harness.md#detached-launch) for what is and is not caught.
Concurrent identical prompts receive separate owned identities, except for Codex, where they
are refused. The [launch ledger](#diagnostics)
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

### Resuming and forking

```sh
cones resume 5bf8392e
cones fork 5bf8392e
```

Both take an id from `cones search` or `cones ls`, or an unambiguous prefix, and run the
command the dashboard runs for that row in a persistent cones terminal, so a dashboard can
open it later. Neither takes a prompt; `cones comms send` gives the session work.

- **Resume** prints the conversation's own id. A conversation that is already running, in
  the roster, a cones terminal or a live run, is refused. A row hidden by `cones rm` comes
  back. Resume fails if the client exits within its first seconds.
- **Fork** runs the harness's native fork and prints the fork's id, a
  [kind](#starting-a-session) as `launch` reports it. Once the fork has a native id, its
  parent is recorded and the dashboard lists it under that parent. A harness with no verified
  native fork is refused. A Codex fork is a thread its daemon makes and names, so no terminal
  is hosted for it; `cones send` gives it work and the dashboard joins it.

### Waiting on a session

```sh
cones wait 5bf8392e-17cb-405d-a400-22dfbda13472 0199f2a4-7c1e-7000-8000-00000000a11d
```

Blocks until one of the named live sessions is not working: its turn ended, it asks for input,
it failed or stopped, or it left the roster. It then prints `ID<TAB>STATE` for that session.
A session already settled answers at once. An id that is not live on the first look is
refused. Ids are exact, as `cones ls` prints them; a job's session counts, though `ls` shows
it as its run. A session whose harness reports no state reads as working and never settles.
`--timeout S` gives up after S seconds and exits 2 with `timeout`, leaving every session
running. An agent runs it as a background task, so the exit wakes it; the state says where to
look, not that the task is done. `launch --wait` uses the same watch, but ignores a state
reported before the new session's first turn.

### Sending a note

```sh
cones send 5bf8392e "rebase onto main when the tests pass"
```

Delivers one note to the live session the id names, wherever it works, through its harness's
native [message operation](harness.md#message-delivery); a harness without one is refused. The
id is one `cones ls` prints, or a prefix of four or more characters. The note names its
sender: a session cones can place is named with a line telling the recipient to answer with
`cones send` to it, and anything else is "another agent, not the owner". There is no folder,
roster, greeting or inbox; [`cones comms`](#comms) adds those for agents sharing a folder.

### Stopping a session

```sh
cones stop 5bf8392e-17cb-405d-a400-22dfbda13472
```

Use the id from `cones ls --json`. Four targets are supported:

| Target | Operation |
| --- | --- |
| Claude background | Native `claude stop <short id>` in its recorded home. Repeated stop succeeds; job record and transcript remain. |
| Codex daemon thread | The daemon interrupts its running turn and closes its background terminals, with or without a client attached. A quiet thread succeeds with nothing to stop. |
| Owned persistent terminal | Host stop, acknowledged after the client exits. Saved host ids and current discovered ids resolve by harness and live client PID. |
| Headless run | A client with no terminal, such as `codex exec` or `claude -p` under a dispatcher: its process is checked to be that harness's, sent `SIGTERM` and seen to exit. Its conversation remains. |

Ambiguous or unknown ids, unrelated external terminals and unsupported harness operations fail.
See [detached launch](harness.md#detached-launch) for what a Codex stop leaves in place.
The dashboard has separate [stop/removal controls](harness.md#native-actions).

### Renaming a session

```sh
cones rename 5bf8392e "Fix the flaky test"
```

`ID` is an id from `cones ls` or an unambiguous prefix of at least four characters. The name
must be one line. Each harness is named the way its own rename does it:

| Target | Operation |
| --- | --- |
| Claude background | `/rename NAME` typed into a client attached only for it, as `ctrl+n` hands it over. The command returns once the transcript records the name, and the running session keeps it. |
| Other Claude session | A `custom-title` record appended to its transcript, which outranks the `agent-name` a running session writes later. |
| Codex thread | The daemon's `thread/name/set`, which Codex's `/rename` calls, loaded or not. Clients showing the thread see the new name. |
| pi or OpenCode client cones owns | The reporter calls pi's `setSessionName` or OpenCode's `session.update`, which their own rename commands call. |

A client cones does not own, a harness with no session name of its own (Gemini, Cursor Agent,
Copilot, Amp, Droid, Kimi) and an ambiguous or unknown id fail without changing anything.

### Asking a side question

```sh
cones btw 5bf8392e "which test did you skip, and why?"
```

Asks the conversation `ID` names one question and prints the answer. The question goes to a
fork of the conversation, run headless under the harness's own read-only setting, and the
fork is not kept, so the conversation's context, transcript and history are untouched. It
works on a live session and on one from history, and starts nothing that keeps running. `ID`
is one `cones search` or `cones ls` prints, or a prefix of one.

| Harness | Fork | Read-only setting | What is left |
| --- | --- | --- | --- |
| Claude Code | `claude -p --resume ID --fork-session --no-session-persistence` | `--permission-mode plan` | Nothing is saved. |
| Codex | `codex exec fork --ephemeral` | `codex exec --sandbox read-only` | Nothing is saved. |
| pi | `pi --print --fork TRANSCRIPT --session-dir DIR` | `--tools read,grep,find,ls` | pi refuses `--no-session` with `--fork`, so the fork is saved in a private directory cones removes. |
| OpenCode | `opencode run --session ID --fork --format json` | `--agent plan` | OpenCode saves every fork; cones deletes the one its events name with `opencode session delete`, never the conversation it came from. |

Each runs in the conversation's folder and native home. The owner's skip-permissions setting
is never added. The fork answers on the configured default model, or on `--model`, through
the harness's own model flag. A fork on another model than the conversation's reads the whole
context without the prompt cache. `--timeout S` ends the fork after S seconds and exits 2 with
`timeout`; OpenCode's fork is still deleted. An error the harness reports is printed and
exits 1. Other harnesses have no verified fork that keeps nothing, and are refused. Verified
against Claude Code 2.1.302, Codex 0.163.0 on a loopback provider, pi 0.85.1 and OpenCode
1.18.31: Claude Code and Codex on a live session, Claude Code, pi and OpenCode on one from
history. The shape of an OpenCode error event is unverified.

### Compacting a session

```sh
cones compact 5bf8392e
```

Compacts a live session's context the way its harness's own `/compact` does, and returns once
the harness reports the compaction done, within ten minutes. A session whose turn is running
is refused; [`cones wait`](#waiting-on-a-session) says when it ends.

| Target | Operation |
| --- | --- |
| Claude background | `/compact` typed into a client attached only for it, as [rename](#renaming-a-session) types `/rename`. Done when the transcript records Claude Code's `compact_boundary`. |
| Codex thread | The daemon's `thread/compact/start`, which Codex's `/compact` calls, after `thread/resume` loads a thread the daemon is not holding. Done when the compaction's turn is listed completed. |
| pi or OpenCode client cones owns | The reporter calls pi's `compact` or OpenCode's `session.summarize` on the model of the session's last reply, which their own `/compact` calls, and answers when it returns. |

A Claude session in a terminal cones cannot join, a pi or OpenCode client cones does not own,
and the harnesses with no compaction of their own (Gemini, Cursor Agent, Copilot, Amp, Droid,
Kimi) are refused without changing anything. Verified against the same versions as side
questions, Codex on a thread its daemon had dropped. A session waiting on a question or
permission dialog is unverified; Claude's shows no empty editor and the command fails after
20 seconds.

### Removing a row

`cones rm TARGET` is the dashboard's `ctrl+x` twice on the row TARGET names: a job's name, an
open folder's path (anything with a `/` or starting with `~`), or a run or session id or
unambiguous prefix from `cones ls`.

A session is stopped first, then removed from the harness's own list. If the stop fails,
nothing is removed and the command exits 1. Runs and jobs belong to the
[jobs client](architecture.md#jobs), folders to the dashboard.

| Target | Operation |
| --- | --- |
| Running run | Stopped, recorded `stopped`, and hidden. |
| Finished run | Hidden; output and ledger record remain. |
| Running job | Its run stopped and hidden; the job stays. |
| Idle job | Deleted from `jobs.yaml`, the rest of the file as written, then schedules reinstalled. |
| Cones terminal | Host and client stopped. |
| Claude background | [`cones stop`](#stopping-a-session), then native `claude rm`, which takes it off `claude agents`, as the [dashboard](harness.md#native-actions) runs it; the transcript remains. |
| Codex daemon thread | Stopped through its daemon when one runs, then forgotten and hidden, its client closed; the thread stays resumable from history. |
| Other session | SIGTERM to the process, once it is verified to be that harness's. The harness keeps no list to remove it from, and the output says so. |
| Open folder | Closed; the directory and any pin remain. A running dashboard drops the row on its next refresh. |

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

These commands also serve dispatchers without a coordinator. `--dir` defaults to cwd. The
folder's roster is every live session working in the folder itself or under it in the same
repository, which includes subfolders of the checkout and worktrees stored inside it, whoever
launched it; the sender need not be on it. A subfolder that is not part of the folder's
repository, such as a separate project under a hub folder, is not on it, unlike `cones ls --dir`. A session id may be an unambiguous prefix of four or more characters, as with `show`.
Commands make no model calls, though delivery can cause a recipient's native turn.

```sh
cones comms --dir ~/src/app send SESSION_ID "text" [--greet]
cones comms --dir ~/src/app mail [--ack N]
cones comms --dir ~/src/app wait [--id SESSION_ID]... [--timeout SECONDS]
```

| Command | Behavior |
| --- | --- |
| `send` | Deliver through the roster recipient's native [message operation](harness.md#message-delivery). Missing operations fail; `--greet` sends at most once per session. Notes identify their sender. A note from a session cones can place tells the recipient to answer with `send` to that session, so the reply arrives as a prompt. An exact id reaches a live session outside the folder, such as that sender or the agent that launched a worker; a prefix names only a roster session. A note from anywhere else is answered through the folder's inbox. A Claude sender writing to another Claude session is refused: SendMessage is its native channel. |
| `mail` | Read pending replies, from workers that could not answer with `send`. Only `--ack N` marks them handled. |
| `wait` | Block for new roster sessions or mail. With repeated `--id`, watch named workers for native input, failure or departure, plus mail. Unknown worker ids fail. |

A Claude session that bypasses permission prompts holds notes for its user's approval; see
[message delivery](harness.md#message-delivery) for the setting that delivers them directly.

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

## Bundled skills

```sh
cones skill
cones skill cones
```

Without a name, `skill` lists bundled skills; with one, it prints the embedded `SKILL.md`.
A running session can read these instructions without restarting. Refreshing a plugin does
not load it into existing sessions; `coordinator start` loads it only into the session it starts.
The skills contain prose and no helpers.

The [cones skill](../assets/coordinator/skills/cones/SKILL.md) tells any agent what cones can do for it,
where to find the commands, how to run its own workers on a task it holds and how to change
settings. Workers it launches report into its conversation, because nothing else wakes it. The dashboard installs it on start into
`skills/cones/SKILL.md` under the Claude Code home and `~/.agents/skills`, which Codex, pi and
OpenCode read. `STATE_DIR/skill/cones/SKILL.md` records the installed text: a copy still equal
to it follows upgrades, while one edited or deleted afterwards, or present before the first
install, is left alone.

The [start-coordinator skill](../assets/coordinator/skills/start-coordinator/SKILL.md) runs a
folder's coordinator; `coordinator start` loads it.

## Diagnostics

`--debug` writes `STATE_DIR/tui-debug.log` as JSONL. Records carry schema `v`, UTC `timestamp`,
`pid`, `dashboard_id`, `level`, `event` and `data`. Related events share `operation_id`;
row events include native identity and discovery source when known.

| Events | Contents |
| --- | --- |
| `dashboard.started`, `dashboard.stopped` | Build version and release commit, configuration, terminal program, tmux or screen, terminal state and exit reason. |
| `dashboard.build` | SHA-256 of the running executable, logged once it is hashed so startup does not wait for it. |
| `row.*`, `view.changed` | Row identity, selection, focus and status changes. |
| `input.*`, `terminal.*` | Shortcut routes, paste sizes, terminal dimensions, colors and accepted keyboard flags. |
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
| `launch.submitted` | Dashboard/detached-CLI `operation_id`, harness, cwd and prompt, before launch. The prompt is null under `--no-prompt-record`. |
| `launch.identified` / `launch.unnamed` | Detached CLI resolved `session_id` or error; no `dashboard_id`. |
| `resume.submitted` | History resume request: source id in `data.operation_id`, saved title in `data.prompt`. The title is not sent as an instruction. |

Submissions do not prove success.

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
