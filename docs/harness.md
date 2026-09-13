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
| Discover live sessions | reported: `~/.claude/sessions/<pid>.json`, one per session, bg or interactive. `$CLAUDE_CONFIG_DIR` relocates it. | unknown |
| Liveness proof | reported: registry `pid` and `procStart`, the process start time as `ps -o lstart` prints it under UTC. cones compares that text with the live process table, so a reused pid is not a session. | unknown |
| Working directory | reported: registry `cwd` | unknown |
| Kind (background or interactive) | reported: registry `kind` | unknown |
| State (working, idle, needs input) | reported: registry `status`: busy, shell, idle, blocked, waiting, needs_user, needs_trust. Any other value renders as the word itself. | unknown |
| Last update time | reported: `~/.claude/jobs/<id>/state.json` `updatedAt`, else registry `updatedAt` or `startedAt`. An entry with none is skipped. | unknown |
| Transcript path | deduced: `projects/<cwd with every non-alphanumeric byte as '-'>/<sessionId>.jsonl`, Claude's internal layout. Background jobs report `linkScanPath` in state.json; interactive sessions report nothing. | unknown |
| Title | reported: transcript `ai-title` or `agent-name`, else registry `name` | unknown |
| Last reply | reported: state.json `detail` for background jobs, else the transcript's last assistant text | unknown |
| Tokens in and out | reported: transcript `message.usage`, summed once per message id | unknown |
| Context tokens at the last turn | reported: the last message's `input_tokens` plus cache creation and cache read, the fields Claude's statusLine `current_usage` carries | unknown |
| Context window size | `-`: stated only in the statusLine stdin JSON (`context_window.context_window_size`), which reaches nothing outside the session. Transcript, registry, hook payloads and `claude agents --json` have none. | unknown |
| Cost | `-` for sessions, the transcript records tokens and no price. Reported for cones runs from the result event `total_cost_usd`. | unknown |
| Model | reported in the transcript as the bare API id. Not shown. | unknown |

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

## Control

What `stop`, `attach`, `logs` and the timeout need.

| Need | Claude Code | Codex |
| --- | --- | --- |
| Stop an interactive session | reported: the registry pid. cones checks the process name with `ps` before SIGTERM. | unknown |
| Stop a background session | reported: `claude stop <id>`. The daemon respawns a killed worker, so a signal is not enough. | unknown |
| Kill a run at the timeout | cones owns it: SIGTERM to the process group, SIGKILL two seconds later | unknown |
| Attach to a session | reported: `claude attach <id>` in its cwd | unknown |
| Read a session's output | reported: the transcript, see Observe | unknown |

## Open

One row is still deduced: the transcript path for interactive sessions. Nothing reports it. Until Claude does, the layout rule stays, and a missing file renders `-`, never a guess.

Codex is parsed and refused at validation ([jobs.md](jobs.md#codex-parsed-refused-at-validation)). Its column fills in as each row is checked against a real Codex binary; the adapter lands when no row is unknown.
