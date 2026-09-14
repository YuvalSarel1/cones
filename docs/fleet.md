# The fleet and the dashboard

Back to the [README](../README.md). Commands are in [cli.md](cli.md).

## The fleet: every Claude and Codex session on the Mac

```sh
cones ls --status blocked        # sessions waiting on a permission, trust or user prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

Claude Code sessions come from Claude's own registry, Codex sessions from the process table, Codex's writer locks and its rollout files; both are described below. Codex rows are seen and joined, not run: `cones ls`, the dashboard and `cones logs` work on them, `cones stop` signals a Codex process (a thread the daemon holds with no client has no pid and no stop), the dashboard opens a thread the app-server daemon holds, `cones attach` at the shell refuses a Codex row, and no budget, job or dollar figure applies.

Nothing is installed. Claude Code keeps a registry of its own sessions, one `~/.claude/sessions/<pid>.json` per live session, interactive or background, written and updated by Claude itself. Every `cones ls` and every dashboard refresh reads that directory, or `$CLAUDE_CONFIG_DIR/sessions` when that variable is set, the same override Claude honors, and fills the rest of the row from the session's transcript under `~/.claude/projects`. No hook runs inside the session and `~/.claude/settings.json` is untouched. If an earlier cones put its hook there, `cones doctor` warns until the entries whose command ends in ` hook $PPID` are deleted; with the hook command gone from the binary each would fail on every event.

Every Claude Code session on the Mac appears in `cones ls` with its working directory, state, start time, harness, dollars and tokens in/out; the dashboard adds the title, model, age, last activity, context and last message. Sessions that belong to a cones run collapse into that run's row. Dollars come from the ledger for cones runs; for other sessions the column stays `-`, since the transcript records tokens and no price.

Every value is a line Claude wrote, and the table names the line. Nothing is read from settings, a model name, a threshold or a file's mtime. A value Claude did not write is absent: `-` in a cell, omitted from `cones ls --json`, never estimated.

| Field | Source |
| --- | --- |
| `session_id`, `pid`, `cwd`, `kind` | The registry entry: `sessionId`, `pid`, `cwd` and `kind`, which is `bg` for a session Claude's daemon owns and `interactive` otherwise. A background job's `cwd` is the launch directory from `~/.claude/jobs/<jobId>/state.json`, the folder `claude agents` files it under; the registry `cwd` follows the session into a worktree when it runs EnterWorktree, and the transcript path follows it too |
| `state` | The registry `status`, mapped as below |
| `started` | The `timestamp` on the first transcript line that carries one. The registry `startedAt` is not read for it |
| `last_activity` | The `timestamp` on the last transcript line that carries one. The registry `updatedAt`, the job's `updatedAt` and the transcript file's mtime are not read |
| `model` | `message.model` on the last transcript message with `usage`, verbatim: the bare API id, `claude-fable-5-1`. A message whose model is `<synthetic>` is Claude's placeholder for a turn no model answered (all-zero usage) and is skipped |
| `transcript_path` | `~/.claude/projects/<cwd with every non-alphanumeric byte as ->/<session_id>.jsonl`, or the job's `linkScanPath` when that file is missing |
| `title` | Claude's `ai-title`, or a user-set `agent-name`, read from the transcript tail; then the job's `name` in `state.json`, the title `claude agents` shows, or the registry `name`, unless either is just the job's 8-hex id or the session id, which Claude uses until it has a title; then the first line of the user's first instruction in the transcript, so a fresh session is named by what it was asked |
| `last` | For a background job, the one-line `detail` Claude keeps in the job's `state.json`; otherwise the first line of the assistant's most recent text |
| `tokens_in`, `tokens_out` | Summed over every transcript message with `usage`, once per message id, recounted when the file grows; input includes cache reads and cache creation. Absent until the first such message |
| `context_tokens` | The prompt size on the same message `model` comes from: `input_tokens` plus `cache_creation_input_tokens` and `cache_read_input_tokens`, the fields Claude's statusLine `current_usage` carries |
| `context_window` | `context_window.context_window_size` from the statusLine payload, the only place Claude Code states it. Only a statusLine command sees that payload, so cones reads it from `~/.claude/statusline/<session id>.json` when your statusLine command saves it there (one line, see [harness.md](harness.md)). Absent otherwise, and the cell shows the prompt alone |

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

### Codex: the process table, the writer lock and the rollout file

Codex keeps no session registry. A Codex row starts from the process table: `TZ=UTC ps -axww -o pid=,lstart=,command=` names every live process whose program is `codex` and whose first argument is not a subcommand that runs no session (`app-server`, `mcp-server`, `login`, `update`, `doctor` and the like), then the kernel names each one's working directory (`proc_pidinfo`, the call `lsof -d cwd` makes; `lsof` itself for a pid that refuses it). A whole-system `lsof` scan cost every refresh hundreds of milliseconds. The word `codex` inside another command's text is not a process. That already makes a row: pid, cwd, start time, harness `>_ codex`.

The rest comes from the rollout file Codex writes for the thread, `~/.codex/sessions/YYYY/MM/DD/rollout-<local start>-<thread id>.jsonl` (the Codex home is `.codex` beside the Claude dir, `~/.codex` next to `~/.claude`; `$CODEX_HOME` relocates it, the same override Codex honors; no Codex home means Codex is not installed and the process table is not read). Codex opens that file on the thread's first turn, so a Codex that has been started and not yet asked anything has none. Which thread a process runs is what the process states: the writer lock `~/.codex/thread-writer-locks/<thread id>.lock` it holds open (Codex 0.154 and later), else the `resume <thread id>` argument it was started with. That thread's rollout is the `rollout_path` in the `threads` table of `~/.codex/state_*.sqlite`, else the file under `sessions/` whose name ends in `-<thread id>.jsonl`. Only a process that states neither falls back to a match: the rollout's first line, `session_meta`, records `cwd` and a `timestamp`, and the rollout belongs to a live process when that process is the only Codex in the rollout's directory that started at or before the rollout's timestamp; the newest such rollout is the live thread, since `/new` opens another. With two Codex processes in one directory the file could be either's, so neither takes it. Only rollouts modified since the oldest live Codex started are opened; that mtime prunes the scan and is shown nowhere. Every unmatched field shows `-`; nothing is estimated.

| Field | Source |
| --- | --- |
| `pid`, `started`, `cwd` | The process table: `ps` pid and `lstart` under UTC, the kernel's cwd for that pid |
| `session_id` | The thread id the process states, else the rollout's `session_meta.session_id`; `codex-<pid>` for a process with no thread, so `logs` and `stop` can still name the row |
| `kind` | `daemon` when the thread's writer lock is held by the app-server daemon's pid, read from `~/.codex/app-server-daemon/app-server.pid`, so the row can be joined from here; absent for a plain TUI, which runs in its own terminal |
| `title` | `name`, else `title` (the first prompt), for the thread id in the `threads` table of `~/.codex/state_*.sqlite`, read with `sqlite3`; then `thread_name` in the legacy `session_index.jsonl`, for threads older than Codex's move to sqlite; then the rollout's first user message |
| `last` | First line of the last assistant `output_text` in the rollout |
| `state` | The rollout's last turn event: `active` after `task_started`, `idle` after `task_complete` or `turn_aborted`, `-` with no rollout. Codex records no needs-input event there, so a turn waiting on an approval reads as working |
| `last_activity` | The `timestamp` of the rollout's last line; absent with no rollout, the process start does not stand in for it |
| `model` | `turn_context.model` on the rollout's last turn, verbatim, such as `openai.gpt-6-astra` |
| `transcript_path` | The rollout file; `cones logs` reads it |
| `tokens_in`, `tokens_out` | `token_count.info.total_token_usage` on the rollout's last such event: `input_tokens` (cache reads included, as Codex counts them) and `output_tokens` |
| `context_tokens`, `context_window` | `token_count.info.last_token_usage.total_tokens` and `model_context_window` on the same event |
| `cost_usd` | Not shown: the rollout records tokens and no price |

`cones stop` on a Codex row sends SIGTERM to the pid after checking it still runs a `codex` binary. A daemon thread with no client attached has no pid and no stop; `ctrl+x` forgets cones's record of it instead, described below.

### Codex behind its daemon: the rows the dashboard can open

Codex 0.154 has an experimental app-server daemon (`codex app-server daemon start`, idempotent, printing its `socketPath`), and its TUI can run as a client of it (`codex --remote unix://<socket> -C <dir>`). The thread then lives in the daemon: leaving the client (Ctrl+C twice ends it; Ctrl+Z leaves it running inside the dashboard, where `enter` on its row returns to it) keeps the thread working, and `codex --remote unix://<socket> resume <thread id>` opens it again. That is how the composer starts Codex when `tab` has picked it, so a Codex opened from the dashboard can be left and re-entered like a Claude background session.

