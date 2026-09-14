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

The state is read in the order `claude agents` reads it for its own rows: a background job's `state` in `state.json` when it is `done`, `failed` or `stopped`; then the registry `status`; then a job whose `tempo` is `blocked`.

| State | Source | Dashboard label | Color |
| --- | --- | --- | --- |
| `done`, `failed`, `stopped` | job `state` | the word itself | green, red, dim |
| `active` | status `busy`, `shell` | working | plain |
| `blocked` | status `blocked`, `waiting`, `needs_user`, `needs_trust`; or job `tempo` `blocked` | needs input | yellow |
| `idle` | status `idle` | idle | dim |
| the word itself | any other status | the word itself | red |

Working is plain and green is kept for finished work, as in Claude's own agents view.

A registry entry is a live session only when its pid is running and the process start time `ps` prints under UTC equals the entry's `procStart`; a gone pid or a reused one is a crashed session and is skipped. An entry marked `spare` is a warm worker Claude's daemon keeps ready for the next `claude --bg`, not a session anyone started; it is skipped too, as `claude agents` skips it. There is no exited state: when a session ends Claude removes its entry and the row leaves the list. Those two are what the removed hook offered that the registry does not, an exited row that lingered for an hour and the name of the last hook event and tool; everything else the hook recorded comes from the registry or the transcript.

Stopping and attaching follow the session's owner; [kinds.md](kinds.md) has the table for every kind of row. A session whose kind is `bg` belongs to Claude's daemon, which respawns a killed worker, so `cones stop` ends it with `claude rm <short id>`, which also drops the job record that `claude agents` would otherwise keep listing as stopped (`claude stop` leaves it there); the transcript stays under `~/.claude/projects`, so `claude --resume <session>` still has the conversation; any other session gets SIGTERM on the registry pid after cones checks the pid still belongs to a `claude` binary. `cones attach` runs `claude attach <short id>` while the session's process is alive and its kind is `bg`; `claude attach` knows background sessions only, so an `interactive` session in someone's terminal is refused with that reason while it runs. Once the process is gone, cones resumes the session in the background (`claude --bg --resume <session>`) and attaches to it, so Ctrl+Z detaches and the session keeps running until it is exited or stopped. cones calls the `claude` binary by path, so a shell alias such as `claude='claude --dangerously-skip-permissions'` does not reach it; typing `claude stop <id>` yourself under that alias turns into a prompt.

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
| `transcript_path` | The rollout file; `cones logs` reads it |
| `kind`, `tokens_in`, `tokens_out`, `context_tokens`, `cost_usd` | Not shown for Codex |

`cones stop` on a Codex row sends SIGTERM to the pid after checking it still runs a `codex` binary.

### Codex behind its daemon: the rows the dashboard can open

Codex 0.154 has an experimental app-server daemon (`codex app-server daemon start`, idempotent, printing its `socketPath`), and its TUI can run as a client of it (`codex --remote unix://<socket> -C <dir>`). The thread then lives in the daemon: leaving the client (Ctrl+C twice, or Ctrl+Z, which the dashboard turns into a kill of the client) keeps the thread working, and `codex --remote unix://<socket> resume <thread id>` opens it again. That is how the dashboard's `n` launches Codex, so a Codex opened from the dashboard can be left and re-entered like a Claude background session.

The process table shows nothing about such a thread while no client is attached, and the daemon has no thread list yet, so cones keeps its own record of the threads it launched in `~/.cones/codex-threads.json`. After the client returns, the launch is matched to the newest rollout in its directory started since the launch; a thread left before its first turn is dropped by the daemon and is not recorded, and the status line says so. A recorded thread is a row with `kind` `daemon`, its state and last reply from the rollout's tail and its title from the index, hidden while a client process shows the same id. `enter` on it opens the resume client; `x` twice forgets the record (`codex resume` still has the thread). A row whose rollout file is gone is not shown. Codex threads started by other daemon clients (the VS Code extension, another terminal's `--remote`) are not listed until the daemon can enumerate them.

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

Stop and delete commands also run in the background, with their progress in the hint line.
A successful delete removes the row immediately. Returning from a harness or completing an
action requests fresh data and discards any read started before the transition.

For intermittent delays, run `cones tui --debug`, then summarize the log with
`python3 scripts/bench-tui.py --log ~/.cones/tui-debug.log`. The timings separate harness
commands, data reads, discarded snapshots, row rebuilding and the first draw after input
or returning from a harness. Time spent using a harness is excluded from return latency.

