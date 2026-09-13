# The fleet and the dashboard

Back to the [README](../README.md). Commands are in [cli.md](cli.md).

## The fleet: every Claude session on the Mac

```sh
cones hook --install             # one global Claude Code hook in ~/.claude/settings.json
cones ls --status blocked        # sessions waiting on a permission or elicitation prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

With the hook installed, every Claude Code session on the Mac appears in `cones ls` with its working directory, state, last update time, harness, dollars and tokens in/out; the dashboard adds the title, age, context fill and last message. Sessions that belong to a cones run collapse into that run's row, and a session whose process is gone is not shown. Dollars come from the ledger for cones runs; for hook-observed sessions the column stays `-`, since the hook records tokens and no price.

`cones hook --install` adds one command to `~/.claude/settings.json` for the `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Notification`, `Stop` and `SessionEnd` events, none of them `PreToolUse`, so Claude's permission checks are untouched. On each event Claude Code runs `cones --state-dir ~/.cones hook $PPID`, which writes `~/.cones/fleet/<session_id>.json`.

| Field in the state file | Source |
| --- | --- |
| `cwd`, `pid`, `transcript_path` | The hook payload; `$PPID` is the Claude process |
| `state`, `event`, `tool` | The event name and tool, mapped as below |
| `title` | Claude's `ai-title`, or a user-set `agent-name`, read from the transcript tail |
| `last` | First line of the assistant's most recent text |
| `tokens_in`, `tokens_out` | Summed from the transcript at `Stop` and `SessionEnd`; input includes cache reads and cache creation |
| `context_tokens`, `context_window` | The last assistant message's prompt size (input plus cache reads and creation) and the window it ran in: 1M when the model id carries `[1m]`, otherwise 200k. Read at the same events. |

Re-running `cones hook --install` replaces the earlier entry, so a moved binary or a different `--state-dir` is picked up. To remove it, delete the entries ending in `hook $PPID` from the settings file.

| State | Set by | Dashboard label |
| --- | --- | --- |
| `active` | `UserPromptSubmit`, `PostToolUse` | working |
| `idle` | `SessionStart` with nothing asked yet, `Stop`, and the `idle_prompt` notification Claude sends a minute after a turn ends | idle |
| `blocked` | A `Notification` of type `permission_prompt`, `elicitation_dialog` or `elicitation_url_dialog` | needs input |
| `exited` | `SessionEnd`. The row leaves the list after an hour; the file stays. | exited |

Other notifications (`auth_success`, `agent_completed`, `quota_*`) leave the state unchanged.

`cones ls` and the dashboard also ask `claude agents --json` (at most every 3 seconds; ignored after 2 seconds, on a non-zero exit or when `claude` is missing). Sessions Claude lists appear even before the hook saw them, with `working` mapped to `active` and anything else to `idle`, and Claude's agent name fills a missing title. When Claude keeps a one-line status for a background job (`~/.claude/jobs/<id>/state.json`), that `detail` line is the session's last column.

Stopping and attaching follow the session's owner. A session that `claude agents --json` lists belongs to Claude's daemon, which respawns a killed worker, so `cones stop` ends it with `claude stop <short id>`; any other session gets SIGTERM on the hook's `$PPID` after cones checks the pid still belongs to a `claude` binary. `cones attach` runs `claude attach <short id>` while the session's process is alive; once it is gone, cones resumes the session in the background (`claude --bg --resume <session>`) and attaches to it, so Ctrl+Z detaches and the session keeps running until it is exited or stopped. cones calls the `claude` binary by path, so a shell alias such as `claude='claude --dangerously-skip-permissions'` does not reach it; typing `claude stop <id>` yourself under that alias turns into a prompt.

## The coordinator: one session per folder

Coordination between agents sharing a tree is not in cones. It is the [start-orchestrator](https://github.com/YuvalSarel1/orchestrator) skill: one Claude Code session that finds every agent whose cwd is the folder, introduces itself, holds commits until it says go, relays findings and insists on a clean tree when the last job ends. cones owns locks, schedule and ledger; the coordinator owns the conversation.

```sh
cones coordinator start            # this folder
cones coordinator start ~/src/app  # another folder
```

Both run `claude --bg /start-orchestrator` in the folder, so the coordinator is an ordinary background session: it shows in `cones ls` and the dashboard, `claude attach <id>` opens it, and telling it "stop orchestrator" ends it. The skill writes `~/.claude/orchestrator/<sha1 of the folder>.json` with its pid and peers every tick; when that file names a live process for the folder, `cones coordinator start` prints it and does nothing, and the skill refuses a second instance on its own as well. The skill is installed as `~/.claude/skills/start-orchestrator`; without it the command fails with the two install lines.

## The dashboard: jobs, sessions and runs on one screen

`cones tui` reloads every second and reads `N working · N need input · N idle · N jobs · N runs` on its summary line.

Its hint line reads `↑↓ move · enter <verb> · x x stop · e edit jobs · s regroup · n new task · / filter · r refresh · q quit`, where the verb is `start job` on a job, `follow log` on a running run, `attach` on a session or a finished run, and `open` with nothing selected.

| Pane | Columns | Details pane |
| --- | --- | --- |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status | schedule, policy line, prompt |
| Sessions | icon, harness, title or short id, then the `columns:` list from [jobs.yaml](jobs.md#dashboard-columns): by default state, age, context, last message or cwd | last prompt and full reply |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason | captured output and harness stderr |

Each table opens with a dim row naming its columns, padded to the table beneath; the cursor skips it and `/` hides it while a filter is set. The sessions row sits once above the first directory group, since the groups share one table. The context cell reads `98k/200k 49%`: tokens in the window at the last turn, the window size, and the fill. It is `-` until the session's first `Stop`.

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top. Within a group they are ordered oldest first by start time, so a new session appends at the bottom and rows hold still; a session file without a start time sorts by its last update until its next hook event pins one.

| Key | Action |
| --- | --- |
| `↑` `↓`, `k` `j` | Move between rows. |
| `enter`, `→`, `a` | On a job: start a run in the background. On a running run: follow its log (Ctrl+C returns). On a finished run or a session: open it in this terminal, as described above; Ctrl+Z comes back. |
| `x` twice (or `ctrl+x` twice) within two seconds | Stop the selected run or session. On a job: stop that job's run in flight; with none, the status line says so. |
| `e` | Open jobs.yaml in `$VISUAL` or `$EDITOR`, then run `cones install` on return so launchd matches the file; an install error shows on the status line. |
| `ctrl+s` (or `s`) | Regroup sessions by state or by directory. |
| `n` | New task: type a prompt, `enter` dispatches it as `cones run --prompt` in the current directory, `esc` cancels. |
| `/` | Filter rows by text; `enter` keeps the filter, `esc` clears it. |
| `r` | Reload now. |
| `esc`, `q`, `ctrl+c` | Quit. |

Ctrl+Z never suspends the dashboard; inside an attached session it detaches and returns here. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger and the fleet files.