A thread the daemon holds is a row whoever opened it: the composer, the VS Code extension, another terminal's `--remote`, the `ctrl+o` picker. The daemon flocks `~/.codex/thread-writer-locks/<thread id>.lock` for as long as the thread is loaded, and the kernel's file table for the daemon's pid (`proc_pidinfo`; `lsof` for a pid that refuses it) names the threads it holds; a lock file whose holder is gone is not a row, and the file's existence or mtime is never read for it. Such a row has `kind` `daemon`, its state, last reply and tokens from the rollout's tail and its title from the threads table, and it carries the client's pid while one is attached. Codex lets several clients share one thread, so `enter` joins a thread another terminal shows rather than refusing it; `enter` opens the resume client. The row leaves when the daemon releases the lock (the thread is closed or archived) or exits; the daemon has no stop for a thread, so `ctrl+x` on a lock-held row says so.

For a Codex without lock files (before 0.154), cones keeps its own record of the threads it launched in `~/.cones/codex-threads.json`. After the client returns, the launch is matched to the newest rollout in its directory started since the launch; a thread left before its first turn is dropped by the daemon and is not recorded, and the status line says so. `ctrl+x` twice forgets the record (`codex resume` still has the thread); a recorded thread whose rollout file is gone is not shown.

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
A confirmed delete removes the row at once, before `claude rm` returns, and a failed one puts it back with the error; the hint line names the session by its title. Returning from a harness or completing an
action requests fresh data and discards any read started before the transition.