`python3 scripts/bench-tui.py target/release/cones --check --output /tmp/cones-bench.json`
measures repeated transitions through the real TUI in an isolated tmux server with fixture
harnesses. It exercises delayed deletions, failures, navigation during a command, normal
detach, a viewer stopped with Ctrl+Z, and a transcript read held across the transition.
It reports p50, p95 and maximum screen latency,
with separate harness acknowledgement and rendering times. `--transcript-mb 16` adds a
large transcript; `--runs` and `--sessions` change repetition and fleet size. These are local
regression budgets, not guarantees for real harness startup or filesystem performance.
The benchmark needs tmux and spends no model tokens. Nothing is installed into user sessions.

Under the composer, its hint line names only the keys that act on the selected row, then the ones that act everywhere: `enter <verb> · ctrl+x <stop|delete|forget> · ctrl+e edit · tab <harness> · ctrl+n new job · ctrl+s regroup · ctrl+o agents · esc quit`. The verb is `start job` on a job, `follow log` on a running run, `attach` on a session or a finished run and `own terminal` on a session that cannot be joined from here; `ctrl+x` reads `delete` on a job with no run in flight, `forget` on a Codex daemon thread, `delete` on a finished run and on a Claude background session, which `claude rm` removes from `claude agents` as well; `ctrl+e` shows on a job only; with nothing selected the line starts at `tab`, which names the harness it switches to. With an instruction typed it reads `enter start <harness> in <dir> · tab <harness> · esc clear`. The last action's status takes the line until the next key.

