# Harness sources and capabilities

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md) · [Harness definitions](harness-definitions.md)

cones discovers sessions, reads their reports and chooses native launch and control operations. This guide follows those operations. Missing reports stay absent; state and usage are never estimated from elapsed time or a model name. cones installs no hooks, watchers or background instrumentation inside harness sessions.

## Discovery

| Harness | Native home | Live session source |
| --- | --- | --- |
| Claude Code | `$CLAUDE_CONFIG_DIR`, otherwise `~/.claude` | `sessions/<pid>.json`, maintained by Claude for interactive and background sessions. |
| Codex | `$CODEX_HOME`, otherwise `.codex` beside the Claude home | Process table, thread writer locks and saved composer threads. Sibling `.codex-*` homes with `config.toml` also supply daemon/history records. |
| pi | `$PI_CODING_AGENT_DIR`, otherwise `.pi/agent` beside the Claude home | Process table and session files. |

A missing native home skips that harness's discovery. Codex and pi processes come from `TZ=UTC ps -axww -o pid=,lstart=,command=`. The program itself must match; a name embedded in another command's arguments is insufficient. Kernel `proc_pidinfo` supplies cwd and open files, with `lsof` as a per-process fallback. An unreadable process table fails the refresh instead of claiming every session exited.

### Claude Code

| Fact | Source or rule |
| --- | --- |
| Session id and kind | Registry `sessionId` and `kind`: `bg` or `interactive`. |
| Liveness | Registry `pid` must be alive and its UTC `ps` start must equal `procStart`, preventing pid reuse from identifying a different process. Gone or reused pids are omitted. |
| Warm workers | `spare: true` entries are omitted, as in Claude's own listing; they are workers awaiting a session. |
| Directory | Registry `cwd` for interactive sessions. Background rows use `jobs/<jobId>/state.json`'s launch cwd; the registry cwd follows later worktrees and still locates the transcript. |
| Transcript | `projects/<cwd with non-alphanumeric bytes replaced by '-'>/<sessionId>.jsonl`, then the job's `linkScanPath` if that path is missing. The constructed path depends on Claude's internal layout; interactive sessions supply no native path. |
| Exit | Claude removes the registry entry. There is no retained exited row. |

Subagents run inside their parent's process and do not get separate registry rows.

### Codex

Codex has no registry. A live `codex` process is a candidate unless its first argument is a service or management subcommand, such as `app-server`, `mcp-server`, `login`, `update` or `doctor`. The [built-in definition](../assets/harnesses/codex.yaml) owns the exclusion list.

| Fact | Source or rule |
| --- | --- |
| Thread id | Explicit `resume <thread id>` argument, then a held writer lock, then an unambiguous rollout match. Before identification, the row uses `codex-<pid>`. |
| Daemon ownership | `thread-writer-locks/<id>.lock` open in the live pid named by `app-server-daemon/app-server.pid`. A client of that thread also has kind `daemon`; several clients share one row. |
| Daemon liveness | The kernel's open-file table, not a lock file's existence or mtime. Codex before 0.154 lacks writer locks, leaving only cones's saved composer records for detached threads. |
| Directory | Kernel cwd for a process. Detached threads use `threads.cwd` in `state_*.sqlite`, then rollout `session_meta.cwd`. |
| Rollout | `threads.rollout_path` in the database, otherwise a file under `sessions/` ending in `-<id>.jsonl`. Rollouts are created on the first turn, so an unused client may have none. |
| Saved threads | Identified composer threads with a reported turn are recorded in `STATE_DIR/codex-threads.json`, allowing resume after daemon exit. A missing rollout removes the saved row. |

A process that states no thread can take the newest rollout in its cwd that started at or after the process, only when exactly one live Codex process could have written it and no known writer owns it. With two candidates the attribution is omitted. Rollout mtimes only prune the scan; they never supply session activity.

While the local daemon runs, a remote client with no explicit thread id has no separate process row. Its thread appears through the writer lock or saved record. Remote clients never receive a rollout through cwd/start matching.

### pi

pi overwrites its argv with the process title `pi`, reporting no flags or session id. Consequently `pi install` and `pi update` also appear until they exit; filtering them would require guessing. A server titled `pi-rpc` does not match.

`PI_CODING_AGENT_SESSION_DIR`, when nonempty, supplies the complete session directory. Otherwise the kernel cwd locates `sessions/--<cwd>--/` under the native home: drop the leading slash and replace `/` and `:` with `-`. With exactly one live pi in that cwd, choose its most recently written session file since process start. The file's first-line cwd must also match, because distinct paths can flatten to the same directory name. With multiple processes, attribution is omitted. The file's own start is insufficient because `--continue` appends to an older session.

The session id is the file's first-line `id`, otherwise `pi-<pid>`. A session file appears after the first turn. The row ends when the process exits.

### Historical sessions

History uses native transcript archives independently of live registries, processes and writer locks.