For intermittent delays, run `cones tui --debug`, then summarize the log with
`python3 scripts/bench-tui.py --log ~/.cones/tui-debug.log`. The timings separate harness
commands, data reads, discarded snapshots, row rebuilding, a viewer's spawn to its first
text and the first draw after input or returning from a viewer. Time spent inside a viewer
is excluded from return latency.

`python3 scripts/bench-tui.py target/release/cones --check --output /tmp/cones-bench.json`
measures repeated transitions through the real TUI in an isolated tmux server with fixture
harnesses. It exercises delayed deletions, failures, navigation during a command, leaving a
viewer with Ctrl+Z, and a transcript read held across the transition.
It reports p50, p95 and maximum screen latency,
with separate harness acknowledgement and rendering times. `--transcript-mb 16` adds a
large transcript; `--runs` and `--sessions` change repetition and fleet size. These are local
regression budgets, not guarantees for real harness startup or filesystem performance.
The benchmark needs tmux and spends no model tokens. Nothing is installed into user sessions.

Under the composer, its hint line names only the keys that act on the selected row, then the ones that act everywhere: `enter <verb> · ctrl+x <stop|delete|hide|forget> · ctrl+e edit · tab <harness> · ctrl+n new job · ctrl+s regroup · ctrl+o agents · esc quit`. The verb is `start job` on a job, `follow log` on a running run, `attach` on a session or a finished run, `return` on a row whose viewer is alive inside the dashboard and `own terminal` on a session that cannot be joined from here; `ctrl+x` reads `delete` on a job with no run in flight, `forget` on a Codex daemon thread, `hide` on a finished run and `delete` on a Claude background session, which `claude rm` removes from `claude agents` as well; `ctrl+e` shows on a job only; on a menu row the verb is `new job`, `agents` or `pick folder`. With an instruction typed it reads `enter start <harness> in <dir> · tab <harness> · ctrl+v paste image · esc clear`, or `enter run once in <dir>` on the menu's `runs` row. The last action's status takes the line until the next key.