| Pane | Columns |
| --- | --- |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status |
| Sessions | icon, harness, `own terminal` on a session that cannot be joined from here (blank otherwise), title or short id, then the `columns:` list from [jobs.yaml](jobs.md#dashboard-columns): by default state, model, age, activity, context, last message or cwd |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason |

There is no details pane for now; a session is read by opening it, a run by `cones logs`. `cones ls --json` and `Data::details` still carry what the pane showed, so it can come back.

Each table opens with a dim row naming its columns, padded to the table beneath; the cursor skips it and `/` hides it while a filter is set. The sessions row sits once above the first directory group, since the groups share one table. The context cell reads `98k`: the prompt size Claude reported on the session's last message, with no window and no percentage, since Claude Code states the window size only in the statusLine payload. `age` counts from the transcript's first timestamp and `activity` from its last; neither reads the registry's `updatedAt` or the file's mtime, so a session that is idle shows a growing `activity` and a fixed `age`. Each of `model`, `age`, `activity` and `context` is `-` until the transcript holds the line it reads.

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top. Within a group they are ordered oldest first by start time, so a new session appends at the bottom and rows hold still; a session whose transcript reports no start sorts last, by id.

| Key | Action |
| --- | --- |
| `↑` `↓` | Move between rows. Every other plain key types into the composer at the bottom, so the actions are on ctrl, as in `claude agents`. |
| `enter` | With the composer empty: on a job, start a run in the background; on a running run, follow its log (Ctrl+C returns); on a finished run or a session, open it in this terminal, as described above, and Ctrl+Z comes back to the same row with the filter and grouping as they were; on a Codex thread the dashboard started (kind `daemon`), open a resume client on it; on a Codex TUI or an interactive Claude running in its own terminal, the row and the hint line say `own terminal` and `enter` explains. With an instruction typed: start a session with it, described under [the composer](#the-composer-a-session-in-the-selected-folder). |
| `tab` | The harness the next session starts under, `claude` or `>_ codex`; the composer's prefix shows it. |
| `ctrl+x` twice | Stop the selected run or session. On a Claude background session: `claude rm`, so the record leaves `claude agents` too and `claude --resume` still has the conversation. On a finished run: hide its row for good; the ledger keeps the run and `cones ls` still lists it. On a Codex daemon thread: forget its record, `codex resume` still has it. On a job: stop its run in flight; with none, delete the job from jobs.yaml and reinstall launchd. The first press marks the row red and stays armed until the second press; any other key keeps it. |
| `ctrl+n` | Add a job: the wizard, described under [the job wizard](#the-job-wizard-ctrln-ctrle-ctrlx). |
| `ctrl+e` | Edit the selected job in the same wizard, filled in from the file. |
| `ctrl+s` | Regroup sessions by state or by directory. |
| `ctrl+o` | Open a harness's own agents view without picking a row first: `claude agents`, or Codex's `resume` picker as a client of its app-server daemon, so a thread picked there keeps working when the client is left. `←` `→` pick the harness, `enter` opens, `esc` cancels. `esc` in Claude's agents view quits it and comes back here (`q` types into its prompt; Ctrl+Z comes back too). Its keys cannot be rebound to do this: Claude's `Agents` keybinding context accepts only its own two actions, and `~/.claude/keybindings.json` is global, not per launch. |
| `ctrl+f` | Filter rows by text; `enter` keeps the filter, `esc` clears it. A kept filter shows at the left of the hint line. |
| `ctrl+r` | Reload now; the dashboard reloads every second on its own, so the key is left off the hint line. |
| `esc` | Backs out one thing at a time: an armed `ctrl+x`, the typed instruction, then the dashboard. `ctrl+c` quits at once. |

Ctrl+Z never suspends the dashboard. Every child the dashboard hands the terminal to runs in its own process group and owns the tty, like a shell job, so Ctrl+C and Ctrl+Z reach it and whatever it forked, and nothing else. Inside `claude attach` Ctrl+Z drops the viewer and the session keeps running. A viewer that stops instead (Claude's agents view lets Ctrl+Z through as SIGTSTP) is killed on the spot, since it has already given the terminal back and the session it showed is untouched, so the dashboard is back at once either way. The rule behind all of this: the dashboard opens a session only when leaving it keeps it working. A stopped agent does no work, so nothing here parks a harness with SIGSTOP; where a harness cannot be left and re-entered, the dashboard says so and does not open it. Opening a session runs `claude attach <short id>` from the dashboard itself; a finished run goes through `cones attach`, which resumes it. Before the hand-off the dashboard blanks the normal screen, so the shell never shows while Claude starts or shuts down. It does not leave its last frame there: `claude attach` execs into `claude agents` on the left key and spends seconds on the normal screen while it starts, and a dead dashboard there would read as a live one. On the way back, and again when it quits, the dashboard resets the tty to what the shell had and switches off every mode a child turns on (mouse reports, focus events, bracketed paste, kitty keys), so a viewer killed on Ctrl+Z, which never gets to switch them off itself, cannot hand the shell a terminal that echoes garbage on every click and paste. The dashboard's state waits in memory meanwhile and on return it reloads and finds the row again by its id, so a session that went from idle to working while it was open, and so moved to another group, is still the selected row. A session that ended while open leaves the cursor on its neighbor. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger.

### The composer: a session in the selected folder

Above the hint line is a composer drawn like Claude Code's own input: ruled above and below, with a block cursor after what is typed or on the first letter of the placeholder, and it wraps and grows a line at a time (up to eight) as an instruction outruns the width, so nothing typed is cut off. Its prefix is the harness `tab` picked; type an instruction and `enter` starts a session with it in the selected row's directory (a job's `cwd`, a session's, a run's), or the dashboard's own with nothing selected, so starting work never depends on a session already being there. Claude starts as a background session (`claude --bg` with the instruction) on a thread, so the dashboard never waits; its row appears when Claude lists it, `enter` on the row attaches and Ctrl+Z comes back. Codex has no background mode, so it opens here as a thread of its app-server daemon with the TUI as a client, described under [Codex behind its daemon](#codex-behind-its-daemon-the-rows-the-dashboard-can-open); leaving the client keeps the thread and the dashboard records its id to resume it. A harness that cannot be left running is refused with the reason on the hint line and the instruction stays in the composer. There is no policy and no ledger here: it is the harness natively in that directory, with its own permission prompts. A supervised one-off run is `cones run --prompt` from a shell.

Under the composer, the hint line names the keys, or shows the last action's result until the next key.

### The job wizard: `ctrl+n`, `ctrl+e`, `ctrl+x`

Jobs are added, edited and deleted from the dashboard; the file stays yours to edit by hand too. `ctrl+n` asks four questions on the prompt line, each answered with `enter`; backspace on an empty answer goes back one, `esc` cancels.

| Question | Answer |
| --- | --- |
| `name ›` | 1-80 letters, digits, `-` or `_`; the launchd label is `local.cones.<name>`. |
| `dir ›` | A directory. `~` expands, a relative path is taken from the jobs file's directory, and the placeholder is the selected row's directory, or the dashboard's cwd with nothing selected; `enter` on an empty answer takes it. A path that is not an existing directory stays on the question with `not a directory: /path` inline. Stored in `~` form. |
| `schedule ›` | Five-field local-time cron, as in `0 9 * * 1-5`, checked the way `cones install` checks it. |
| `prompt ›` | The task. |

The job is a Claude job under the file's defaults; model, budget, tools and the rest are edited in the file. Saving rewrites only that job's block of jobs.yaml, found by its `- name:` line, so comments elsewhere and the other jobs' formatting survive; the whole file is validated first and a bad answer comes back inline with the file untouched. `ctrl+e` on a job row opens the same wizard filled in from the file, and saving keeps every field the wizard does not ask about. `ctrl+x` twice on a job with no run in flight removes its block. After each of these the dashboard runs `cones install`, so launchd matches the file; an install error shows on the hint line.
