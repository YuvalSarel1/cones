# What cones needs from a harness

Back to the [README](../README.md). Run flags are in [runs.md](runs.md), the fleet columns in [fleet.md](fleet.md).

cones never runs inside a session and never deduces what a session is doing. Every value it shows or acts on comes from something the harness reports, or it is not shown. A guessed value is a defect: the context window was once inferred from settings.json and a token threshold, and live sessions rendered at 194%. A harness is supported when every row below is `reported` or `-`, never `deduced`.

Status words:

| Word | Meaning |
| --- | --- |
| reported | The harness states it; the source is named. |
| deduced | cones derives it from a rule of thumb. Works today, breaks silently. Listed under [Open](#open). |
| `-` | Not reported. The column shows `-`, the feature is off. |
| unknown | Not yet checked for this harness. |

## Observe

What the fleet view and `cones ls` show for every live session.

| Need | Claude Code | Codex |
| --- | --- | --- |
| Discover live sessions | reported: `~/.claude/sessions/<pid>.json`, one per session, bg or interactive. `$CLAUDE_CONFIG_DIR` relocates it. | reported: no registry; the process table plus Codex's writer locks. `ps -axww` lists every `codex` process whose first argument is not a subcommand that runs no session (`app-server`, `mcp-server`, `login` and the like). A thread the app-server daemon holds with no client attached is a `thread-writer-locks/<thread id>.lock` under the Codex home, flocked by the daemon for as long as the thread is loaded; `lsof` on those files names the holder, and the daemon's pid is `app-server-daemon/app-server.pid`. Codex before 0.154 has no lock directory; then only threads cones launched itself are known, from `~/.cones/codex-threads.json`. |
| Liveness proof | reported: registry `pid` and `procStart`, the process start time as `ps -o lstart` prints it under UTC. cones compares that text with the live process table, so a reused pid is not a session. | reported: the row is the live process; nothing to reconcile. A daemon thread is live while its lock is open in `lsof`; a lock file whose holder died is not listed and is not a row. |
| Working directory | reported: registry `cwd` | reported: `lsof -a -p <pids> -d cwd`, one call per refresh. |
| Kind (background or interactive) | reported: registry `kind` | reported: `daemon` when the thread's writer lock is held by the app-server daemon's pid; a process-table row has no kind and runs in its own terminal. The `source` column of the `threads` table (`vscode`, `cli`, `exec`) names the client that opened it; not shown. |
| State (working, idle, needs input, done, failed, stopped) | reported: a background job's `state` in state.json when done, failed or stopped; else registry `status`: busy, shell, idle, blocked, waiting, needs_user, needs_trust; else a job `tempo` of blocked. Any other value renders as the word itself. | reported: the rollout's `event_msg` turn events, `task_started` for working, `task_complete` or `turn_aborted` for idle. No needs-input event is written there; a turn waiting on an approval reads as working. `-` with no rollout. |
| Session start | reported: the `timestamp` on the first transcript line that carries one. The registry `startedAt` is not read. | reported: the process start as `ps -o lstart` prints it under UTC. The rollout's `session_meta.timestamp` is the match key, not the row's start. |
| Last activity | reported: the `timestamp` on the last transcript line that carries one. The registry `updatedAt`, the job's `updatedAt` and file mtimes are not read. | reported: the `timestamp` on the rollout's last line. `-` with no rollout; the process start is not substituted. |
| Transcript path | deduced: `projects/<cwd with every non-alphanumeric byte as '-'>/<sessionId>.jsonl`, Claude's internal layout. Background jobs report `linkScanPath` in state.json; interactive sessions report nothing. | reported, then matched: `~/.codex/sessions/YYYY/MM/DD/rollout-<local start>-<id>.jsonl` (`$CODEX_HOME` relocates it), created on the session's first turn. A process states its thread when it holds the thread's writer lock or was started with `resume <thread id>`; that thread's rollout is the `rollout_path` in the `threads` table of `state_*.sqlite`, else the file under `sessions/` whose name ends in `-<id>.jsonl`. Only a process that states neither falls back to the match: the rollout's `session_meta` records `cwd` and a start `timestamp` and no pid, and it is tied to the only live Codex in that cwd that started at or before it, else `-`. |
| Title | reported: transcript `ai-title` or `agent-name`, else registry `name` | reported: `name`, else `title` (the first prompt), for the id in the `threads` table of `~/.codex/state_*.sqlite` (Codex 0.154; read with `sqlite3`), else `thread_name` in the legacy `session_index.jsonl`, else the rollout's first `UserMessage`. |
| Last reply | reported: state.json `detail` for background jobs, else the transcript's last assistant text | reported: the rollout's last assistant `response_item` message, its `output_text`. |
| Tokens in and out | reported: transcript `message.usage`, summed once per message id | reported: `token_count.info.total_token_usage` on the rollout's last such event, `input_tokens` (cache reads included, as Codex counts them) and `output_tokens`. Codex holds no cones budget, so no dollar figure follows. |
| Context tokens at the last turn | reported: the last message's `input_tokens` plus cache creation and cache read, the fields Claude's statusLine `current_usage` carries. Shown with no denominator. A message whose model is `<synthetic>` is Claude's placeholder for a turn no model answered and is skipped. | reported: `token_count.info.last_token_usage.total_tokens` on the rollout's last such event. |
| Context window size | `-`: stated only in the statusLine stdin JSON (`context_window.context_window_size`), which reaches nothing outside the session. Transcript, registry, hook payloads and `claude agents --json` have none. The fleet record has no field for it. | `-`: `token_count.info.model_context_window` in the rollout. No field for it. |
| Cost | `-` for sessions, the transcript records tokens and no price. Reported for cones runs from the result event `total_cost_usd`. | `-`: the rollout records tokens and no price. |
| Model | reported: `message.model` on the last transcript message with usage, the bare API id such as `claude-fable-5-1`, shown verbatim. | reported: `turn_context.model` on the rollout's last turn, such as `openai.gpt-6-astra`, shown verbatim. |

## Trigger

What `cones run` and `cones coordinator start` need to launch a harness. The compiled argv is in [runs.md](runs.md#what-the-harness-is-told).

| Need | Claude Code | Codex |
| --- | --- | --- |
| Headless run with a prompt | reported: `--print -- <prompt>` | unknown |
| Streamed events | reported: `--output-format stream-json --verbose`, one JSON object per line | unknown |
| Result event with cost, usage and session id | reported: the `result` event. A missing `total_cost_usd` fails the run as `missing_cost`. Budget stops zero the aggregate usage and keep `modelUsage`; cones sums that instead. | unknown |
| Permission denial as an event | reported: `permission_denials` on the result and a `system` event with subtype `permission_denied`. A sandboxed command the OS refuses raises no event; the sandbox blocks it and the run goes on. | unknown |
| No prompts ever | reported: `--permission-mode dontAsk --permission-prompts none` | unknown |
| Tool allowlist | reported: `--tools`, `--allowedTools` | unknown |
| Read-only or workspace-write sandbox | reported: `--settings` with `sandbox.enabled` and `failIfUnavailable` | unknown |
| Turn cap | reported: `--max-turns` | unknown |
| Dollar budget | reported: `--max-budget-usd` | unknown |
| Model select | reported: `--model` | unknown |
| Pinned session id | reported: `--session-id`. A different id in any event ends the run as `session_mismatch`. | unknown |
| Session name | reported: `--name` | unknown |
| No user or project settings, no MCP, no slash commands | reported: `--setting-sources ""`, `--strict-mcp-config --mcp-config`, `--disable-slash-commands` | unknown |
| Version and flag probe | reported: `claude --version` against `>=2.1, <3`; every compiled flag against `claude --help` | unknown |
| Background session with a skill | reported: `claude --bg --plugin-dir <dir> /<skill>` | unknown |
| Session in a directory with a first instruction (the dashboard's composer) | reported: `claude --bg` with the instruction in that cwd, resolved from the launch PATH | reported: `codex --remote` with the instruction in that cwd, as a client of the daemon |

## Control

What `stop`, `attach`, `logs` and the timeout need.

| Need | Claude Code | Codex |
| --- | --- | --- |
| Stop an interactive session | reported: the registry pid. cones checks the process name with `ps` before SIGTERM. | reported: the process table pid, checked the same way before SIGTERM. |
| Stop a background session | reported: `claude rm <id>`. The daemon respawns a killed worker, so a signal is not enough; `claude stop` ends the process but `claude agents` keeps the stopped record until `claude rm`. | `-`: Codex has no daemon-owned sessions in the fleet; `app-server` processes are not listed. |
| Kill a run at the timeout | cones owns it: SIGTERM to the process group, SIGKILL two seconds later | unknown |
| Attach to a session | reported: `claude attach <id>` in its cwd, for registry kind `bg`; an `interactive` session is refused, `claude attach` takes background jobs only | `-`: Codex has no attach command. `cones attach` refuses a Codex row. |
| Read a session's output | reported: the transcript, see Observe | reported: the rollout, when matched; `cones logs` reads it. |

## Open

One row is still deduced: the transcript path for interactive sessions. Nothing reports it. Until Claude does, the layout rule stays, and a missing file renders `-`, never a guess.

Codex is observed, not run. Its Observe and Control rows were checked against Codex CLI 0.154 on this Mac: a rollout from a real session, a TUI started without a prompt (which writes no rollout until the first turn), `ps` and `lsof` on that process. Its rollout ties to a process by cwd and start time, the two facts it records, and `-` stands wherever that match is not certain. The Trigger column stays unknown until a real headless run is checked; Codex jobs are parsed and refused at validation ([jobs.md](jobs.md#codex-parsed-refused-at-validation)), and the adapter lands when no row is unknown.