| Harness | Files and exclusions |
| --- | --- |
| Claude | `projects/*/*.jsonl`; exclude nested subagent transcripts and records marked `isSidechain`. |
| Codex | Rollouts under `sessions/` and `archived_sessions/`; exclude sources identified as subagents. Database names precede the legacy index and transcript prompt. |
| pi | `sessions/*/*.jsonl`, or the directory named by `PI_CODING_AGENT_SESSION_DIR`. |

Identity is harness, canonical native home and session id. Aliases of a home collapse; separate homes stay distinct. Directory symlinks are not followed. Claude copies across project folders collapse to the copy with the latest recorded activity. Entries without a recorded cwd are omitted; missing activity remains absent and sorts last. No file mtime substitutes for a reported timestamp.

Previews select user and assistant text in file order. Claude uses native message content; Codex uses UI `UserMessage` events or legacy `user_message` records plus assistant `response_item` text; pi uses its user/assistant messages. Tool results, thinking and injected instructions are excluded, and terminal control sequences are stripped before display.

### Coordinator identity

The skill writes `<Claude home>/orchestrator/<sha1 of absolute cwd>.json` each sweep with pid, cwd and peers. Matching both pid and cwd sets `coordinator: true` in session JSON. The title is not used for identification. The [launcher](cli.md#coordinator-launch) starts a background Claude session; a hand-started coordinator can be interactive, and all other behavior follows that native kind.

## Reports

| Value | Claude Code | Codex | pi |
| --- | --- | --- | --- |
| Session start | First transcript timestamp; registry `startedAt` is not read. | Process start; detached threads use rollout `session_meta.timestamp`, then saved launch time. | Process start, since continuing an older file does not start a new process at that file's timestamp. |
| Last activity | Last transcript timestamp; registry/job `updatedAt` is not used. | Timestamp on the last rollout line. | Timestamp on the last session entry. |
| Title | Transcript `custom-title` or user-set `agent-name`, then `ai-title`; otherwise background job `name`, registry `name`, then first instruction line. Bare job/session ids do not count as names. Dashboard rename appends `custom-title`. | `threads.name`, then `threads.title` in `state_*.sqlite`, then legacy `session_index.jsonl`'s `thread_name`, then first rollout `UserMessage`. Database reads use `sqlite3`. | Last `session_info.name`, otherwise first instruction line. |
| Last reply | Background job `detail`, otherwise first line of last assistant text. | Last assistant `response_item` message's `output_text`. | Last `text` block of the last assistant message. |
| Input/output totals | Transcript `message.usage`, counted once per message id. Input includes cache reads and creation. | Latest `token_count.info.total_token_usage.input_tokens` and `output_tokens`; input includes cache reads. | Sum assistant `usage`: input + cacheRead + cacheWrite for input, output for output. `totalTokens` is not used. |
| Context tokens | Last real message's input + cache creation + cache read, the same usage fields as statusLine. | Latest `token_count.info.last_token_usage.total_tokens`. | Those same three input counters on the last assistant message, as in pi's status line. |
| Context window | Saved statusLine payload, described below. | Latest `token_count.info.model_context_window`. | Absent; the catalog's model window is not written in session files. |
| Model | Last message with usage, `message.model`. | Latest `turn_context.model`. | Last assistant message's `model`; `model_change` entries are not used. |
| Session cost | Saved statusLine `cost.total_cost_usd`, when present, including reported zero. Finished supervised run cost comes from the [result event](jobs.md#results). | Absent: rollouts contain tokens without prices. | Sum `usage.cost.total`; zero shows `-`. pi writes zero for models it has not priced, including its Bedrock models. |

Claude's `<synthetic>` messages are skipped: they represent turns without a model answer and contain zero usage. Before a harness reports usage, counters stay absent. An absent last-activity timestamp is never replaced by process start.

Model names come from the provider catalog recorded by `aws bedrock list-foundation-models`, normalized across regions and revisions. The display drops the redundant Claude prefix: `claude-fable-5-1` becomes Fable 5.1. Known families absent from the catalog are spelled from their ids; unknown families and bare aliases remain verbatim. The [catalog and naming code](../src/fleet.rs) define the mapping.

### State

| Harness | State mapping |
| --- | --- |
| Claude interactive | Registry `busy` or `shell` means working, `waiting` means input, `idle` means idle. Other words are preserved. |
| Claude background | The precedence table below, matching Claude Code 2.1.272's own listing. |
| Codex | Rollout `event_msg`: `task_started` means working, `task_complete` done, `turn_aborted` stopped. New turns replace the prior turn state. Approval waits have no event and remain working; no rollout means `-`. |
| pi | User or tool-result entry means working. Assistant `stopReason`: `toolUse` working, `stop` idle, `aborted` stopped, `error` failed. Unknown reasons and no turn give `-`. There is no approval prompt or input state. |

Claude background state uses the first matching rule:

| Order | Native report | State and reason |
| --- | --- | --- |
| 1 | Registry status `busy` or `shell` | Working. The registry updates for a new prompt before the previous job result catches up. |
| 2 | Job state `done`, `failed` or `stopped`, with tempo no longer `active` | That terminal state; `done` also requires no routine, self-wake or in-flight `session_cron` that could restart it. |
| 3 | Tempo `blocked` or registry status `waiting` | Input. |
| 4 | Any other background job | Working. Claude's listing never labels a live background job idle. |

A completed turn followed by a local command such as `/compact` may still satisfy rule 4. Treating the pane's quiet prompt as idle would override the native report and misclassify the lag case in rule 1.

### Context window for Claude

Only statusLine stdin reports `context_window.context_window_size`; the transcript, registry and `claude agents --json` do not. cones reads a saved copy under `<Claude home>/statusline/<session_id>.json`, including its reported `cost.total_cost_usd` when present. Missing, negative or non-finite costs remain unavailable. To provide it, add this after `input=$(cat)` in your statusLine command, using the same native home as the dashboard:

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

## Native actions

A daemon-owned session supports clients that can join and leave without ending the agent. An interactive terminal elsewhere has no such protocol, so cones reports `own terminal`; it neither takes over that tty nor tries a background attach on it. Before signaling an interactive process, cones verifies its program name and native identity.

| Session or run | Open | What survives viewer closure | Stop or removal |
| --- | --- | --- | --- |
| Claude background | `claude attach <short id>` in its cwd. | The daemon-owned session. | `claude rm <short id>` removes the job record but preserves the transcript. A signal alone lets the daemon respawn it; `claude stop` leaves a stopped record. |
| Claude interactive, standalone Codex, external pi | Refused: own terminal. | Not owned by this dashboard. | SIGTERM to the verified process. |
| Codex daemon thread | `codex --remote unix://<socket> resume <thread id>`, even with another client attached. | The thread in its daemon. | Forget the saved record, hide the id and close or signal any client on the row. A detached thread has no native stop; it remains resumable. Hiding prevents its held lock from restoring the row after restart. |
| pi from the composer | Return to its existing viewer; there is no live attach. | Nothing; pi owns that viewer's terminal and ends with it. | Close the owned viewer. |
| Supervised run in flight | Follow captured output. | The supervised process. | [Terminate its process group](jobs.md#run-lifecycle). |
| Finished Claude run or Claude history | `claude --bg --resume <session>`, then attach. | A new background session, also visible live. | The original finished-run row can be hidden without deleting its output. History offers no deletion. |
| Codex history | Unarchive if needed, then native remote resume. | The thread in its daemon. | History offers no deletion. |
| pi history | `pi --session <transcript>`. | Nothing; its resumed client owns the terminal. | History offers no deletion. |

Historical resume uses the recorded cwd and native home. Claude background conversations remain available in `claude --resume` after removal; forgotten Codex conversations remain in `codex resume`. For shell use, invoke the native binary directly: a Claude alias that appends flags can turn `claude stop <id>` into a new prompt instead of a subcommand.

### Composer identity

| Harness | Launch | Native identity handover |
| --- | --- | --- |
| Claude | `claude --bg -- <instruction>` in the chosen cwd. | Returned short id matched to registry id and folder. An unreported launch expires after 90 seconds. |
| Codex | Start or find the app-server daemon, then open its remote client with the instruction. | Child pid first. A daemon thread replaces it only when exactly one new thread matches folder, start time and first prompt, with no competing unresolved launch. |
| pi | `pi -- <instruction>` in the viewer terminal. | Child pid, harness and folder match the process row. A later session-file id preserves that viewer association. |

Codex's daemon start reports `socketPath` and is idempotent. Its remote client does not report an initial thread id, so prompt/cwd/start matching is an association limit. Ambiguous launches retain their own viewer rows; cones never chooses a thread merely because its rollout is newest. Returning to the list before discovery finishes keeps trying on later refreshes. Closing an unidentified client may leave no saved row.

Model and provider overrides follow [configuration](jobs.md#job-fields-and-defaults). A Codex daemon keeps the provider from its own configuration; a different provider region requires another native home. Pi receives `defaults.pi_model` through `--model` and `defaults.pi_provider` through `--provider` when set. Native session permissions remain harness-owned.

### Supervised execution

| Harness | Support and reason |
| --- | --- |
| Claude Code | Execution adapter verifies version `>=2.1, <3` and the compiled flags against native help, then uses the [run contract](jobs.md#what-the-harness-is-told). |
| Codex | No execution adapter: native enforcement and terminal result reporting remain unverified. |
| pi | No execution adapter: pi offers no sandbox for the required write policy; terminal result reporting is unverified. |

Unsupported jobs parse but fail execution validation, including installation. A direct run records the validation failure. Native discovery and control do not require an execution adapter. A new integration's schema and native handlers are described in [Harness definitions](harness-definitions.md#changing-or-adding-a-harness).
