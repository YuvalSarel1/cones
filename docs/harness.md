# Harness sources and capabilities

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md) · [Harness definitions](harness-definitions.md)

cones discovers sessions, reads their reports and chooses native launch and control operations. This guide follows those operations. Missing reports stay absent; state and usage are never estimated from elapsed time or a model name. cones installs no global session hooks. Owned OpenCode viewers use a [native TUI reporter](#opencode).

The tables below cover the four native report readers. See [additional terminal harnesses](#additional-terminal-harnesses) for the six experimental launchers and their limits.

## Discovery

| Harness | Native home | Live session source |
| --- | --- | --- |
| Claude Code | `$CLAUDE_CONFIG_DIR`, otherwise `~/.claude` | `sessions/<pid>.json`, maintained by Claude for interactive and background sessions. |
| Codex | `$CODEX_HOME`, otherwise `.codex` beside the Claude home | Process table, thread writer locks and saved composer threads. Sibling `.codex-*` homes with `config.toml` also supply daemon/history records. |
| pi | `$PI_CODING_AGENT_DIR`, otherwise `.pi/agent` beside the Claude home | Process table and session files. |
| OpenCode | `$XDG_DATA_HOME/opencode`, otherwise `~/.local/share/opencode` | Process table; explicit session arguments can identify SQLite conversations. |

For the four readers above, a missing native home skips discovery, and so does [`<harness>_enabled: false`](jobs.md#job-fields-and-defaults): the home is never read and the harness's sessions leave the list. Codex, pi and OpenCode processes come from `TZ=UTC ps -axww -o pid=,lstart=,command=`. The program itself must match; a name embedded in another command's arguments is insufficient. The process must also run against a home this machine reads: its own home override and `HOME`, as `ps -wwEp` prints them beyond the command line, resolve to the native home it uses, and a process that resolves to another home, such as a capture fixture's, belongs to that fleet and is not listed here. An environment macOS does not print, as for a platform binary, keeps the process, because a hidden environment must not empty the fleet. Kernel `proc_pidinfo` supplies cwd and open files, with `lsof` as a per-process fallback. An unreadable process table fails the refresh instead of claiming every session exited.

### Claude Code

| Fact | Source or rule |
| --- | --- |
| Session id and kind | Registry `sessionId` and `kind`: `bg` or `interactive`. |
| Liveness | Registry `pid` must be alive and its UTC `ps` start must equal `procStart`, preventing pid reuse from identifying a different process. Gone or reused pids are omitted. |
| Warm workers | `spare: true` entries are omitted, as in Claude's own listing; they are workers awaiting a session. |
| Directory | Registry `cwd` for interactive sessions. Background rows use `jobs/<jobId>/state.json`'s launch cwd; the registry cwd follows later worktrees and still locates the transcript. |
| Transcript | `projects/<cwd with non-alphanumeric bytes replaced by '-'>/<sessionId>.jsonl`, then the job's `linkScanPath` if that path is missing. The constructed path depends on Claude's internal layout; interactive sessions supply no native path. |
| Exit | Claude removes the registry entry. An interactive session has no retained row. |
| Settled background session | The daemon retires a finished background session and drops its registry entry, while `jobs/<jobId>/state.json` keeps reporting `done`, `failed` or `stopped`. That record is a row with no process, in the job's launch cwd, until `claude rm` removes it. Killed jobs leave no record. A record with no terminal state and no live registry entry is not a row. |

Subagents run inside their parent's process and do not get separate registry rows.

### Codex

Codex has no registry. A live `codex` process is a candidate unless its first argument is a service or management subcommand, such as `app-server`, `mcp-server`, `login`, `update` or `doctor`. The [built-in definition](../assets/harnesses/codex.yaml) owns the exclusion list.

| Fact | Source or rule |
| --- | --- |
| Thread id | Explicit `resume <thread id>` argument, then a held writer lock, then an unambiguous rollout match. Before identification, the row uses `codex-<pid>`. |
| Daemon ownership | `thread-writer-locks/<id>.lock` open in the live pid named by `app-server-daemon/app-server.pid`. A client of that thread also has kind `daemon`; several clients share one row. |
| Daemon liveness | The kernel's open-file table, not a lock file's existence or mtime. Codex before 0.154 lacks writer locks, leaving only cones's saved composer records for detached threads. |
| Directory | Kernel cwd for a process. Detached threads use `threads.cwd` in `state_*.sqlite`, then rollout `session_meta.cwd`. |
| Model and effort | `turn_context.model` and `turn_context.effort` on the latest turn, verbatim. A rollout with no turn reports neither. |
| Rollout | `threads.rollout_path` in the database, otherwise a file under `sessions/` ending in `-<id>.jsonl`. Rollouts are created on the first turn, so an unused client may have none. |
| Saved threads | Identified composer threads with a reported turn are recorded in `STATE_DIR/codex-threads.json`, allowing resume after daemon exit. A missing rollout removes the saved row. |

A process that states no thread can take the newest rollout in its cwd that started at or after the process, only when exactly one live Codex process could have written it and no known writer owns it. With two candidates the attribution is omitted. Rollout mtimes only prune the scan; they never supply session activity.

While the local daemon runs, a remote client with no explicit thread id has no separate process row. Its thread appears through the writer lock or saved record. Remote clients never receive a rollout through cwd/start matching.

### pi

pi overwrites its argv with the process title `pi`, reporting no flags or session id. Consequently `pi install` and `pi update` also appear until they exit; filtering them would require guessing. A server titled `pi-rpc` does not match.

`PI_CODING_AGENT_SESSION_DIR`, when nonempty, supplies the complete session directory. Otherwise the kernel cwd locates `sessions/--<cwd>--/` under the native home: drop the leading slash and replace `/` and `:` with `-`. With exactly one live pi in that cwd, choose its most recently written session file since process start. The file's first-line cwd must also match, because distinct paths can flatten to the same directory name. With multiple processes, attribution is omitted. The file's own start is insufficient because `--continue` appends to an older session.

The session id is the file's first-line `id`, otherwise `pi-<pid>`. A session file appears after the first turn. The row ends when the process exits.

### OpenCode

The [definition](../assets/harnesses/opencode.yaml) excludes service and management commands. Terminal clients appear as `opencode-<pid>` until a leading `--session <id>`, `--session=<id>` or `-s <id>` identifies a saved conversation whose recorded directory matches the process cwd. Multiple external clients naming the same session keep separate process rows. `run`, remote attachments, forks and prompt text do not identify local conversations.

Viewers launched or resumed by the dashboard load a [native TUI reporter](../assets/harnesses/opencode-report.mjs). It reads OpenCode's current route and session state through its TUI plugin API, verified against 1.18.31. Reports identify the current conversation directly, including after a session switch, and supply its status, model, timestamps, tokens and cost. Pending native questions and permission requests show as input. The dashboard matches each private report to its owned child pid.

The reporter uses a temporary TUI configuration and private report file, retained by the terminal host until the native client ends. Existing global and project settings remain native; an explicit `OPENCODE_TUI_CONFIG` is copied alongside its original so relative paths retain their meaning. Its plugins remain in the list. The reporter registers no tools or input handlers and changes no execution permissions. Existing clients must be reopened to load it. `cones ls` includes reports from owned hosts; unrelated external terminals retain the process and SQLite sources described above.

OpenCode's normal TUI uses an in-process backend. Joining an arbitrary terminal would require a reported server address, so those rows say `own terminal`. cones currently launches the standalone TUI; attaching through OpenCode's native server is not integrated.

The reader uses the standard CLI's `session`, `message` and `part` SQLite tables, verified against OpenCode 1.18.31. It scans `opencode.db` and `opencode-*.db` under the native home. `OPENCODE_DB` selects one database: absolute paths are used directly, relative paths resolve under that home, and `:memory:` has no disk history. Legacy JSON storage is not scanned.

Reads use the bundled SQLite library in process with a read-only connection. Caches include the main database and WAL fingerprints; a write confined to the WAL invalidates them. A checkpointed WAL database with no WAL file is opened as an immutable snapshot, with before/after checks rejecting a concurrent writer. Browsing starts no OpenCode process and performs no migrations.

The native contracts come from the [CLI](https://opencode.ai/docs/cli/), [table schema](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/core/src/session/sql.ts), [database paths](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/core/src/database/database.ts) and [context sidebar](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/tui/src/feature-plugins/sidebar/context.tsx). Tests use the [SQLite fixture](../assets/harnesses/fixtures/opencode.sql) and spend no model tokens.

For an end-to-end check with the real OpenCode binary:

```sh
scripts/check
python3 scripts/check-opencode.py target/debug/cones /path/to/opencode
```

The script requires tmux and uses isolated homes with a loopback fixture provider. It exercises composer launch, native response rendering and session columns, Left on empty input and within drafts and menus, viewer reuse, stopping, transcript preview and resume of the original session id. It makes no real model calls and inherits no credentials. Screens and diagnostics remain in the printed temporary directory. The controller kills and reaps its dashboard and checks that its native viewers exit.

### Historical sessions

History uses native transcript archives independently of live registries, processes and writer locks.

| Harness | Files and exclusions |
| --- | --- |
| Claude | `projects/*/*.jsonl`; exclude nested subagent transcripts and records marked `isSidechain`. |
| Codex | Rollouts under `sessions/` and `archived_sessions/`; exclude sources identified as subagents. Database names precede the legacy index and transcript prompt. |
| pi | `sessions/*/*.jsonl`, or the directory named by `PI_CODING_AGENT_SESSION_DIR`. |
| OpenCode | SQLite sessions with no `parent_id` and a recorded directory. `time_archived` controls archive visibility. |

Identity is harness, canonical native home and session id. Aliases of a home collapse; separate homes stay distinct. Directory symlinks are not followed. Claude copies across project folders collapse to the copy with the latest recorded activity. Entries without a recorded cwd are omitted; missing activity remains absent and sorts last. No file mtime substitutes for a reported timestamp.

Previews select user and assistant text in native order. Claude uses native message content; Codex uses UI `UserMessage` events or legacy `user_message` records plus assistant `response_item` text; pi uses its user/assistant messages. OpenCode orders messages by creation time and id, and text parts by id, excluding synthetic and ignored parts. Tool results, thinking and injected instructions are excluded, and terminal control sequences are stripped before display. A harness whose definition declares no peek, such as pi and OpenCode, uses the same preview for live rows without an open owned viewer. Launching or reconnecting to an owned terminal shows its native pane. A harness that declares peek keeps its native pane while a row has a client to join, and falls back to the preview for a row whose client the harness has retired.

### Coordinator identity

`cones coordinator claim` records the session id, harness, pid and folder in the folder's [coordinator state](cli.md#state). Matching the session id and folder sets `coordinator: true` in session JSON, so the mark survives the harness moving that conversation to another process. The holder also refreshes its recorded pid as it ticks. The title is not used for identification, and the mark is applied to every harness's rows, so the role is not tied to one harness. The [launcher](cli.md#launch) starts a background Claude session; a hand-started coordinator can be interactive, and all other behavior follows that native kind.

### Message delivery

A harness definition may declare a `message` operation beside `attach` and `fork`, giving the native command that queues one note to a live session. Placeholders are `{id}`, `{text}` and, where the harness talks to a daemon, `{remote}`. Codex declares `codex queue --thread`, verified against 0.154.0, resolved against the daemon of the home holding the thread. A harness without the block cannot be written to from cones and says so: cones does not type into a session's terminal to approximate it.

## Reports

| Value | Claude Code | Codex | pi |
| --- | --- | --- | --- |
| Session start | First transcript timestamp; registry `startedAt` is not read. | Process start; detached threads use rollout `session_meta.timestamp`, then saved launch time. | Process start, since continuing an older file does not start a new process at that file's timestamp. |
| Last activity | Last transcript timestamp; registry/job `updatedAt` is not used. | Timestamp on the last rollout line. | Timestamp on the last session entry. |
| Title | Transcript `custom-title` or user-set `agent-name`, then `ai-title`; otherwise background job `name`, registry `name`, then first instruction line. Bare job/session ids do not count as names. Dashboard rename uses native `/rename`; a manual name for an external interactive client appends `custom-title`. | `threads.name`, then `threads.title` in `state_*.sqlite`, then legacy `session_index.jsonl`'s `thread_name`, then first rollout `UserMessage`. Database reads use the bundled SQLite library in process. | Last `session_info.name`, otherwise first instruction line. |
| Last reply | Background job `detail`, otherwise first line of last assistant text. | Last assistant `response_item` message's `output_text`. | Last `text` block of the last assistant message. |
| Input/output totals | Transcript `message.usage`, counted once per message id. Input includes cache reads and creation. | Latest `token_count.info.total_token_usage.input_tokens` and `output_tokens`; input includes cache reads. | Sum assistant `usage`: input + cacheRead + cacheWrite for input, output for output. `totalTokens` is not used. |
| Context tokens | Last real message's input + cache creation + cache read, the same usage fields as statusLine. | Latest `token_count.info.last_token_usage.total_tokens`. | Those same three input counters on the last assistant message, as in pi's status line. |
| Context window | Saved statusLine payload, described below. | Latest `token_count.info.model_context_window`. | Absent; the catalog's model window is not written in session files. |
| Model | Last message with usage, `message.model`. | Latest `turn_context.model`. | Last assistant message's `model`; `model_change` entries are not used. |
| Session cost | Saved statusLine `cost.total_cost_usd`, including reported zero, takes precedence over [response accounting](#cost-estimates). A [supervised run](jobs.md#results) reads the same two sources, since its session is an ordinary background session. | [Calculated estimate](#cost-estimates) from reported usage and cached provider/model prices, prefixed `~`. | Reported response costs and [fallback estimates](#cost-estimates) use the same accounting path as every harness. |

Claude's `<synthetic>` messages are skipped: they represent turns without a model answer and contain zero usage. Before a harness reports usage, counters stay absent. An absent last-activity timestamp is never replaced by process start.

OpenCode uses `session.title` and `session.time_updated` for title and activity. Input sums assistant `tokens.input`, `tokens.cache.read` and `tokens.cache.write`; output sums `tokens.output`. Model is the last assistant's `providerID/modelID`. Context follows the native sidebar: input, output, reasoning and both cache counters on the latest assistant with output. No context window is recorded in these tables. Cost uses a valid `session.cost` when the schema provides it, otherwise shared response accounting. Last reply is the first line of the latest visible assistant text.

Model names come from the provider catalog recorded by `aws bedrock list-foundation-models`, normalized across regions and revisions. The display drops the redundant Claude prefix: `claude-fable-5-1` becomes Fable 5.1. Known families absent from the catalog are spelled from their ids; unknown families and bare aliases remain verbatim. The [catalog and naming code](../src/fleet.rs) define the mapping.

### Cost estimates

Every harness uses the same accounting component for agent and history rows. Native adapters translate identities, response costs and token counters into a common response record. Shared code handles duplicate response ids, native-cost precedence, catalog fallback, provenance and complete, partial or unavailable coverage. A resumed run displays its live agent's cost while keeping the original total in the ledger. With no live agent, the recorded total takes precedence over saved-output accounting.

A valid native session total, including zero, takes precedence over all response accounting. Otherwise, each response uses its valid native dollar amount first. Zero response costs are accepted only with explicitly empty usage: Pi and OpenCode can write zero when prices are unavailable. A missing or unpriced response falls back to its reported provider, model and disjoint input, output, cache-read and cache-write counters. Missing identities or counters remain unpriced. A total containing any catalog estimate is prefixed `~`, including totals that also contain native costs.

Claude normally reports no provider identity in its transcript, so it needs its saved statusLine cost. Cones never fills the provider from a model name or local settings. If the transcript does report a provider, its usage can use the same fallback. Claude and Pi reports of long-retention cache writes stay unpriced by the catalog; a native dollar report still takes precedence.

Codex's native reader uses the reported provider, model and request usage. It separates ordinary input, cache reads, cache writes and output; reasoning is already included in output. Requests are priced at their own model and input size before summing. Repeated cumulative updates count once. Missing requests, counters, models or rates make the estimate incomplete. Reported nonstandard service tiers stay unpriced.

OpenCode's normalized output includes both `tokens.output` and `tokens.reasoning`, which its native format stores separately. All five native counters must be present for fallback pricing. Native session and response prices continue to take precedence.

The calculator matches exact provider/model keys in a cached models.dev catalog. Rates are USD per million tokens, with context tiers applied to the whole request. All-zero tables are treated as unpriced. Estimates use standard cache-write rates; unreported cache retention, discounts, subscriptions and additional fees are not reconciled. Historical sessions use the selected snapshot's rates, not reconstructed invoice prices.

The dashboard refreshes the public catalog asynchronously after 24 hours. Failed downloads retain the previous valid snapshot and retry no sooner than an hour later. Snapshots older than seven days become unavailable. Validated data atomically replaces `STATE_DIR/prices.json`; rendering performs no downloads. Internal CLI listings use the existing cache without fetching. Every reader invalidates cached accounting when the catalog arrives, changes or expires, including SQLite history.

Calculated costs show `~$…`. A subtotal with gaps shows the amount it could price and nothing else; the gap is named in session details and JSON, not in the cell. When nothing can be priced, the cell stays `-`. Session details and JSON `cost_info` identify source, coverage, priced/unpriced records, reasons for gaps, and catalog fetch time and checksum. The fetch date identifies snapshot age, not verified provider billing.

### State

| Harness | State mapping |
| --- | --- |
| Claude interactive | Registry `busy` or `shell` means working, `waiting` means input, `idle` means idle. Other words are preserved. |
| Claude background | The precedence table below, matching Claude Code 2.1.272's own listing. |
| Codex | Rollout `event_msg`: `task_started` means working, `task_complete` done, `turn_aborted` stopped. New turns replace the prior turn state. Approval waits have no event and remain working; no rollout means `-`. |
| pi | User or tool-result entry means working. Assistant `stopReason`: `toolUse` working, `stop` idle, `aborted` stopped, `error` failed. Unknown reasons and no turn give `-`. There is no approval prompt or input state. |
| OpenCode | Owned dashboard viewers use the native TUI's busy, retry and idle status and pending question or permission requests. External process rows remain `-`; saved messages do not establish current state. |

Claude background state uses the first matching rule:

| Order | Native report | State and reason |
| --- | --- | --- |
| 1 | Registry status `busy` or `shell` | Working. The registry updates for a new prompt before the previous job result catches up. |
| 2 | Job state `done`, `failed` or `stopped`, with tempo no longer `active` | That terminal state; `done` also requires no routine, self-wake or in-flight `session_cron` that could restart it. |
| 3 | Tempo `blocked` or registry status `waiting` | Input. |
| 4 | Any other background job | Working. Claude's listing never labels a live background job idle. |

A completed turn followed by a local command such as `/compact` may still satisfy rule 4. Treating the pane's quiet prompt as idle would override the native report and misclassify the lag case in rule 1.

### Context window for Claude

Only statusLine stdin reports `context_window.context_window_size`; the transcript, registry and `claude agents --json` do not. cones reads a saved copy under `<Claude home>/statusline/<session_id>.json`, including its reported `cost.total_cost_usd` and `effort.level` when present. Missing, negative or non-finite costs remain unavailable. To provide it, add this after `input=$(cat)` in your statusLine command, using the same native home as the dashboard:

```sh
claude_dir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
mkdir -p "$claude_dir/statusline"
printf '%s' "$input" > "$claude_dir/statusline/$(jq -r .session_id <<<"$input").json"
```

Without that saved payload, the context cell shows prompt tokens alone. cones does not modify the statusLine command or Claude settings itself.

### Activity

Each timestamped transcript/rollout line contributes one `lines` count. The other [chart metrics](jobs.md#activity) use these events:

| Metric | Claude Code | Codex | pi |
| --- | --- | --- | --- |
| `messages` | Assistant lines with usage, once per message id. | Assistant `response_item` messages. | Assistant message entries. |
| `tools` | `tool_use` blocks. | `function_call`, `custom_tool_call`, `local_shell_call` items. | `toolCall` blocks. |
| `tokens` | Assistant `output_tokens`. | `last_token_usage.output_tokens` on `token_count` events. | Assistant `usage.output`. |

OpenCode activity charts are not implemented.

## Native actions

A daemon-owned session supports clients that can join and leave without ending the agent. An interactive terminal elsewhere has no such protocol, so cones reports `own terminal`; it neither takes over that tty nor tries a background attach on it. Before signaling a process, cones verifies its harness process match.

| Session or run | Open | What survives viewer closure | Stop or removal |
| --- | --- | --- | --- |
| Claude background | `claude attach <short id>` in its cwd. | The daemon-owned session. | `claude rm <short id>` removes the job record but preserves the transcript. A signal alone lets the daemon respawn it; `claude stop` leaves a stopped record. |
| Settled Claude background | `claude attach <short id>`, which wakes the session from its job record. Resting on the row never attaches: a peek would wake it. | The woken daemon-owned session. | `claude rm <short id>` removes the record and the row. |
| Externally started Claude interactive, standalone Codex, pi, OpenCode or experimental CLI | Refused: own terminal. | Not owned by this dashboard. | SIGTERM to the verified process. |
| Codex daemon thread | `codex --remote unix://<socket> resume <thread id>`, even with another client attached. | The thread in its daemon. | Forget the saved record, hide the id and close or signal any client on the row. A detached thread has no native stop; it remains resumable. Hiding prevents its held lock from restoring the row after restart. |
| Interactive Claude fork or experimental CLI from the composer | Reconnect to the cones-owned terminal. | The host keeps the native client running. | Stop the hosted terminal with `ctrl+x` twice. |
| pi from the composer | Reconnect to the cones-owned terminal; pi has no native live attach. | The host keeps pi running. | Stop the hosted terminal with `ctrl+x` twice. |
| Supervised run in flight | Join its live session when available; otherwise follow captured output. | The supervisor and daemon-owned session. | [Stop its session and worker process group](jobs.md#run-lifecycle). |
| Finished Claude run | Join its session if still live; otherwise `claude --bg --resume <session>`, then attach. | The background session, represented by the existing run row. | Hide the run row without deleting its output. |
| Claude history | `claude --bg --resume <session>`, then attach. | A new background session in the live list. | History offers no deletion. |
| Codex history | Unarchive if needed, then native remote resume. | The thread in its daemon. | History offers no deletion. |
| pi history | `pi --session <transcript>`, then reconnect to the hosted terminal. | The host keeps the resumed client running. | Stop the live terminal; history offers no deletion. |
| OpenCode composer or history | Reconnect to the hosted terminal, or resume history with `opencode --session <id>`. | The host keeps the native client and reporter running. | Stop the live terminal; history offers no deletion. |

Hosted terminals accept one dashboard attachment at a time. Their registry proves ownership with a held lock, never a saved PID alone. The host retains terminal state and scrollback in memory, answers terminal queries and relays input without changing harness permissions. A host or machine restart ends its processes; only native conversation history remains resumable. This persistence mechanism does not add native history or state reporting to experimental launchers.

For harnesses with a history reader, resume uses the recorded cwd and native home. Claude background conversations remain available in `claude --resume` after removal; forgotten Codex conversations remain in `codex resume`. For shell use, invoke the native binary directly: a Claude alias that appends flags can turn `claude stop <id>` into a new prompt instead of a subcommand.

[Conversation forks](dashboard.md#fork-a-conversation) use a new native identity. Claude forks are interactive clients, unlike its normal background launches. Forks stay in the source directory and do not isolate edits.

### Composer identity

| Harness | Launch | Native identity handover |
| --- | --- | --- |
| Claude | `claude --bg -- <instruction>` in the chosen cwd. | Returned short id matched to registry id and folder. An unreported launch expires after 90 seconds. |
| Codex | Start or find the app-server daemon, then open its remote client with the instruction. | Child pid first. A daemon thread replaces it only when exactly one new thread matches folder, start time and first prompt, with no competing unresolved launch. |
| pi | `pi -- <instruction>` in the viewer terminal. | Child pid, harness and folder match the process row. A later session-file id preserves that viewer association. |
| OpenCode | `opencode --prompt=<instruction>` in the viewer terminal. | Child pid, harness and folder match the process row. Saved history remains independently available. |

Codex's daemon start reports `socketPath` and is idempotent. Its remote client does not report an initial thread id, so prompt/cwd/start matching is an association limit. Ambiguous launches retain their own viewer rows; cones never chooses a thread merely because its rollout is newest. Returning to the list before discovery finishes keeps trying on later refreshes. Closing an unidentified client may leave no saved row.

### Detached launch

[`cones launch`](cli.md#starting-a-session) leaves its session running after the launching shell exits and prints one identifier for the other commands. It adds no new mechanism: Claude keeps its native background daemon, and every other terminal harness is given the same persistent host the composer uses. What differs per harness is the identifier, and how long it stays the name of that conversation.

| Harness | Detached by | Identifier returned | How long it names the session |
| --- | --- | --- | --- |
| Claude | Its own daemon, from `claude --bg`. | Native session id, resolved from the returned short id through the registry. | Permanently: it survives the daemon, cones and the machine. |
| OpenCode | A cones-owned terminal host. | The host's stored session id, which the roster prefers over the process row. Its reporter replaces the initial host id with the native conversation id. | Until the reporter identifies or switches the conversation; re-read the roster after a change. |
| pi | A cones-owned terminal host. | `pi-<pid>`, or pi's own session id once pi has written its session file. | Until pi writes that file, after which the roster carries pi's id; re-read with `cones ls --dir PATH --json`. |
| Gemini, Cursor Agent, Copilot, Amp, Droid, Kimi | A cones-owned terminal host. | `<harness>-<pid>`, a client process rather than a conversation. | The client process's lifetime. |
| Codex | Not detached. It keeps the launching terminal, as before. | None. | — |

A hosted launch waits up to five seconds for the harness's own process row before it answers, because that is the id the roster keeps; a harness process discovery never reports keeps the cones-owned id, which its host record holds for as long as the host runs.

Identity comes from a key the launch owns, never from prompt, folder or start time: Claude's returned background id, or the pid of the client the host has just spawned. Concurrent identical prompts in one folder therefore stay distinct, and cones never resolves a launch by choosing the newest thread or session. Claude naming waits up to twenty seconds. A hosted launch first waits up to five seconds for its process row, then up to twenty seconds for its host id if needed. These waits bound naming alone; expiring reports an unnamed launch and leaves the session running.

An unattended session that is waiting for input stays waiting. cones does not answer it, does not treat quiet output as an ended turn, and does not retire a host for silence. Its native state is what reports the wait, and ending it explicitly is [`cones stop`](cli.md#stopping-a-session).

Codex is unsupported for detached launch. Against codex-cli 0.155.1 on September 22, 2026, no subcommand creates a thread and reports its id: `codex --remote <addr> -- <instruction>` opens a TUI client that reports none, `codex agents` is a browser, `codex app-server daemon` manages the daemon rather than its threads, and `codex exec` is a non-interactive run rather than a session to join. A detached Codex session also has no supported stop: `archive` and `delete` are history operations, and `remote-control stop` and `app-server daemon stop` end the daemon that owns every thread. Since exact identity and continued execution cannot both be established, cones reports no detached launch for it rather than returning an identity it would have to guess.

A hosted terminal has one known gap. [`cones stop`](cli.md#stopping-a-session) finds a cones-owned terminal by matching the id against the host record's own session id, and finds a native session in the Claude registry. An OpenCode row is the host's record, so stopping it works. A pi or experimental row is the harness's process row, `pi-<pid>`, which matches neither, so `cones stop pi-<pid>` reports that it is not a live session even though it is listed and cones owns its terminal. Stop such a session from the dashboard instead, with `ctrl+x` twice on its row. The capability is present on both sides; what is missing is the lookup from a roster row to the host record that owns its pid.

Hosting an experimental launcher gives it a lifetime, nothing else. Their [declared limits](#additional-terminal-harnesses) are unchanged: no transcript, no message delivery, no state, model, context or cost, and a process row rather than a conversation. A cones-owned terminal does not turn a process into a reported session.

Detached launch is exercised in `tests/launch.rs` against disposable homes and fixture launch targets, covering the returned identity, concurrent identical prompts in one folder, survival of the launcher's exit for both a background and a hosted session, a native launch failure, a launch that reports no identity, `--print-command`, refused harnesses and refused folders.

Claude was checked live on September 22, 2026 with Claude Code 2.1.278, a disposable home and a loopback provider: `cones launch` returned the full session id for a real `claude --bg`, the registry agreed and the eight characters the CLI prints were its prefix, the session was listed as `active` after the launcher exited, two concurrent identical prompts in one folder produced two ids and two transcripts, and each session reached the loopback provider and then stopped. Unverified: detach and rejoin of a hosted client through a live dashboard, resume of a detached session, OpenCode's identifier, whose table row follows from how the roster merges host records rather than from a live launch, and hosting for the experimental launchers, which were checked only through the composer.

Model and provider overrides follow [configuration](jobs.md#job-fields-and-defaults). A Codex daemon keeps the provider from its own configuration; a different provider region requires another native home. Pi receives `defaults.pi_model` through `--model` and `defaults.pi_provider` through `--provider` when set. Claude receives `defaults.effort` through `--effort`, for jobs as well as composer sessions, and pi receives `defaults.pi_thinking` through `--thinking`. Codex takes reasoning effort only through a `-c model_reasoning_effort=` configuration override, and OpenCode takes none at all, so cones passes neither. OpenCode receives `defaults.opencode_model` through `--model`. Native session permissions remain harness-owned.

### Supervised execution

| Harness | Support and reason |
| --- | --- |
| Claude Code | Execution adapter verifies version `>=2.1, <3` and the compiled flags against native help, then uses the [run contract](jobs.md#what-the-harness-is-told). |
| Codex | No execution adapter: native enforcement and terminal result reporting remain unverified. |
| pi | No execution adapter: supervised execution and terminal result reporting remain unverified. |
| OpenCode | Supervised jobs are not implemented. Native enforcement and completion reporting remain unverified. Interactive sessions keep OpenCode's own permissions. |

Jobs naming a harness without an execution adapter are rejected when the jobs file is read, with the job's name in the error. They cannot be installed or started as configured jobs. A supported Claude job can still fail native version or capability validation at compilation; a direct run records that failure. Native discovery and control do not require an execution adapter. A new integration's schema and native handlers are described in [Harness definitions](harness-definitions.md#changing-or-adding-a-harness).

## Additional terminal harnesses

Gemini CLI, Cursor Agent, GitHub Copilot CLI, Amp, Droid and Kimi have experimental terminal launch and process discovery adapters. They retain their native permission settings. cones installs no report hooks or config rewrites and reads no transcripts for these integrations. Composer launches inherit their native environment and receive configured `AWS_PROFILE` and `AWS_REGION`, as other harness launches do.

| Harness | Initial interactive instruction | Model selection |
| --- | --- | --- |
| Gemini CLI | `--prompt-interactive=<instruction>` | `--model` |
| Cursor Agent | positional instruction after `--` | `--model` |
| Copilot CLI | `--interactive=<instruction>` | `--model` |
| Amp | anonymous stdin file, while stdout remains a terminal | native configuration |
| Droid | positional instruction after `--` | native configuration |
| Kimi | `--prompt=<instruction>` | `--model` |

Their YAML definitions explicitly declare no native transcript reader. A process row identifies a client PID, not a conversation. State, model, context, tokens, cost and last reply remain absent. Native history, joins of external terminals, exact conversation identity and supervised execution remain unsupported. Their owned terminal can be left with Ctrl+Z, revisited and stopped; tab and arrows retain native behavior. Changing a harness's `*_enabled` field controls its composer entry and discovery. Model defaults exist only where the native CLI has an interactive model flag.

Native boot, process-row identity, Ctrl+Z, returning to the same viewer and stopping were exercised in disposable homes with Gemini 0.60.0, Cursor Agent 2026.09.15-d2fe57e, Copilot 1.0.86, Amp 0.0.1789704050-g778045, Droid 0.222.0 and Kimi 1.50.0 on September 18, 2026. Kimi reports its process title as `Kimi Code`; management commands that use the same title cannot be distinguished after argv is replaced. Model-backed turns, their native permission questions and conversation reporting are unverified. These launchers are not full harness integrations under the acceptance criteria above.

Native references: [Gemini CLI](https://geminicli.com/docs/cli/cli-reference/), [Cursor Agent](https://cursor.com/docs/cli/reference/parameters), [Copilot CLI](https://docs.github.com/en/copilot/reference/cli-command-reference), [Amp](https://ampcode.com/docs/cli), [Droid](https://docs.factory.ai/reference/cli-reference), [Kimi CLI](https://moonshotai.github.io/kimi-cli/en/reference/kimi-command.html). Superset's preset catalog supplied leads; flags were checked independently, and its permission-bypass settings are not included.
