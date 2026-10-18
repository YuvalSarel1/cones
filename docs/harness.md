# Harness sources and capabilities

[README](../README.md) · [Dashboard](dashboard.md) · [Configuration](jobs.md) · [Harness definitions](harness-definitions.md)

cones discovers native sessions, reads reports and invokes supported native operations.
Missing state, context and usage stay absent. It installs no global session hooks; owned
pi and OpenCode clients load native reporting extensions. Four harnesses have report readers;
six more have [experimental terminal adapters](#additional-terminal-harnesses).

## Capabilities

What each [control plane](architecture.md#the-control-plane) verb reaches. "Owned" means a
client in a terminal cones hosts. "Others" are Gemini, Cursor Agent, Copilot, Amp, Droid and
Kimi. The [command reference](cli.md#commands) has each verb's rules.

| Verb | Claude Code | Codex | pi | OpenCode | Others |
| --- | --- | --- | --- | --- | --- |
| `ls` | ✓ | ✓ | ✓ | ✓ | Process only |
| `show`, `search` | ✓ | ✓ | ✓ | ✓ | Copilot, Cursor; owned screens |
| `launch` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `resume` | ✓ | ✓ | ✓ | ✓ | Copilot, Cursor |
| `fork` | ✓ | ✓ | ✓ | ✓ | – |
| `send` | ✓ | Daemon | Owned | Owned | – |
| `wait` | ✓ | ✓ | ✓ | Owned | – |
| `rename` | ✓ | ✓ | Owned | Owned | – |
| `btw` | ✓ | ✓ | ✓ | ✓ | – |
| `compact` | Background | ✓ | Owned | Owned | – |
| `stop` | Background | Daemon | Owned | Owned | Owned |
| `rm` | ✓ | ✓ | ✓ | ✓ | ✓ |
| Join from the dashboard | Background | Daemon | Owned | Owned | Owned |

`wait` needs a reported state, so a session with none never settles. `rm` removes from the
harness's own list where it keeps one and otherwise stops the verified process.

## Discovery

| Harness | Native home | Live source |
| --- | --- | --- |
| Claude Code | `$CLAUDE_CONFIG_DIR`, otherwise `~/.claude` | `sessions/<pid>.json` registry. |
| Codex | `$CODEX_HOME`, otherwise `.codex` beside the Claude home | Processes, writer locks and saved threads. Sibling `.codex-*` homes with `config.toml` also supply daemon/history records. |
| pi | `$PI_CODING_AGENT_DIR`, otherwise `.pi/agent` beside the Claude home | Processes and session files. |
| OpenCode | `$XDG_DATA_HOME/opencode`, otherwise `~/.local/share/opencode` | Processes, owned-client reports and SQLite. |

A missing home or disabled harness skips its reader. Process discovery matches executables,
not names inside arguments.

Each harness can have more homes than its own, and every one is read the same way: live
sessions, state and reported columns, history, and the native join, message, stop and resume,
which run against the home that recorded the session. A headless run, such as `claude -p` or
`codex exec`, has no client to join and is read like any other external session.

| Source | Homes |
| --- | --- |
| Configured | The directories [`homes`](jobs.md#homes) names for the harness. Needed for sessions that have finished, and for clients started under another `HOME`. |
| Detected | The home a live client names in its environment, when that client runs under your own `HOME`, and the home of the rollout a live Codex client holds open, under any `HOME`. cones reads the home from then until it exits; nothing is written to configuration. |

Detection is off for a harness whose home variable is set where cones runs, and for every
harness while `CLAUDE_CONFIG_DIR` is set: an override pins what cones reads, and configured
homes are still added. A client under another `HOME` whose home is not configured belongs to
another fleet and is not listed, unless it is a Codex client with a rollout open: `codex exec`
keeps its rollout open for its whole run, and the file names the home. A client whose
environment cannot be read is offered to every home of its harness: the one holding its conversation names it, and otherwise it is a
bare process row. An environment hidden by macOS keeps a process out for its first ten
seconds. pi hides its environment once it starts, so a pi home is never detected and has to
be configured. A [`homes` name ending in `*`](jobs.md#homes) is detection too: it is off
under the same overrides, and it needs the client's environment, so pi refuses one. Reading
another home starts nothing in it.

Checked against live clients in a second home, with no model call: Codex 0.162.1 (listed,
read, noted and stopped through that home's daemon), pi (configured home; listed and read)
and OpenCode 1.18.31 (listed and read). The experimental terminal harnesses have no home
variable, so they have no second home to read. Kernel information supplies cwd/open files, with `lsof` fallback. An unreadable
process table fails the refresh rather than reporting that every session exited.

### Claude Code

| Fact | Source or rule |
| --- | --- |
| Identity/kind | Registry `sessionId` and `kind` (`bg` or `interactive`). |
| Liveness | Live registry PID with UTC process start matching `procStart`; omit `spare: true` workers. |
| Directory | Rows group by registry `cwd` for interactive sessions and `jobs/<jobId>/state.json` launch cwd for background jobs. Registry cwd locates transcripts after worktree changes. The folder cell, branch and `⑂` mark use the current folder: statusline `workspace.current_dir`, else the latest transcript line's `cwd`, which history rows also use, else registry cwd. Only Git decides `⑂`, so a plain `cd` changes the folder cell alone. |
| Transcript | `projects/<escaped cwd>/<sessionId>.jsonl`, then job `linkScanPath`. The cwd replaces non-alphanumeric bytes with `-`; interactive paths depend on native layout. |
| Background tasks | A background job's `fan` lists one entry per running task with its `kind`, such as `shell`; a record without `fan` gives only the `inFlight.tasks` count. Interactive sessions write no job record, so their tasks are not reported. |
| Exit | Interactive rows leave with their registry entry. Background job records remain at their last reported state until removal, as `claude agents` lists them, including a record a crash left mid-turn. Killed jobs leave no record. |

Subagents share their parent's process and have no separate registry rows.

### Codex

The [definition](../assets/harnesses/codex.yaml) excludes service and management commands.
Before thread attribution, a process row uses `codex-<pid>`.

| Fact | Source or rule |
| --- | --- |
| Thread id | Explicit resume id, then held writer lock, then unambiguous rollout match. |
| Daemon ownership | `thread-writer-locks/<id>.lock` open in the live daemon PID from `app-server-daemon/app-server.pid`, or `daemon.pid` in Codex's managed daemon package. Multiple clients share one thread row. |
| Liveness | Kernel open files, never lock-file existence or mtime. Versions before 0.154 lack writer locks. |
| Directory | Process cwd; detached threads use database `threads.cwd`, then rollout `session_meta.cwd`. |
| Rollout | Database `threads.rollout_path`, then a `sessions/` filename ending in `-<id>.jsonl`. Created on the first turn. |
| Saved threads | Identified composer threads with a turn persist in `STATE_DIR/codex-threads.json`. Missing rollouts remove saved rows. |

A process without an explicit thread may match a rollout started after it in the same cwd
only when exactly one live process could own it and no known writer does. Ambiguity leaves
identity absent. Mtimes prune scans but never supply activity. Remote clients are excluded
from this matching; while the daemon runs, their threads appear through locks or saved
records rather than separate unidentified client rows.

### pi

Pi replaces argv with `pi`, so management commands may appear until exit; `pi-rpc` is excluded.
A nonempty `PI_CODING_AGENT_SESSION_DIR` selects the complete session directory. Otherwise use
`sessions/--<cwd>--/` under the native home, dropping the leading slash and replacing `/` and
`:` with `-`.

With exactly one pi process in a cwd, select its most recently written file since process
start and require the first-line cwd to match. Multiple external candidates remain
unattributed; owned clients report their own identity and [input](#native-input-signals).
This accommodates `--continue` appending to an older session. The row is `pi-<pid>`; its
conversation is the file's first-line `id`. Files appear after the first turn; process exit
removes the row.

### OpenCode

The [definition](../assets/harnesses/opencode.yaml) excludes management commands. Rows are
`opencode-<pid>`. An external client names its conversation only when a leading `--session`,
`--session=` or `-s` names a local one matching the process cwd. Multiple clients keep separate process rows. `run`,
remote attachments, forks and prompt text do not identify a local conversation.

Owned viewers load the [TUI reporter](../assets/harnesses/opencode-report.mjs), verified against
1.18.31. It reports current conversation, state, model, timestamps, tokens and cost, including
session switches and pending questions/permissions. Private reports match the owned child PID
and remain available while detached. A temporary TUI config preserves existing plugins and
settings; explicit `OPENCODE_TUI_CONFIG` copies stay beside their source to preserve relative
paths. The reporter changes no tools, input handlers or permissions. Existing clients must be
reopened to load it.

SQLite history reads `session`, `message` and `part` in `opencode.db` and `opencode-*.db`.
`OPENCODE_DB` chooses one database; relative paths use the native home and `:memory:` has no
disk history. Legacy JSON is unsupported. Reads use bundled SQLite, read-only, with database
and WAL fingerprints invalidating caches; immutable snapshots reject concurrent changes.
Browsing starts no client or migrations. Arbitrary external terminals cannot be joined;
native server attachment is not integrated.

Native contracts: [CLI](https://opencode.ai/docs/cli/),
[schema](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/core/src/session/sql.ts),
[paths](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/core/src/database/database.ts),
[context](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/tui/src/feature-plugins/sidebar/context.tsx).
See [native checks](testing.md#resource-and-native-checks).

### Historical sessions

History reads native archives independently of live processes.

| Harness | Sources and exclusions |
| --- | --- |
| Claude | `projects/*/*.jsonl`; exclude subagent transcripts and `isSidechain` records. |
| Codex | `sessions/` and `archived_sessions/` rollouts; exclude subagent sources. Database names precede legacy index and prompt titles. |
| pi | `sessions/*/*.jsonl`, or `PI_CODING_AGENT_SESSION_DIR`. |
| OpenCode | SQLite sessions with a directory and no `parent_id`; `time_archived` controls archive visibility. |

Identity is harness, canonical native home and session id. Home aliases collapse, separate
homes stay distinct, and directory symlinks are not followed. Duplicate Claude transcripts
use the latest recorded activity. Entries without cwd are omitted; missing activity sorts
last without mtime substitution.

Previews use native user/assistant text in order, excluding thinking, tool results and injected
instructions. Codex uses UI `UserMessage` or legacy `user_message` events plus assistant
`response_item` text. OpenCode orders messages by creation time/id and text parts by id,
excluding synthetic/ignored parts. Terminal escapes are stripped. Unjoinable live rows and
settled sessions can use these previews; owned terminals show their native panes.

### Coordinator identity

A [claim](cli.md#coordinator) identifies the coordinator by native session id and folder,
independently of title or PID changes. Matching rows carry `coordinator: true`; ticks refresh
the recorded PID. The launcher starts background Claude, but the role follows native identity
on every harness.

### Message delivery

A definition's `message` operation supplies native delivery: a command with `{id}`, `{text}`
and optional `{remote}`, Claude's `peer_inbox`, or the `reporter` in a pi or OpenCode client.
Codex uses `queue --thread`, verified
against 0.154.0, on the daemon owning that home. Claude has no delivery command, so cones
writes to the cross-session inbox that Claude's own SendMessage uses. The socket comes from
the session's registry record (`messagingSocketPath`), and the token comes from the key file
Claude writes beside it (`<pid>.<sha256 of the socket path>.key`). cones sends the same auth
line and `user` message SendMessage sends. Claude Code 2.1.278, 2.1.280 and 2.1.281 publish
`peerProtocol` 1 and accept these messages. A session with no inbox, or with a protocol the
definition does not list, is refused with its version.

The recipient applies its own inbound policy. cones is not a Claude session and does not
claim a permission mode. A session that bypasses permission prompts therefore holds the note,
shows it to its user as "Held message from another session" and delivers it only after the
user approves it. Setting `"crossSessionInbound": "accept"` in `~/.claude/settings.json`
delivers these notes without approval; a note held before the change still needs one approval.
cones cannot see whether a note was held, and never releases one by typing into the terminal.

pi and OpenCode have no delivery command, so the reporter cones loads into a client it
launched submits the note through the harness's extension API: pi's `sendUserMessage` with
`deliverAs: "steer"`, and OpenCode's `session.prompt_async` for the session the client shows.
Both start a turn in an idle client and queue the note behind a running one; with pi 0.85.1
and OpenCode 1.18.31 each note became one user message in the saved conversation. cones
leaves the note in the reporter's private directory, named in the terminal host's record, and
waits five seconds for the reporter's answer. A client in a terminal cones does not own, one
whose reporter is not running and an OpenCode showing no session are refused, and an
unanswered note is withdrawn. The answer means the harness accepted the call. A failure after
that, such as a provider error, appears in the client. Delivery to a client waiting on a
question or permission dialog is unverified.

A definition with no channel states `unavailable` with the reason, and the refusal quotes it.
Gemini, Cursor Agent, Copilot, Amp, Droid and Kimi take prompts at launch, in headless runs or
through servers that serve their own clients. Their `--help` output names nothing that
addresses a running interactive session; their extension APIs were not examined. cones never
approximates delivery by typing into a terminal. See [comms](cli.md#comms).

## Reports

| Value | Claude Code | Codex | pi |
| --- | --- | --- | --- |
| Session start | First transcript timestamp; registry `startedAt` is not read. | Process start; detached threads use rollout `session_meta.timestamp`, then saved launch time. | Process start, since continuing an older file does not start a new process at that file's timestamp. |
| Last activity | Last transcript timestamp; registry/job `updatedAt` is not used. | Timestamp on the last rollout line. | Timestamp on the last session entry. |
| Title | Transcript `custom-title` or user-set `agent-name`, then `ai-title`; otherwise background job `name`, registry `name`, then first instruction line. Bare job/session ids do not count as names. A `custom-title` outranks an `agent-name` written after it, since a running session re-appends its own every turn. Dashboard and [CLI](cli.md#renaming-a-session) rename use native `/rename`; a manual name for an external interactive client appends `custom-title`. | `threads.name` (set over the daemon's `thread/name/set` on rename), then `threads.title` in `state_*.sqlite`, then legacy `session_index.jsonl`'s `thread_name`, then first rollout `UserMessage`. Database reads use the bundled SQLite library in process. | Last `session_info.name`, which rename sets through the reporter's `setSessionName`, otherwise first instruction line. |
| Last reply | Background job `detail`, otherwise first line of last assistant text. | Last assistant `response_item` message's `output_text`. | Last `text` block of the last assistant message. |
| Input/output totals | Transcript `message.usage`, counted once per message id. Input includes cache reads and creation. | Latest `token_count.info.total_token_usage.input_tokens` and `output_tokens`; input includes cache reads. | Sum assistant `usage`: input + cacheRead + cacheWrite for input, output for output. `totalTokens` is not used. |
| Context tokens | Last real message's input + cache creation + cache read, the same usage fields as statusLine. | Latest `token_count.info.last_token_usage.total_tokens`. | Those same three input counters on the last assistant message, as in pi's status line. |
| Context window | Saved statusLine payload, described below. | Latest `token_count.info.model_context_window`. | Absent; the catalog's model window is not written in session files. |
| Model | Last message with usage, `message.model`. | Latest `turn_context.model`. A later `thread_settings_applied` naming another model is `next_model` until the next `turn_context`, and the row shows `GPT-6 Astra → 5.6 Sol`; `model` stays the running turn's, since the running turn keeps its model and its price. | Last assistant message's `model`; `model_change` entries are not used. |
| Plan | Last message with usage: a `msg_bdrk_` id is `bedrock`, `msg_vrtx_` is `vertex`; any other id is first-party and shows the signed-in account's `oauthAccount.organizationType` from `.claude.json` without its `claude_` prefix (`pro`, `max`, `team`, `enterprise`), or `api` with no account. The account is machine-wide, so a session started with `ANTHROPIC_API_KEY` while signed in shows the account's plan. Foundry ids are unverified and read as first-party. | `rate_limits.plan_type` on the latest `token_count` when reported, the ChatGPT plan such as `plus` or `pro`; otherwise `session_meta.model_provider`. An API key reports no plan type, so it shows `openai`. | Last assistant message's `provider`. A subscription signed in through pi shows its provider, such as `anthropic`, not the plan. |
| Session cost | Saved statusLine `cost.total_cost_usd`, including reported zero, takes precedence over [response accounting](#cost-estimates). A [supervised run](jobs.md#results) reads the same two sources, since its session is an ordinary background session. | [Calculated estimate](#cost-estimates) from reported usage and cached provider/model prices, prefixed `~`. | Reported response costs and [fallback estimates](#cost-estimates) use the same accounting path as every harness. |

Claude `<synthetic>` messages are skipped. Missing usage and activity remain absent.
OpenCode reads title/activity from `session.title`/`time_updated`, sums assistant input/cache
and output counters, and uses the latest assistant `providerID/modelID`; `providerID` is its plan. Its context is the
latest assistant with output: input, output, reasoning and both cache counters. No context
window is stored. A valid native session cost takes precedence over response accounting;
last reply is the first visible line of the latest assistant text.

Model display names use the Bedrock provider catalog, normalized across regions/revisions.
Known uncatalogued families derive spelling from their ids; unknown families and bare aliases
stay verbatim. [Naming code](../src/fleet.rs) owns the mapping. Plans show provider ids
verbatim except `amazon-bedrock`, shown as `bedrock`, and `google-vertex…`, shown as `vertex`.

### Cost estimates

All live/history readers use shared response accounting. A valid native session total,
including zero, wins. Otherwise each response uses native cost, falling back to reported
provider/model and disjoint usage counters with cached prices. Zero response costs require
explicitly empty usage because some harnesses write zero for unpriced responses. Duplicate
response ids count once. Missing identities, counters or prices leave gaps.

Claude writes no provider into its transcript. A reply's id names the endpoint that served it,
so a first-party `msg_` reply is priced as Anthropic's; a Bedrock or Vertex reply needs the saved
statusLine cost, because those catalogs key prices by model ids the transcript does not carry.
cones does not infer a provider from settings or model names. Some Claude Code 2.1.278 replies
zero every top-level counter and report the counts only under `usage.iterations`; cones reads
those replies as the iterations' sum. Codex prices requests at their own
model/input size, counts cumulative updates once and includes reasoning in output. OpenCode
adds its separately stored reasoning to output and requires all five native counters for
fallback pricing. Hour-long cache writes, which Claude and pi report for Anthropic replies,
are priced at twice the model's input rate, Anthropic's published multiplier, because the
catalog has no rate for them. Reported nonstandard Codex service tiers remain unpriced unless
native dollar amounts exist.

The models.dev catalog uses exact provider/model keys and USD-per-million-token rates, applying
context tiers to whole requests. All-zero rates are unpriced. Estimates do not reconcile
subscriptions, discounts or invoice history. The dashboard refreshes prices
after 24 hours, retries failures after an hour and rejects snapshots older than seven days.
Validated snapshots replace `STATE_DIR/prices.json` atomically and invalidate accounting caches.
Rendering never downloads; internal listings only use existing cache data.

Calculated totals show `~$…`; partial totals show only the priced subtotal, and unavailable
values show `-`. Details and JSON `cost_info` expose source, coverage, gaps and catalog metadata.
A resumed run displays its live session's cost while preserving the ledger total; without a
live session, the ledger precedes saved-output accounting.

### State

| Harness | Mapping |
| --- | --- |
| Claude interactive | Registry `busy`: working; `shell`: shell, a finished turn with a background shell still running; `waiting`: input; `idle`: idle. Other words are preserved. |
| Claude background | Precedence below, following native listing behavior in 2.1.272. |
| Codex | Daemon threads: [native runtime state](#native-input-signals), including approvals and questions. Otherwise rollout `task_started`: working; `task_complete`: done; `turn_aborted`: stopped; no rollout means `-`. |
| pi | User/tool-result: working. Assistant `stopReason`: `toolUse` working, `stop` idle, `aborted` stopped, `error` failed. Unknown/no turn: `-`. Owned clients report native idle/working and dialogs as [input](#native-input-signals). |
| OpenCode | Owned reporter: busy/retry/idle and pending questions/permissions. External process rows: `-`; saved messages cannot establish live state. |

Claude background uses the first matching rule:

1. Registry `busy`: working, and `shell`: shell, ahead of any previous job result.
2. Job `done`/`failed`/`stopped` with tempo no longer `active`: that state. Done also requires
   no routine, self-wake, in-flight `session_cron` or `monitor` that could restart it.
3. Tempo `blocked` or registry `waiting`: input.
4. Registry `idle`, no queue, no in-flight work other than a `session_cron` wake or a
   `monitor`, and tempo `idle` or a `done` job: idle, even when the job still reports
   `working` while it waits on another agent, or a done job sleeps until a scheduled wake or
   keeps tempo `active` beside a running monitor. Its reported detail stays on the row.
5. Otherwise working.

Quiet output does not override these reports. No other harness reports a turn that ended
beside a running background shell: Codex 0.163's thread status carries only approval and
input flags, so its sessions, like pi's and OpenCode's, stay working or idle as reported.

### Native input signals

Local Codex daemon threads are read with `thread/read` over the native home's
`app-server-control/app-server-control.sock`. `waitingOnApproval` and `waitingOnUserInput`
mean input; active, idle and system error map to working, idle and failed. Reads start no
daemon, resume or subscribe to no thread and answer no request. A failed or unsupported read
falls back to that refresh's rollout state without keeping an earlier wait. Standalone
clients and custom server sockets have rollout reporting only.
[Native protocol](https://developers.openai.com/codex/app-server/).

Owned pi launches, forks and resumes add a private `--extension`, keeping native settings and
other extensions. It reports `ui_prompt_start`/`ui_prompt_end`, native idle state and the
session id and file; nested dialogs stay input until all close, and native identity keeps
clients sharing a cwd apart. It registers no tools or input handlers. Reports refresh every
second and expire after five; a failed write withdraws the state and leaves the client
running. The detached host keeps reporting after the dashboard closes. Running clients must
be reopened through cones to load it.
[Event contract](https://github.com/YuvalSarel1/pi/blob/61716b03c944d3096a94d0e7ab7dd666b4a370ec/packages/coding-agent/src/core/extensions/types.ts#L745-L761).

Verified with Codex 0.156.1 and pi 0.85.1+bedrock-images.61716b03c; older native APIs may lack
these signals.

### Context window for Claude

Only statusLine stdin reports `context_window.context_window_size`. cones reads a saved
payload at `<Claude home>/statusline/<session_id>.json`, also using valid
`cost.total_cost_usd` and `effort.level`. To supply it, add this after `input=$(cat)` in the
statusLine command:

```sh
claude_dir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
mkdir -p "$claude_dir/statusline"
printf '%s' "$input" > "$claude_dir/statusline/$(jq -r .session_id <<<"$input").json"
```

Use the same native home as cones. Without the payload, context shows prompt tokens only.
cones does not modify Claude settings or the statusLine command.

### Activity

Each timestamped transcript line contributes one `lines` count. Other [metrics](jobs.md#activity):

| Metric | Claude | Codex | pi |
| --- | --- | --- | --- |
| `messages` | Assistant usage lines, once per message id. | Assistant `response_item`. | Assistant entries. |
| `tools` | `tool_use` blocks. | Function/custom-tool/local-shell calls. | `toolCall` blocks. |
| `tokens` | Assistant `output_tokens`. | `last_token_usage.output_tokens`. | Assistant `usage.output`. |

OpenCode activity charts are not implemented.

## Native actions

Native daemon sessions can be joined without ending the agent. External interactive terminals
without an attach protocol say `own terminal`. Dashboard stop verifies a process's harness
before signaling it. The [CLI stop command](cli.md#stopping-a-session) has narrower support.

| Session | Open | Dashboard stop/removal |
| --- | --- | --- |
| Claude background | Native attach. Settled jobs require explicit entry because attach wakes them. | `claude stop`, then `claude rm` removes the job record; a refused stop removes nothing. `claude rm` preserves the transcript. A signal alone permits daemon respawn. When `claude rm` keeps a session for unpushed worktree commits, cones discards the worktree only if it is clean and another branch already holds its head; otherwise the delete fails and names the unmerged work. |
| External interactive client | Refused: own terminal. | SIGTERM to the verified process. |
| Codex daemon thread | Remote native resume, including with another client attached. | Stop its turn and background terminals through the daemon when one runs, then hide/forget its saved row and close its client. The daemon keeps the thread. |
| Owned interactive terminal | Reconnect to its host. | Stop the host and client. |
| Supervised run | Join a live session or follow output. | Stop the native session and worker group. |
| Finished Claude run | Join if live, otherwise background resume and attach. | Hide the run; retain output. |
| Claude history | Background resume and attach. | No history deletion. |
| Codex history | Unarchive if needed, then remote resume. | No history deletion. |
| pi history | `pi --session <transcript>`. | Stop the resumed host; retain history. |
| OpenCode history | `opencode --session <id>`. | Stop the resumed host; retain history. |
| Copilot or Cursor history | `copilot --resume <id>` or `cursor-agent --resume <id>`. | Stop the resumed host; retain history. |

Hosts retain process, screen and draft across dashboard closure, with one attachment at a time
and ownership proven by a held lock. Host/machine restart ends their processes. Claude and
Codex daemon sessions retain native ownership; closing an attach client leaves the session.
History resumes in its recorded cwd/home. Forks create native identities in the same folder;
Claude forks are interactive rather than background sessions. A Codex fork is made by the
daemon's `thread/fork`, which answers with the new thread, and a client joins it with remote
resume. A remote Codex client refuses permission flags on resume and fork ("Permission
overrides are not supported when resuming a remote task", 0.154 to 0.163), so a resumed thread
keeps the settings it was started with, and the fork takes the
[skip setting](jobs.md#composer-harnesses) as its approval policy and sandbox in that call.

For native shell commands, invoke the binary directly: aliases adding flags can turn a Claude
subcommand into a prompt. Removed Claude jobs remain in native resume history; forgotten Codex
threads remain resumable.

### Folder environment

A start, fork or resume runs the user's `$SHELL -l -i` in the session folder first and passes on
every exported value that differs from cones' own environment, so rc files, `chpwd` hooks and
direnv choose the provider, profile or region the same way a `claude` typed in that folder would.
Configured `bedrock`, `aws_profile` and `aws_region` still win, and host identity stays out.
Aliases and shell functions are not run, but an alias named after the harness lends the
`NAME=value` words it starts with, so `alias claude='CLAUDE_CODE_USE_BEDROCK=1 claude'` reaches
the launch and its flags do not. Values that need expansion stop the read, and a function's
variables never apply; put those in an exported rule or the folder's native settings. A shell that
fails or takes over five seconds contributes nothing. Claude's background daemon hands a launch the
Bedrock switch and `AWS_` values but not other names, which reach foreground harnesses alone, and
a session reopened with `claude attach` runs on the daemon's own environment, whatever the folder
exports. Only native settings reach both. Scheduled jobs keep their
[cleared environment](jobs.md#environment).

### Composer identity

| Harness | Launch | Attribution |
| --- | --- | --- |
| Claude | `claude --bg -- <instruction>`, after accepting the folder's trust dialog in Claude's config; when that config cannot be written and Claude refuses the folder, the same command without `--bg` in the pane, so Claude asks its own trust question. | Returned short id matched to registry/folder; the untrusted-folder start by child PID; unreported launches expire after 90 seconds. |
| Codex | Find/start daemon, open remote client with instruction. | Child PID, then exactly one new thread matching folder/start/prompt without competing unresolved launches. |
| pi | `pi -- <instruction>`. | Child PID/harness/folder; the session file names its conversation. |
| OpenCode | `opencode --prompt=<instruction>`. | Child PID/harness/folder; the owned reporter names its conversation. |

Before `claude --bg`, cones sets `hasTrustDialogAccepted` on the folder's `projects` entry in
Claude's `.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` when that is set, else `~/.claude.json`),
which is what accepting Claude's dialog records. Only that entry changes; nothing is written when
the folder or one above it is already trusted, or when the file is missing. The file is replaced
whole with its keys in sorted order, and a Claude saving its own copy at the same moment can drop
the entry, in which case that launch falls back to the pane question.

Before a Codex client starts, cones saves the folder as trusted through the daemon's
`config/batchWrite`, the write Codex's own "Trust and continue" makes, so a client with no
screen does not stop on that question. A folder Codex's config already names, trusted or
untrusted, keeps its setting, and a daemon that refuses the write leaves the question to the
client. Codex 0.163.0 asks a remote client's trust question from the daemon's config, so a
`-c projects...` override on the client does not answer it. Other foreground clients show
their own trust question in the pane before the instruction runs.

A harness whose launch identity is `client_pid` keys its row by the process, `<harness>-<pid>`,
for every client, whoever started it. The conversation it reports is an attribute of that row:
it is absent until pi's first reply, and `/new`, `/resume`, an in-app fork or a second client
in the folder change or withdraw it without adding or removing a row. History, forks and
coordinator claims use the reported conversation. Discovery never lists a `--version` or
`--help` probe this dashboard is running; a probe another cones process runs can still appear
for the moment it takes.

Codex daemon startup returns `socketPath` and is idempotent. The remote client reports no
initial thread id, so ambiguous launches keep their viewer rows and attribution retries on
later refreshes. Closing an unidentified client may leave no saved row.

### Detached launch

[`cones launch`](cli.md#starting-a-session) uses the harness's daemon or the composer's
persistent host.

| Harness | Returned identity | Lifetime |
| --- | --- | --- |
| Claude | Full native session id resolved from the returned background id. | Native conversation. |
| Codex, with a prompt | Thread id, once its first turn is reported. | Daemon thread; the launching client is stopped. |
| Codex, no prompt | Refused: Codex keeps no thread before a first turn, so no id would last. | Nothing is started. |
| OpenCode | `opencode-<pid>`, or the stored host id while discovery reports none. | Hosted client lifetime. |
| pi | `pi-<pid>`, or the stored host id while discovery reports none. | Hosted client lifetime. |
| Experimental launchers | Process row, or stored host id when discovery supplies none. | Hosted client lifetime. |

Detached identity uses the owned background id or child PID, never prompt/cwd matching.
Codex is the exception. Its daemon returns a new thread's id only to the client that asked,
and the client's own message id is one it invents, so nothing a second process can read ties
a thread to its launch. The thread is the one new thread matching folder, start and prompt.
Two that match are refused, and so is one that matches while another launch of that prompt
into that folder, submitted within the last 20 seconds, is still unnamed in the launch ledger;
overlapping identical launches therefore all fail unnamed and keep running. The same prompt
typed into Codex by anything that writes no ledger line is not told apart. Claude and Codex
naming wait up to 20 seconds; a Codex client stuck on a sign-in screen reports no thread and
stays hosted. Once its thread appears, the thread is the folder's row and the client gets none.
Hosted launches wait up to five seconds for discovery,
then up to 20 for a stored host identity if needed. An unnamed launch fails without stopping
the session. Input waits remain native and require user action.

A hosted launch watches its client for three seconds, and until it is named. A client that
exits in that time fails the launch with its exit status, the last screen that had text on it
and the first line of its stderr; no id is printed and no host remains. A client that exits
later is not caught. A client that stays alive is named whatever it shows: no harness
definition declares its sign-in, trust or update screens, so cones does not recognise them.
When the harness has reported no conversation of its own, stderr carries the client's screen
for the caller to read, and a prompt the client has not taken is otherwise indistinguishable
from one it has. A dashboard cannot open the terminal during the watch. Codex launches with a
prompt are not watched this way.

CLI stop resolves a host's saved id or current discovered id through harness and live client
PID. Ambiguous matches are refused. Codex has no stop command, so a daemon thread is stopped
over its home's control socket, with or without a client attached: `turn/interrupt` for the
turn `thread/turns/list` reports in progress, then `thread/backgroundTerminals/clean`, because
an interrupted command keeps running as a background terminal. Stop returns once the daemon
lists neither, and a thread running nothing succeeds with nothing stopped. Attached clients stay
open, queued notes stay queued, and the thread remains loaded and resumable. The terminal
calls need the protocol's experimental capability. A `codex-<pid>` or host id stops only that
client, which leaves a turn it started running in the daemon; stop the thread id instead.

Detached Claude launch was verified with 2.1.278, and Codex with 0.162.1: the thread finished
its turn and took a later note with no client attached, and a stop with no client attached
interrupted a turn and ended the command it was running. Older Codex protocols and a stop
during an approval wait are unverified. pi 0.85.1 and OpenCode 1.18.31 were
verified against a loopback provider: launched with no terminal, each took the instruction as
its first user message, `cones ls` and `cones stop` took the returned id, a dashboard started
afterwards listed the session and opened its host, the host outlived that dashboard, and the
stopped conversation resumed from history with its earlier turns. `cones show` takes their
`native_id`, not the returned process id. Gemini 0.60.0, Cursor Agent 2026.09.18-9a7762b,
Copilot 1.0.95, Amp 0.0.1789790452-gad4023 and Droid 0.223.0 were launched the same way with no
login and no network: each was named, listed, opened from a later dashboard and stopped. None
got past its trust or sign-in screen, so whether it takes the instruction remains unverified;
Amp's sign-in question consumes it. Detached resume of a Claude or Codex launch remains
unverified.

Model/provider settings follow [configuration](jobs.md#composer-harnesses). Claude and pi take
effort flags; Codex and OpenCode receive none. Codex's daemon keeps its configured provider,
so a different provider region requires another native home. Permissions remain native.

### Supervised execution

Claude and Codex have execution adapters. Claude validates native version `>=2.1, <3` and
required flags before using the [run contract](jobs.md#what-the-harness-is-told). A Codex job
is a daemon thread started the way `cones launch` starts one; it was verified on Codex 0.163
for a manual and a scheduled run, a timeout, a stop and an orphaned supervisor. Codex reports
no separate request for input, so a Codex run never ends `input`. Jobs selecting any other
harness are rejected when read; pi and OpenCode supervised execution remain unverified. Discovery and interactive controls do not
require an execution adapter. See [integration requirements](harness-definitions.md#changing-or-adding-a-harness).

## Additional terminal harnesses

These adapters offer launch, process discovery, Ctrl+Z return, viewer reuse and stop. They
have no external-terminal join, message delivery or supervised execution. State, model,
context, tokens, cost and last reply remain absent. Model selection only supplies a launch
flag. No hooks or native configuration rewrites are installed. A session cones hosts can be
read as its terminal screen with [`cones show`](cli.md#reading-a-conversation); one in a
terminal cones does not own, with no saved conversation, has nothing cones can read.

Copilot and Cursor Agent conversations are listed in history and search, printed by
`cones show` and resumed by their native id, with no state, usage or fork. Both readers were
verified from files the installed CLIs had already written, with no model turn. Copilot is
read from `session-state/ID/events.jsonl`; a live row names its conversation as `native_id`
from the `inuse.PID.lock` the client holds, ignoring a lock older than the process. Cursor
is read from `projects/*/agent-transcripts/ID/ID.jsonl`, which stamps no message, and takes
its folder and start from `chats/*/ID/meta.json`; a transcript without that record is not
listed, and a live Cursor row names no conversation. Gemini, Droid, Amp and Kimi
conversations are unsupported: Gemini's chat file is a log of `$set` updates and re-emitted
message ids whose replay is unverified, no Droid file here holds an assistant reply, Amp
keeps threads on its server and Kimi wrote no home.

| Harness | Interactive instruction | Model flag | Checked CLI |
| --- | --- | --- | --- |
| Gemini | `--prompt-interactive=` | `--model` | 0.60.0 |
| Cursor Agent | Positional after `--` | `--model` | 2026.09.15-d2fe57e |
| Copilot | `--interactive=` | `--model` | 1.0.86 |
| Amp | Anonymous stdin file | Native config | 0.0.1789704050-g778045 |
| Droid | Positional after `--` | Native config | 0.222.0 |
| Kimi | None; an instruction is refused | `--model` | 2.1.1 |

Native boot, process identity, return/reuse and stop were checked with disposable homes.
Resume was checked against Copilot 1.0.95 and Cursor Agent 2026.09.18-9a7762b, each
reopening a saved conversation with no model turn. Model-backed turns, permission questions,
live conversation reporting and opening a Copilot or Cursor history row in the dashboard
remain unverified.
These are not full harness integrations. Kimi's `--prompt` answers and exits, and 2.1.1
refuses it beside `--auto`, so Kimi starts only without an instruction. Kimi replaces its process title with `Kimi Code`,
so management commands using that title cannot be distinguished. Tab/Left remain native;
configured AWS profile/region are passed as for other launches.

Native references: [Gemini](https://geminicli.com/docs/cli/cli-reference/),
[Cursor](https://cursor.com/docs/cli/reference/parameters),
[Copilot](https://docs.github.com/en/copilot/reference/cli-command-reference),
[Amp](https://ampcode.com/docs/cli), [Droid](https://docs.factory.ai/reference/cli-reference),
[Kimi](https://moonshotai.github.io/kimi-cli/en/reference/kimi-command.html).