| Pane | Columns |
| --- | --- |
| Menu | `runs`, `agents`, `folder`: three rows above the tables, described under [the menu](#the-menu-runs-agents-folder) |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status |
| Sessions | icon, harness, `own terminal` on a session that cannot be joined from here (blank otherwise), title or short id, then the `columns:` list from [jobs.yaml](jobs.md#dashboard-columns): by default state, model, age, activity, context, last message or cwd |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason |

There is no details pane for now; a session is read by opening it, a run by `cones logs`. `cones ls --json` and `Data::details` still carry what the pane showed, so it can come back.

Each table opens with a dim row naming its columns, padded to the table beneath. A column only grows for the life of the dashboard: a cell that changes length (`59s` to `1m`, `working` to `needs input`, a long title leaving) never moves the columns beside it, so a column that was once wide stays wide until the dashboard restarts. The cursor skips the naming row and `/` hides it while a filter is set. The sessions row sits once above the first directory group, since the groups share one table. The context cell reads `98k/200k`: the prompt size the harness reported on the session's last message over the window it stated, or `98k` alone when nothing stated a window (a Claude session whose statusLine command does not save its payload). `age` counts from the transcript's first timestamp and `activity` from its last; neither reads the registry's `updatedAt` or the file's mtime, so a session that is idle shows a growing `activity` and a fixed `age`. Each of `model`, `age`, `activity` and `context` is `-` until the transcript holds the line it reads.

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top. Within a group they are ordered oldest first by start time, so a new session appends at the bottom and rows hold still; a session whose transcript reports no start sorts last, by id.

| Key | Action |
| --- | --- |
| `↑` `↓` | Move between rows; `↑` past the first table lands on the menu. Every other plain key types into the composer at the bottom, so the actions are on ctrl, as in `claude agents`. |
| `enter` | With the composer empty: on a job, start a run in the background; on a running run, follow its log (Ctrl+C returns); on a finished run or a session, open it in this terminal, as described above, and Ctrl+Z comes back to the same row with the filter and grouping as they were; on a row whose viewer is still alive inside the dashboard, the verb reads `return` and `enter` shows its current screen in one frame; on a Codex thread the dashboard started (kind `daemon`), open a resume client on it; on a Codex TUI or an interactive Claude running in its own terminal, the row and the hint line say `own terminal` and `enter` explains; on a menu row, `runs` opens the `ctrl+n` wizard, `agents` opens the `ctrl+o` picker and `folder` asks for a directory. With an instruction typed: start a session with it, described under [the composer](#the-composer-a-session-in-the-selected-folder). |
| `tab` | The harness the next session starts under, `claude` or `>_ codex`; the composer's prefix shows it. |
| `ctrl+x` twice | Stop the selected run or session. On a Claude background session: `claude rm`, so the record leaves `claude agents` too and `claude --resume` still has the conversation. On a finished run: hide its row for good; the ledger keeps the run and `cones ls` still lists it. On a Codex daemon thread: forget its record, `codex resume` still has it. On a job: stop its run in flight; with none, delete the job from jobs.yaml and reinstall launchd. The first press marks the row red and stays armed until the second press; any other key keeps it. |
| `ctrl+n` | Add a job: the wizard, described under [the job wizard](#the-job-wizard-ctrln-ctrle-ctrlx). |
| `ctrl+e` | Edit the selected job in the same wizard, filled in from the file. |
| `ctrl+s` | Regroup sessions by state or by directory. |
| `ctrl+o` | Open a harness's own agents view without picking a row first: `claude agents`, or Codex's `resume` picker as a client of its app-server daemon, so a thread picked there keeps working when the client is left. `←` `→` pick the harness, `enter` opens, `esc` cancels. `esc` in Claude's agents view quits it and comes back here (`q` types into its prompt); Ctrl+Z comes back too and leaves the view running, so `ctrl+o` then `enter` on that harness returns to it. Its keys cannot be rebound to do this: Claude's `Agents` keybinding context accepts only its own two actions, and `~/.claude/keybindings.json` is global, not per launch. |
| `ctrl+f` | Filter rows by text; `enter` keeps the filter, `esc` clears it. A kept filter shows at the left of the hint line. |
| `ctrl+v` | Paste the clipboard's image into the instruction, as Claude Code does: it is written as a PNG under `$TMPDIR/cones/pasted-<time>.png` and its path is typed into the composer, where the harness reads it as a file. Text pastes arrive as one paste, into the composer, or into the focused viewer, and need no key. With no image on the clipboard the status line says so. macOS only, through `osascript`. |
| `ctrl+r` | Reload now; the dashboard reloads every second on its own, so the key is left off the hint line. |
| `esc` | Backs out one thing at a time: an armed `ctrl+x`, the typed instruction, then the dashboard. `ctrl+c` quits at once. Inside a viewer both are the viewer's keys; only Ctrl+Z is the dashboard's. |

Ctrl+Z never suspends the dashboard. A session, a finished run or an agents view opens as a viewer: a terminal the dashboard emulates and draws. The viewer runs on a private pseudo-terminal the dashboard owns, as its own session with the shell's terminal modes, sized to the frame and resized with it; its output goes to the emulator, and the dashboard draws the emulator's screen, cell for cell, as its own frame, with the terminal's cursor where the viewer put it. Nothing the viewer writes reaches the real terminal, so the shell never shows, not while the viewer starts and not while it shuts down, and no mode a viewer turns on is left on the terminal. Ctrl+Z is a focus change: the dashboard takes the frame back and redraws at once, the viewer stays alive and keeps parsing off-screen, and `enter` on its row returns to its current screen in one frame, with no second `claude attach` start to wait for. The three most recently used viewers are kept; when a fourth opens, the least recently focused closes; `ctrl+x` confirmed on a row closes its viewer first; and every viewer closes with the dashboard. The session a viewer showed is untouched in every case. The rule behind all of this: the dashboard opens a session only when leaving it keeps it working. A stopped agent does no work, so nothing here parks a harness with SIGSTOP, and a viewer that stops itself (SIGTSTP) is closed; where a harness cannot be left and re-entered, the dashboard says so and does not open it. Opening a session runs `claude attach <short id>` from the dashboard itself; a finished run goes through `cones attach`, which resumes it. A Codex client needs the daemon's socket first, so the command is prepared on a thread with `opening codex` in the hint line, `esc` cancels it and the instruction stays in the composer; the address is kept and probed, so the second open does not start the CLI again. The dashboard answers a viewer's terminal queries itself: cursor position, device attributes, and the default foreground and background colors, which it probes from the real terminal once at start so a viewer picks the same light or dark theme it would in a shell. It does not implement the kitty keyboard protocol, so keys reach a viewer in the classic xterm encoding; chords that encoding cannot express, shift+enter among them, arrive as their plain key. Pasted text arrives as one paste, bracketed when the viewer asked for that, and mouse reports are forwarded, relative to the pane, for as long as the viewer asks for them. The dashboard's state waits in memory meanwhile and on return it reloads and finds the row again by its id, so a session that went from idle to working while it was open, and so moved to another group, is still the selected row. A session that ended while open leaves the cursor on its neighbor. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger.

### The menu: `runs`, `agents`, `folder`

Three rows sit above the tables. A fresh dashboard opens on the first table, and `↑` from there lands on them. They act on the menu's folder: the directory the dashboard was started in until `folder` picks another, so work can start in a directory nothing runs in yet. `runs` turns the composer into a supervised one-off run: type an instruction and `enter` starts `cones run --prompt` with it in that folder, under the first job's policy, in the ledger like any other run; with nothing typed, `enter` opens the job wizard, the same as `ctrl+n`. `agents` opens the same harness picker as `ctrl+o`. `folder` asks for a directory on the prompt line, relative to the current folder with `~` expanded; `tab` completes it as a shell completes `cd`: one match fills in with a trailing `/`, several fill in what they share and a second `tab` lists them on the hint line, and hidden folders are offered only after a `.`; a path that is not a directory is refused on the hint line and the prompt stays. The `folder` row names the folder, and the composer on any menu row starts its session there.

### The composer: a session in the selected folder

Above the hint line is a composer drawn like Claude Code's own input: ruled above and below, with a block cursor after what is typed or on the first letter of the placeholder, and it is three rows tall, one of text between the rules, until an instruction outruns the width; then it wraps and grows a line at a time (up to eight), so nothing typed is cut off. Its prefix is the harness `tab` picked; type an instruction and `enter` starts a session with it in the selected row's directory (a job's `cwd`, a session's, a run's), or the menu's folder from a menu row, so starting work never depends on a session already being there. Claude starts as a background session (`claude --bg` with the instruction) on a thread, so the dashboard never waits; its row appears when Claude lists it, `enter` on the row attaches and Ctrl+Z comes back. Codex has no background mode, so it opens here as a thread of its app-server daemon with the TUI as a client, described under [Codex behind its daemon](#codex-behind-its-daemon-the-rows-the-dashboard-can-open); leaving the client keeps the thread and the dashboard records its id to resume it. A harness that cannot be left running is refused with the reason on the hint line and the instruction stays in the composer. There is no policy and no ledger here: it is the harness natively in that directory, with its own permission prompts. A supervised one-off run is the menu's `runs` row, or `cones run --prompt` from a shell.

Under the composer, the hint line names the keys, or shows the last action's result until the next key.

### The job wizard: `ctrl+n`, `ctrl+e`, `ctrl+x`

Jobs are added, edited and deleted from the dashboard; the file stays yours to edit by hand too. `ctrl+n`, or `enter` on the menu's `runs` row, asks four questions on the prompt line, each answered with `enter`; backspace on an empty answer goes back one, `esc` cancels.

| Question | Answer |
| --- | --- |
| `name ›` | 1-80 letters, digits, `-` or `_`; the launchd label is `local.cones.<name>`. |
| `dir ›` | A directory. `~` expands, a relative path is taken from the jobs file's directory, and the placeholder is the selected row's directory, or the dashboard's cwd with nothing selected; `enter` on an empty answer takes it. `tab` completes it as the `folder` prompt does. A path that is not an existing directory stays on the question with `not a directory: /path` inline. Stored in `~` form. |
| `schedule ›` | Five-field local-time cron, as in `0 9 * * 1-5`, checked the way `cones install` checks it. |
| `prompt ›` | The task. |

The job is a Claude job under the file's defaults; model, budget, tools and the rest are edited in the file. Saving rewrites only that job's block of jobs.yaml, found by its `- name:` line, so comments elsewhere and the other jobs' formatting survive; the whole file is validated first and a bad answer comes back inline with the file untouched. `ctrl+e` on a job row opens the same wizard filled in from the file, and saving keeps every field the wizard does not ask about. `ctrl+x` twice on a job with no run in flight removes its block. After each of these the dashboard runs `cones install`, so launchd matches the file; an install error shows on the hint line.
