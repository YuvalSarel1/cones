# The fleet and the dashboard

Back to the [README](../README.md). Commands are in [cli.md](cli.md).

## The fleet: every Claude session on the Mac

```sh
cones ls --status blocked        # sessions waiting on a permission, trust or user prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

Nothing is installed. Claude Code keeps a registry of its own sessions, one `~/.claude/sessions/<pid>.json` per live session, interactive or background, written and updated by Claude itself. Every `cones ls` and every dashboard refresh reads that directory, or `$CLAUDE_CONFIG_DIR/sessions` when that variable is set, the same override Claude honors, and fills the rest of the row from the session's transcript under `~/.claude/projects`. No hook runs inside the session and `~/.claude/settings.json` is untouched. If an earlier cones put its hook there, `cones doctor` warns until the entries whose command ends in ` hook $PPID` are deleted; with the hook command gone from the binary each would fail on every event.

Every Claude Code session on the Mac appears in `cones ls` with its working directory, state, last update time, harness, dollars and tokens in/out; the dashboard adds the title, age, context fill and last message. Sessions that belong to a cones run collapse into that run's row. Dollars come from the ledger for cones runs; for other sessions the column stays `-`, since the transcript records tokens and no price.

| Field | Source |
| --- | --- |
| `session_id`, `pid`, `cwd`, `kind` | The registry entry: `sessionId`, `pid`, `cwd` and `kind`, which is `bg` for a session Claude's daemon owns and `interactive` otherwise |
| `state` | The registry `status`, mapped as below |
| `started`, `updated` | The registry `startedAt` and `updatedAt`; for a background job, the `updatedAt` in `~/.claude/jobs/<id>/state.json` |
| `transcript_path` | `~/.claude/projects/<cwd with every non-alphanumeric byte as ->/<session_id>.jsonl`, or the job's `linkScanPath` when that file is missing |
| `title` | Claude's `ai-title`, or a user-set `agent-name`, read from the transcript tail; the registry `name` when the transcript has neither |
| `last` | For a background job, the one-line `detail` Claude keeps in the job's `state.json`; otherwise the first line of the assistant's most recent text |
| `tokens_in`, `tokens_out` | Summed from the transcript, recounted when the file grows; input includes cache reads and cache creation |
| `context_tokens`, `context_window` | The last assistant message's prompt size (input plus cache reads and creation), and the window size when the harness reported one. Claude Code states the window only in its statusLine payload, so today it is absent and never inferred |

| State | Registry `status` | Dashboard label |
| --- | --- | --- |
| `active` | `busy`, `shell` | working |
| `idle` | `idle` | idle |
| `blocked` | `blocked`, `waiting`, `needs_user`, `needs_trust` | needs input |
| the word itself | any other value | the word itself |

A registry entry is a live session only when its pid is running and the process start time `ps` prints under UTC equals the entry's `procStart`; a gone pid or a reused one is a crashed session and is skipped, as is an entry with no timestamp. There is no exited state: when a session ends Claude removes its entry and the row leaves the list. Those two are what the removed hook offered that the registry does not, an exited row that lingered for an hour and the name of the last hook event and tool; everything else the hook recorded comes from the registry or the transcript.

Stopping and attaching follow the session's owner. A session whose kind is `bg` belongs to Claude's daemon, which respawns a killed worker, so `cones stop` ends it with `claude stop <short id>`; any other session gets SIGTERM on the registry pid after cones checks the pid still belongs to a `claude` binary. `cones attach` runs `claude attach <short id>` while the session's process is alive; once it is gone, cones resumes the session in the background (`claude --bg --resume <session>`) and attaches to it, so Ctrl+Z detaches and the session keeps running until it is exited or stopped. cones calls the `claude` binary by path, so a shell alias such as `claude='claude --dangerously-skip-permissions'` does not reach it; typing `claude stop <id>` yourself under that alias turns into a prompt.

## The coordinator: one session per folder

Coordination between agents sharing a tree is not cones logic. It is the [start-orchestrator](https://github.com/YuvalSarel1/orchestrator) skill: one Claude Code session that finds every agent whose cwd is the folder, introduces itself, holds commits until it says go, relays findings and insists on a clean tree when the last job ends. cones owns locks, schedule and ledger; the coordinator owns the conversation. cones ships the skill inside its binary, from `assets/coordinator/`, so nothing needs installing.

```sh
cones coordinator start            # this folder
cones coordinator start ~/src/app  # another folder
```

Each start rewrites the plugin under `~/.cones/coordinator/plugin` (or the `--state-dir`), then runs `claude --bg --plugin-dir <that> /cones:start-orchestrator` in the folder, so the coordinator is an ordinary background session loaded with the skill for that session only: it shows in `cones ls` and the dashboard, `claude attach <id>` opens it, and telling it "stop orchestrator" ends its role. The skill writes `~/.claude/orchestrator/<sha1 of the folder>.json` with its pid and peers every tick, the same file a hand-typed `/start-orchestrator` from an installed copy of the skill writes, so `cones coordinator start` and the skill's own guard both see a coordinator started either way: when that file names a live process for the folder, the command prints it and does nothing.

The copy under `assets/coordinator/` is the upstream skill with one line changed, the helper path, which cones fills in when it writes the plugin. Update it by copying the upstream files over and re-applying that line.

## The dashboard: jobs, sessions and runs on one screen

`cones tui` reloads every second and reads `N working · N need input · N idle · N jobs · N runs` on its summary line.

Its hint line reads `↑↓ move · enter <verb> · x x stop · e edit jobs · s regroup · n new task · / filter · r refresh · q quit`, where the verb is `start job` on a job, `follow log` on a running run, `attach` on a session or a finished run, and `open` with nothing selected.

| Pane | Columns | Details pane |
| --- | --- | --- |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status | schedule, policy line, prompt |
| Sessions | icon, harness, title or short id, then the `columns:` list from [jobs.yaml](jobs.md#dashboard-columns): by default state, age, context, last message or cwd | last prompt and full reply |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason | captured output and harness stderr |

Each table opens with a dim row naming its columns, padded to the table beneath; the cursor skips it and `/` hides it while a filter is set. The sessions row sits once above the first directory group, since the groups share one table. The context cell reads `98k`: tokens in the window at the last turn, with `/200k 49%` appended only when the harness reported the window size. It is `-` until the transcript holds a message with usage.

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top. Within a group they are ordered oldest first by start time, so a new session appends at the bottom and rows hold still; a registry entry without a start time sorts by its last update.

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

Ctrl+Z never suspends the dashboard; inside an attached session it detaches and returns here. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger.
