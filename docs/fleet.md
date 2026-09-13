# The fleet and the dashboard

Back to the [README](../README.md). Commands are in [cli.md](cli.md).

## The fleet: every Claude and Codex session on the Mac

```sh
cones ls --status blocked        # sessions waiting on a permission, trust or user prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

Claude Code sessions come from Claude's own registry, Codex sessions from the process table and Codex's rollout files; both are described below. Codex rows are seen, not driven: `cones ls`, the dashboard, `cones logs` and `cones stop` work on them, `cones attach` refuses (Codex has no attach command), and no budget, job, token or dollar figure applies.

Nothing is installed. Claude Code keeps a registry of its own sessions, one `~/.claude/sessions/<pid>.json` per live session, interactive or background, written and updated by Claude itself. Every `cones ls` and every dashboard refresh reads that directory, or `$CLAUDE_CONFIG_DIR/sessions` when that variable is set, the same override Claude honors, and fills the rest of the row from the session's transcript under `~/.claude/projects`. No hook runs inside the session and `~/.claude/settings.json` is untouched. If an earlier cones put its hook there, `cones doctor` warns until the entries whose command ends in ` hook $PPID` are deleted; with the hook command gone from the binary each would fail on every event.

Every Claude Code session on the Mac appears in `cones ls` with its working directory, state, start time, harness, dollars and tokens in/out; the dashboard adds the title, model, age, last activity, context and last message. Sessions that belong to a cones run collapse into that run's row. Dollars come from the ledger for cones runs; for other sessions the column stays `-`, since the transcript records tokens and no price.

Every value is a line Claude wrote, and the table names the line. Nothing is read from settings, a model name, a threshold or a file's mtime. A value Claude did not write is absent: `-` in a cell, omitted from `cones ls --json`, never estimated.

| Field | Source |
| --- | --- |
| `session_id`, `pid`, `cwd`, `kind` | The registry entry: `sessionId`, `pid`, `cwd` and `kind`, which is `bg` for a session Claude's daemon owns and `interactive` otherwise |
| `state` | The registry `status`, mapped as below |
| `started` | The `timestamp` on the first transcript line that carries one. The registry `startedAt` is not read for it |
| `last_activity` | The `timestamp` on the last transcript line that carries one. The registry `updatedAt`, the job's `updatedAt` and the transcript file's mtime are not read |
| `model` | `message.model` on the last transcript message with `usage`, verbatim: the bare API id, `claude-fable-5-1`. A message whose model is `<synthetic>` is Claude's placeholder for a turn no model answered (all-zero usage) and is skipped |
| `transcript_path` | `~/.claude/projects/<cwd with every non-alphanumeric byte as ->/<session_id>.jsonl`, or the job's `linkScanPath` when that file is missing |
| `title` | Claude's `ai-title`, or a user-set `agent-name`, read from the transcript tail; the registry `name` when the transcript has neither |
| `last` | For a background job, the one-line `detail` Claude keeps in the job's `state.json`; otherwise the first line of the assistant's most recent text |
| `tokens_in`, `tokens_out` | Summed over every transcript message with `usage`, once per message id, recounted when the file grows; input includes cache reads and cache creation. Absent until the first such message |
| `context_tokens` | The prompt size on the same message `model` comes from: `input_tokens` plus `cache_creation_input_tokens` and `cache_read_input_tokens`, the fields Claude's statusLine `current_usage` carries. There is no window field: Claude Code states the window size only in its statusLine payload, which reaches nothing outside the session, so the cell has no denominator and no percentage |

| State | Registry `status` | Dashboard label |
| --- | --- | --- |
| `active` | `busy`, `shell` | working |
| `idle` | `idle` | idle |
| `blocked` | `blocked`, `waiting`, `needs_user`, `needs_trust` | needs input |
| the word itself | any other value | the word itself |

A registry entry is a live session only when its pid is running and the process start time `ps` prints under UTC equals the entry's `procStart`; a gone pid or a reused one is a crashed session and is skipped. An entry marked `spare` is a warm worker Claude's daemon keeps ready for the next `claude --bg`, not a session anyone started; it is skipped too, as `claude agents` skips it. There is no exited state: when a session ends Claude removes its entry and the row leaves the list. Those two are what the removed hook offered that the registry does not, an exited row that lingered for an hour and the name of the last hook event and tool; everything else the hook recorded comes from the registry or the transcript.

Stopping and attaching follow the session's owner. A session whose kind is `bg` belongs to Claude's daemon, which respawns a killed worker, so `cones stop` ends it with `claude stop <short id>`; any other session gets SIGTERM on the registry pid after cones checks the pid still belongs to a `claude` binary. `cones attach` runs `claude attach <short id>` while the session's process is alive; once it is gone, cones resumes the session in the background (`claude --bg --resume <session>`) and attaches to it, so Ctrl+Z detaches and the session keeps running until it is exited or stopped. cones calls the `claude` binary by path, so a shell alias such as `claude='claude --dangerously-skip-permissions'` does not reach it; typing `claude stop <id>` yourself under that alias turns into a prompt.

### Codex: the process table and the rollout file

Codex keeps no session registry. A Codex row starts from the process table: `TZ=UTC ps -axww -o pid=,lstart=,command=` names every live process whose program is `codex` and whose first argument is not a subcommand that runs no session (`app-server`, `mcp-server`, `login`, `update`, `doctor` and the like), then one `lsof -a -p <pids> -d cwd` call gives each its working directory. The word `codex` inside another command's text is not a process. That already makes a row: pid, cwd, start time, harness `>_ codex`.

The rest comes from the rollout file Codex writes for the session, `~/.codex/sessions/YYYY/MM/DD/rollout-<local start>-<session id>.jsonl` (the Codex home is `.codex` beside the Claude dir, `~/.codex` next to `~/.claude`; `$CODEX_HOME` relocates it, the same override Codex honors; no Codex home means Codex is not installed and the process table is not read). Codex opens that file on the session's first turn, so a Codex that has been started and not yet asked anything has none. Its first line, `session_meta`, records the session id, `cwd` and a `timestamp`; a rollout belongs to a live process when that process is the only Codex in the rollout's directory that started at or before the rollout's timestamp, and the newest such rollout is the live thread, since `/new` opens another. With two Codex processes in one directory the file could be either's, so neither takes it. A resumed session (`codex resume`) appends to its old rollout, whose start predates the process, so it matches nothing. Only rollouts modified since the oldest live Codex started are opened; that mtime prunes the scan and is shown nowhere. Every unmatched field shows `-`; nothing is estimated.

| Field | Source |
| --- | --- |
| `pid`, `started`, `cwd` | The process table: `ps` pid and `lstart` under UTC, `lsof` cwd |
| `session_id` | The rollout's `session_meta.session_id`; `codex-<pid>` for a process with no rollout, so `logs` and `stop` can still name the row |
| `title` | `thread_name` for the session id in `~/.codex/session_index.jsonl`, the name Codex gives a thread |
| `last` | First line of the last assistant `output_text` in the rollout |
| `state` | The rollout's last turn event: `active` after `task_started`, `idle` after `task_complete` or `turn_aborted`, `-` with no rollout. Codex records no needs-input event there, so a turn waiting on an approval reads as working |
| `last_activity` | The `timestamp` of the rollout's last line; absent with no rollout, the process start does not stand in for it |
| `model` | `turn_context.model` on the rollout's last turn, verbatim, such as `openai.gpt-6-astra` |
| `transcript_path` | The rollout file; `cones logs` and the details pane read it |
| `kind`, `tokens_in`, `tokens_out`, `context_tokens`, `cost_usd` | Not shown for Codex |

`cones stop` on a Codex row sends SIGTERM to the pid after checking it still runs a `codex` binary.

## The coordinator: one session per folder

Coordination between agents sharing a tree is not cones logic. It is the [start-orchestrator](https://github.com/YuvalSarel1/orchestrator) skill: one Claude Code session that finds every agent whose cwd is the folder, introduces itself, holds commits until it says go, relays findings and insists on a clean tree when the last job ends. cones owns schedule and ledger; the coordinator owns the conversation. cones ships the skill inside its binary, from `assets/coordinator/`, so nothing needs installing.

```sh
cones coordinator start            # this folder
cones coordinator start ~/src/app  # another folder
```

Each start rewrites the plugin under `~/.cones/coordinator/plugin` (or the `--state-dir`), then runs `claude --bg --plugin-dir <that> /cones:start-orchestrator` in the folder, so the coordinator is an ordinary background session loaded with the skill for that session only: it shows in `cones ls` and the dashboard, `claude attach <id>` opens it, and telling it "stop orchestrator" ends its role. The skill writes `~/.claude/orchestrator/<sha1 of the folder>.json` with its pid and peers every tick, the same file a hand-typed `/start-orchestrator` from an installed copy of the skill writes, so `cones coordinator start` and the skill's own guard both see a coordinator started either way: when that file names a live process for the folder, the command prints it and does nothing.

The copy under `assets/coordinator/` is the upstream skill with one line changed, the helper path, which cones fills in when it writes the plugin. Update it by copying the upstream files over and re-applying that line.

## The dashboard: jobs, sessions and runs on one screen

`cones tui` reloads about every second, on a thread of its own so a slow transcript read never holds a keypress or the spinner, and reads `N working · N need input · N idle · N jobs · N runs` on its summary line.

Its hint line reads `↑↓ move · enter <verb> · tab peek · x x stop · e edit jobs · s regroup · n new task · / filter · r refresh · q quit`, where the verb is `start job` on a job, `follow log` on a running run, `attach` on a session or a finished run, and `open` with nothing selected; `tab peek` reads `tab more` while the pane is showing and `tab hide` while it is expanded.

| Pane | Columns | Details pane |
| --- | --- | --- |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status | schedule, policy line, prompt |
| Sessions | icon, harness, title or short id, then the `columns:` list from [jobs.yaml](jobs.md#dashboard-columns): by default state, model, age, activity, context, last message or cwd | model, start, last activity, context and tokens on one line, then the last prompt and full reply; with `tab`, the last 12 exchanges |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason | captured output and harness stderr |

The details pane starts hidden, so the list has the screen. `tab` shows it as the bottom 40% of the screen, where it shows the end of its text; a second `tab` expands it to 75% and, on a session, reads the last 12 prompts and replies from the transcript instead of the last one, so a session can be read before it is opened; nothing is shown that the transcript does not record, so a turn that was all tool calls shows its prompt alone. `pgup` and `pgdn` (or `shift+↑` `↓`) page through the text; the pane title then reads `lines 41-80 of 120`. The pane stays pinned to the end until it is paged up, so a working session keeps scrolling by itself, and moving to another row or pressing `tab` again pins it back. A third `tab` hides the pane again.

Each table opens with a dim row naming its columns, padded to the table beneath; the cursor skips it and `/` hides it while a filter is set. The sessions row sits once above the first directory group, since the groups share one table. The context cell reads `98k`: the prompt size Claude reported on the session's last message, with no window and no percentage, since Claude Code states the window size only in the statusLine payload. `age` counts from the transcript's first timestamp and `activity` from its last; neither reads the registry's `updatedAt` or the file's mtime, so a session that is idle shows a growing `activity` and a fixed `age`. Each of `model`, `age`, `activity` and `context` is `-` until the transcript holds the line it reads.

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top. Within a group they are ordered oldest first by start time, so a new session appends at the bottom and rows hold still; a session whose transcript reports no start sorts last, by id.

| Key | Action |
| --- | --- |
| `↑` `↓`, `k` `j` | Move between rows. |
| `enter`, `→`, `a` | On a job: start a run in the background. On a running run: follow its log (Ctrl+C returns). On a finished run or a session: open it in this terminal, as described above; Ctrl+Z comes back to the same row, with the filter and grouping as they were. |
| `tab` | Show the details pane, hidden by default; `tab` again expands it and, on a session, shows the last 12 exchanges; a third `tab` hides it. |
| `pgup`, `pgdn` (or `shift+↑`, `shift+↓`) | Page the details pane up towards the start of the text and back down to its end. |
| `x` twice (or `ctrl+x` twice) within two seconds | Stop the selected run or session. On a job: stop that job's run in flight; with none, the status line says so. |
| `e` | Open jobs.yaml in `$VISUAL` or `$EDITOR`, then run `cones install` on return so launchd matches the file; an install error shows on the status line. |
| `ctrl+s` (or `s`) | Regroup sessions by state or by directory. |
| `n` | New task, in any folder: four questions on the footer line, described below. `esc` cancels at any of them. |
| `/` | Filter rows by text; `enter` keeps the filter, `esc` clears it. |
| `r` | Reload now. |
| `esc`, `q`, `ctrl+c` | Quit. |

Ctrl+Z never suspends the dashboard; inside an attached session it detaches and returns here. A viewer that stops instead (Claude's agents view lets Ctrl+Z through as SIGTSTP) is killed on the spot, since it has already given the terminal back and the session it showed is untouched, so the dashboard is back at once either way. A harness launched with `n` or the `e` editor is the real thing, not a viewer: Codex and vi both stop on Ctrl+Z, so the dashboard resumes them at once and Ctrl+Z is a no-op there; quit them normally to return. Opening a session runs `claude attach <short id>` from the dashboard itself; a finished run goes through `cones attach`, which resumes it. Before the hand-off the dashboard paints its last frame on the normal screen and erases it on the way back, so the shell never shows while Claude starts or shuts down. The dashboard's state waits in memory meanwhile and on return it reloads and finds the row again by its id, so a session that went from idle to working while it was open, and so moved to another group, is still the selected row. A session that ended while open leaves the cursor on its neighbor. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger.

### `n`: launch in any folder

Starting work never depends on a session already being there: `n` works on an empty fleet and never runs in the dashboard's own cwd unless you say so. Each answer is `enter`; backspace on an empty answer goes back one question.

| Question | Answer |
| --- | --- |
| `dir ›` | A directory. `~` expands, a relative path is taken from the dashboard's cwd, and the placeholder is the selected row's directory (a job's `cwd`, a session's, a run's), or the dashboard's cwd with nothing selected; `enter` on an empty answer takes it. A path that is not an existing directory stays on the question with `not a directory: /path` inline. |
| `harness ›` | `←` `→` (or `space`) pick between the harnesses cones knows, `claude` and `>_ codex`. |
| `start ›` | `interactive` or `managed` (`←` `→`, or `i` / `m`). Interactive suspends the dashboard, runs the harness natively in the directory as typing its name in a shell there would, with the harness's own permission prompts, and comes back when it exits; while it runs the session shows in the fleet. Managed is a supervised one-off run under the read-only defaults or the first job's policy, so a harness with no adapter (Codex today) is refused here with the validation message rather than as a failed row. |
| `managed ›` | The task. `enter` dispatches it as `cones run --prompt` with the chosen directory as the subprocess's working directory; it appears under runs and in the ledger like any other. |
