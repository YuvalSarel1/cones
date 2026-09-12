# cones

cones schedules Claude Code jobs on a Mac. launchd fires each job on a cron schedule, Claude runs it headless under a policy Claude itself enforces (dollar budget, turn cap, tool allowlist, read-only or sandboxed write, no MCP, no prompts) plus a timeout cones enforces, and every run lands in a JSONL ledger with a status and a reason. One global hook also puts every Claude session on the Mac, scheduled or interactive, into `cones ls` and a dashboard.

Current version: v0.1.0. Scheduling is launchd's; cones runs no process between ticks.

cones is built for three things the agent fleet tools around it leave out:

| Gap | What cones does |
| --- | --- |
| Fleet tools list only the sessions they launched. | A global Claude Code hook writes one state file per session, so every Claude session on the Mac shows in `cones ls` and the dashboard, including sessions started from a terminal. |
| Dollar budgets and tool policy are left to the user. | `budget_usd` compiles to Claude's own `--max-budget-usd`; `tools` and `write` compile to `--tools` and `--allowedTools`; a run cannot prompt, load settings or reach an MCP server. |
| Scheduling needs the tool's own daemon. | Five-field cron compiles to `StartCalendarInterval` in a per-user LaunchAgent. |

The ownership rule, rule 1 of [AGENTS.md](AGENTS.md): cones owns the clock, supervision, budgets, locks and the ledger. Claude owns execution and permissions. Every tool call goes to Claude's own permission engine, and a guarantee Claude cannot enforce natively is a `cones validate` error.

## Install cones and check the Mac

Requires Rust and Claude Code.

```sh
cargo install --git https://github.com/YuvalSarel1/cones --tag v0.1.0
```

Or from a checkout:

```sh
git clone https://github.com/YuvalSarel1/cones && cd cones
cargo install --path .
cones --version                  # cones 0.1.0
```

Verified against Claude Code 2.1.269 on macOS 26.6.1. To repeat the check, run `cargo test --all-targets` in the checkout and `cones doctor` after installing.

## First job: validate, run, read the ledger

A job is one prompt, run in one directory, on one cron schedule, under one policy. `jobs.example.yaml` is a working read-only job; `jobs.yaml` is gitignored.

```sh
cp jobs.example.yaml jobs.yaml
cones validate
cones doctor
cones run readme-check
```

Real output, ids and paths shortened:

```
$ cones validate
readme-check	valid	claude
$ cones run readme-check
173c4d8b-...	started	readme-check
173c4d8b-...	ok
$ cones logs 173c4d8b-...
Read  ~/personal/cones/README.md
1	# cones
...
Result: success  $0.018371
$ cones ls
173c4d8b-...	readme-check	ok	2026-09-12T09:09:25+00:00	claude	$0.02	-
2b2aa8d2-...	~/personal/cones	active	2026-09-12T13:54:25+00:00	claude	-	8.1M/45k
8077985c-...	~/personal/cones	idle	2026-09-12T13:54:23+00:00	claude	-	40.6M/157k
```

`cones ls` prints runs newest first, then every live Claude session on the Mac; columns are under Command reference. `cones tui` shows the same jobs, sessions and runs as a dashboard. When the schedule looks right, `cones install --dry-run` prints the launchd plists, `cones install` writes them, `cones uninstall` removes them and keeps history. Installs are idempotent. `cones hook --install` adds the fleet hook once.

### Run a prompt without a job

```sh
cones run --prompt "fix the flaky test"     # under the first job's policy, in the current directory
cones run nightly-triage --prompt "..."     # under a named job's policy
```

With no jobs file, or one that does not parse, the task runs under the read-only defaults (30 minutes, $2.00, Read/Grep/Glob); run `cones validate` first when you expect a job's policy. Each task gets a unique `adhoc-<8 hex>` name, so `overlap` is checked per task. The writer lock is checked per directory as for any job.

## The job file: one prompt, one directory, one schedule, one policy

`jobs.yaml` is `version: 1`, an optional `defaults` block, and a list of jobs. `defaults` accepts the policy fields `timeout_min`, `budget_usd`, `daily_budget_usd`, `write`, `tools`, `max_turns`, `overlap`, `notify` and `codex_full_access`; each job may override them. Unknown fields anywhere in the file are rejected.

```yaml
version: 1
defaults:
  timeout_min: 30
  budget_usd: 2.00
  daily_budget_usd: 10.00
  write: false
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"          # five-field cron, compiled to launchd
    harness: claude
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    write: true                    # false removes Edit, Write and Bash
    tools: ["Read", "Grep", "Glob", "Edit", "Write"]
    model: sonnet
    max_turns: 5
    overlap: skip                  # skip | allow | replace
    notify: true                   # macOS notification when a run fails, times out or is skipped on budget
    # env: ["ANTHROPIC_API_KEY"]   # only named variables reach the job
```

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | 1-80 ASCII letters, digits, `-` or `_`; unique in the file. Becomes the launchd label `local.cones.<name>` and Claude's session `--name`. |
| `schedule` | required | Five-field local-time cron: minute, hour, day, month, weekday (0 or 7 is Sunday). Lists, ranges and steps work. At most 4096 launchd intervals. |
| `harness` | required | `claude`. `codex` parses and is refused at validation (see Codex below). |
| `cwd` | required | Working directory. `~/` expands, a relative path resolves against the jobs file's directory, and it must exist. |
| `prompt` | required | The task. Nonempty; passed after `--` on the command line. |
| `model` | Claude's default | Passed as `--model`. |
| `enabled` | `true` | `false` records each tick as `skipped` with reason `disabled`, and `cones install` removes that job's LaunchAgent. |
| `archive_transcript` | `false` | Copy Claude's transcript into `~/.cones/transcripts/<run_id>/<session_id>.jsonl` when the run ends. |
| `env` | `[]` | Names of shell variables to pass through. Values are read at install or run time and baked into the plist; nothing else from your shell reaches the job. |
| `timeout_min` | `30` | Runner timeout. Positive, at most 10080 (one week). |
| `budget_usd` | `2.00` | Per-run cap, passed as `--max-budget-usd`. |
| `daily_budget_usd` | none | Rolling 24-hour cap per job. At least `budget_usd`. |
| `write` | `false` | `false` strips Edit, Write and Bash from the compiled allowlist even if `tools` lists them. `true` keeps them and turns on Claude's sandbox when Bash is listed. |
| `tools` | `Read, Grep, Glob` | Any of Read, Grep, Glob, Edit, Write, Bash, or a `Bash(pattern)` rule. |
| `max_turns` | none | Passed as `--max-turns`. |
| `overlap` | `skip` | `skip`, `allow` or `replace`: what a tick does while the previous run is still going. |
| `notify` | `false` | macOS notification (`osascript`) when a run is `failed` or `timeout`, or `skipped` with reason `budget`. `CONES_NOTIFIER` names a command that receives the title and message instead. |
| `codex_full_access` | `false` | Codex only. Rejected on a Claude job. |

`cones validate` compiles every job's policy and prints `<name>  valid  <harness>`, or the first error with the job's name. Beyond the per-field rules it rejects:

- `overlap: allow` with `write: true`. Two writers in one directory would need a worktree per run, which is not implemented; use `skip` or `replace`.
- `Bash(pattern)` rules other than `Bash(*)` with `write: true`. Claude treats a scoped Bash rule as a pre-approval, so other commands still reach ordinary permission checks; use `Bash` for sandboxed Bash, or stay read-only. A read-only job strips the rules with the rest of Bash.
- A schedule that restricts both day and weekday while one uses a wildcard step. launchd ORs the two fields where cron ANDs them.
- An `env` name that could change execution policy: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, or anything starting with `DYLD_`, `LD_` or `CLAUDE_CODE_`. Names must be valid shell identifiers.
- `version` other than `1`, a duplicate name, a `cwd` that is not a directory, `daily_budget_usd` below `budget_usd`, `max_turns` or `tools` on a Codex job, an unknown tool name, a `claude` binary missing from the launchd PATH.

## What the harness is told

The job compiles to one `claude` command with a fixed argv. `cones validate` and `cones doctor` both compile it, and `cones doctor` checks each flag against `claude --help`. The compiled argument list, its hash and the resolved policy are stored in the run's `started` record.

| Guarantee | Claude flags |
| --- | --- |
| Headless, streamed events | `--print --output-format stream-json --verbose` |
| No prompts | `--permission-mode dontAsk --permission-prompts none`. A denied tool call ends the run with reason `permission`. |
| Safe mode, restricted | `--safe-mode --restricted` |
| No user or project settings | `--setting-sources ""` |
| No MCP servers | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'` |
| No slash commands | `--disable-slash-commands` |
| Tool allowlist | `--tools <bases> --allowedTools <rules>`, from `tools` and `write` |
| Pinned session ID | `--session-id <fresh uuid>`. cones generates it, so the ledger, the transcript, `attach` and the fleet agree; a mismatch ends the run with reason `session_mismatch`. |
| Dollar budget | `--max-budget-usd <budget_usd>` |
| Job name | `--name <job>` |
| Sandbox when Bash is allowed | `--settings` with `sandbox.enabled` and `sandbox.failIfUnavailable` true, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands` false, `excludedCommands` empty |
| Model, turn cap | `--model <m>`, `--max-turns <n>` when set |
| The task | `-- <prompt>` as the final positional argument |

The timeout is the runner's: at `timeout_min` the whole process group gets SIGTERM, then SIGKILL after two seconds. The worker also ends itself one second past the timeout or when its supervisor disappears.

The harness starts with a cleared environment: `HOME`, `USER` and `TMPDIR` from the installing shell, a fixed `PATH` (`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`) from which `claude` is resolved, `LANG=en_US.UTF-8`, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, and the names listed in `env`. `cones install --dry-run` prints the plist XML including those values, so it can contain secrets; it warns on stderr when a job imports any.

## What a run records

A run is one supervised harness process. `cones run` takes a global admission lock, reaps runs whose worker died as `orphan`, applies the skip rules, appends a `started` record, spawns a worker in its own process group, and reads the harness's JSON events until the process exits, the timeout passes, a permission is denied, or a stop arrives. On timeout, stop, replace or permission denial the whole process group is terminated: SIGTERM, two seconds, SIGKILL.

| Status | Reason | When | Exit |
| --- | --- | --- | --- |
| `started` | | Running. Dollars show as `-` until it ends. | |
| `ok` | | Claude exited 0 with a `success` result and a reported cost. | 0 |
| `skipped` | `disabled` | `enabled: false` | 0 |
| `skipped` | `overlap` | `overlap: skip` and a previous run is still going | 0 |
| `skipped` | `budget` | The daily reservation would exceed `daily_budget_usd` | 0 |
| `skipped` | `workspace` | Another writer holds the directory's lock | 0 |
| `skipped` | `replace_unconfirmed` | `overlap: replace` and the previous run did not confirm shutdown within 10 seconds | 0 |
| `timeout` | `timeout` | The runner's clock ran out | 124 |
| `timeout` | `replaced` | This run was the old one under `overlap: replace`; it received SIGUSR1 and stopped | 124 |
| `failed` | `interrupted` | `cones stop`, or SIGTERM / SIGINT to the runner | 1 |
| `failed` | `permission` | Claude reported a permission denial, or a sandboxed Bash call hit `Permission denied`, `Operation not permitted` or `Read-only file system` | 1 |
| `failed` | `session_mismatch` | An event carried a session id other than the pinned one | 1 |
| `failed` | `missing_result`, `missing_cost` | Claude exited without a result event, or with one that had no `total_cost_usd` | 1 |
| `failed` | `validation: ...`, `spawn: ...`, `runner: ...` | The policy did not compile, the worker could not start, or cones hit an error while supervising | 1 |
| `failed` | `exit`, or Claude's result subtype | Nonzero exit with no other explanation, or a result other than `success`, recorded verbatim | 1 |
| `failed` | `orphan` | The supervisor died; written when the next run of that job, or a writer on the same directory, reaps it | |
| `crashed` | | Derived at read time: a `started` record with no terminal record past its timeout plus five seconds. | |

The `started` record has `trigger` (`manual` or `schedule`), `session_id`, `cwd`, `pid`, `pgid`, `timeout_s`, `budget_usd`, the compiled `policy` and its SHA-256 `policy_hash` (session id, job name and prompt normalized out, so a compiler flag change changes the hash). The terminal record has `duration_s`, `exit`, `tokens_in`, `tokens_out`, `cost_usd`, `reason` and, when archived, `transcript`.

State lives in `~/.cones` (`--state-dir` to isolate). Directories are created `0700` and files `0600`.

| Path under `~/.cones` | Contents |
| --- | --- |
| `runs.jsonl` | The ledger: one `started` and one terminal record per run. Appends hold an exclusive lock; a partial last line from a killed writer is repaired on the next append. |
| `output/<run_id>/events.jsonl`, `output/<run_id>/stderr.log` | Claude's stream-json events (64 MiB cap, 1 MiB per line) and stderr (1 MiB cap). |
| `transcripts/<run_id>/<session_id>.jsonl` | The archived transcript when `archive_transcript: true`. |
| `logs/<job>.out.log`, `logs/<job>.err.log` | launchd's stdout and stderr for the scheduled `cones run`. |
| `fleet/<session_id>.json` | One state file per Claude session, written by the hook. |
| `locks/admission/`, `locks/runs/`, `locks/workspaces/` | Admission, per-run and per-workspace lock files. |

## When a job is still running at its next tick

| `overlap` | Behavior |
| --- | --- |
| `skip` | The tick is recorded as `skipped` / `overlap`. |
| `allow` | Both run. Rejected with `write: true`. |
| `replace` | The previous run gets SIGUSR1 and ends as `timeout` / `replaced`; the new run starts once that is confirmed within 10 seconds, else it is `skipped` / `replace_unconfirmed`. |

## Budgets: a per-run cap Claude enforces and a rolling daily reservation

`budget_usd` is Claude's own `--max-budget-usd`. `daily_budget_usd` is a rolling 24-hour reservation per job: the ledger sums the job's runs from the last 24 hours, counting a run's actual cost when its record has one and its `budget_usd` while it is still going or when it ended without a reported cost, and a tick whose own `budget_usd` would push that sum over the cap is `skipped` / `budget`. The reservation is checked before a `replace` sends SIGUSR1, so a budget skip leaves the previous run going.

## One writer per directory, from jobs or from the shell

Writers on the same directory are serialized across jobs regardless of `overlap`: a `write: true` run takes `~/.cones/locks/workspaces/<sha256 of the canonical cwd>.lock` and a tick that finds it held is `skipped` / `workspace`. Read-only runs do not take it.

```sh
cones lock . -- git commit -m msg
```

`cones lock DIR -- COMMAND` takes that same writer lock from a shell or another agent, waits until scheduled writers on the directory finish, runs the command, releases, and exits with the command's status.

## Schedules on launchd: sleep, login and reboot

`cones install` writes one per-user LaunchAgent per enabled job and loads it with `launchctl bootstrap` in the `gui/<uid>` domain.

| plist key | Value |
| --- | --- |
| `Label` | `local.cones.<name>`, at `~/Library/LaunchAgents/local.cones.<name>.plist` |
| `StartCalendarInterval` | One entry per cron tick |
| `RunAtLoad` | `false` |
| `ProcessType` | `Background` |
| `WorkingDirectory` | The job's `cwd` |
| `ProgramArguments` | The installed `cones` binary with `--jobs <file> --state-dir <dir> run <name> --trigger schedule` |
| `StandardOutPath`, `StandardErrorPath` | `~/.cones/logs/<name>.out.log`, `.err.log` |
| `EnvironmentVariables` | The environment above, including the values of variables named in `env`, so a variable exported after install needs a reinstall |

`cones install` rewrites and re-bootstraps only plists whose content changed, bootstraps ones that are on disk and not loaded, and boots out the LaunchAgent of any job now disabled. `cones uninstall` boots out and deletes every `local.cones.<name>.plist` whose `Label` matches its file name, refuses to continue when one does not, and keeps every ledger record and transcript.

Per launchd.plist(5), ticks missed while the Mac sleeps coalesce into one launch on wake, so a wake starts at most one run per job and `overlap` decides if the previous run is still going; nothing runs at login or on `cones install`. Ticks that pass while the Mac is powered off or you are logged out are lost, and launchd does not wake the Mac.

Status: this has not been observed through a real lid-close or reboot yet. The check is `cones ls --json` showing one `schedule` record fired after a slept-through tick and none after a reboot past one.

## The fleet: every Claude session on the Mac

```sh
cones hook --install             # one global Claude Code hook in ~/.claude/settings.json
cones ls --status blocked        # sessions waiting on a permission or elicitation prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

With the hook installed, every Claude Code session on the Mac appears in `cones ls` with its working directory, state, last update time, harness, dollars and tokens in/out; the dashboard adds the title, age and last message. Sessions that belong to a cones run collapse into that run's row, and a session whose process is gone is not shown. Dollars come from the ledger for cones runs; for hook-observed sessions the column stays `-`, since the hook records tokens and no price.

`cones hook --install` adds one command to `~/.claude/settings.json` for the `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Notification`, `Stop` and `SessionEnd` events, none of them `PreToolUse`, so Claude's permission checks are untouched. On each event Claude Code runs `cones --state-dir ~/.cones hook $PPID`, which writes `~/.cones/fleet/<session_id>.json`.

| Field in the state file | Source |
| --- | --- |
| `cwd`, `pid`, `transcript_path` | The hook payload; `$PPID` is the Claude process |
| `state`, `event`, `tool` | The event name and tool, mapped as below |
| `title` | Claude's `ai-title`, or a user-set `agent-name`, read from the transcript tail |
| `last` | First line of the assistant's most recent text |
| `tokens_in`, `tokens_out` | Summed from the transcript at `Stop` and `SessionEnd`; input includes cache reads and cache creation |

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

## The dashboard: jobs, sessions and runs on one screen

`cones tui` reloads every second and reads `N working · N need input · N idle · N jobs · N runs` on its summary line.

| Pane | Columns | Details pane |
| --- | --- | --- |
| Jobs | enabled marker, name, schedule, harness, on/off, last run status | schedule, policy line, prompt |
| Sessions | icon, harness, title or short id, state, age, tokens, last message or cwd | last prompt and full reply |
| Runs (newest 200) | icon, job, status, fired time, duration, dollars, reason | captured output and harness stderr |

Sessions group by directory like Claude's own agents view, or by state so the rows that need a human are on top.

| Key | Action |
| --- | --- |
| `↑` `↓`, `k` `j` | Move between rows. |
| `enter`, `→`, `a` | On a job: start a run in the background. On a running run: follow its log (Ctrl+C returns). On a finished run or a session: open it in this terminal, as described under the fleet; Ctrl+Z comes back. |
| `ctrl+x` twice (or `x` twice) within two seconds | Stop the selected run or session. |
| `ctrl+s` (or `s`) | Regroup sessions by state or by directory. |
| `n` | New task: type a prompt, `enter` dispatches it as `cones run --prompt` in the current directory, `esc` cancels. |
| `/` | Filter rows by text; `enter` keeps the filter, `esc` clears it. |
| `r` | Reload now. |
| `esc`, `q`, `ctrl+c` | Quit. |

Ctrl+Z never suspends the dashboard; inside an attached session it detaches and returns here. Runs the dashboard starts are ordinary `cones run` subprocesses and appear in the ledger and the fleet files.

## Doctor: what breaks a scheduled run before it starts

`cones doctor` prints one `OK`, `WARN` or `FAIL` line per check and exits 1 on any `FAIL`. Nothing it prints contains credentials or env values.

| Check | Level when wrong |
| --- | --- |
| Running on macOS (launchd requires a logged-in user) | FAIL |
| The first three launchd PATH entries are also on the shell PATH | WARN |
| `jobs.yaml` parses and each job's policy compiles | FAIL |
| Each `env` name is set in this shell | FAIL |
| The installed plist has every `env` name and the full launchd PATH | FAIL, or WARN when the job is not installed |
| A job permits broad Bash, archives plaintext transcripts alongside it, lists Edit/Write/Bash under `write: false`, or enables Codex full access | WARN |
| `claude` is on the launchd PATH and `claude --version` runs | FAIL |
| Claude version is inside the tested range `>=2.1, <3` | WARN |
| Every flag the compiler emits for a job that uses every option appears in `claude --help`; `--max-turns` is hidden there and probed by parsing an invalid value instead | FAIL (WARN for the probe) |
| `claude auth status --json` reports logged in; a scheduled job cannot prompt to log in | FAIL |
| The fleet hook is in `~/.claude/settings.json` | WARN |
| `~/.claude/projects`, Claude's session store, exists | WARN |
| `~/.cones/runs.jsonl` is readable and writable | FAIL |

## Codex: parsed, refused at validation

`harness: codex` is parsed: Codex jobs take no `tools` list, choose `write: false` (read-only) or `write: true` (workspace-write), and may set `codex_full_access`. Status: no Codex adapter exists in v0.1.0. `cones validate` and `cones install` stop with `codex execution is not available in v0.1; its dollar budget cannot yet be enforced`, `cones doctor` reports the job as FAIL, and `cones run` of such a job records a `failed` run with reason `validation: ...`. The blocker is a native dollar budget for Codex; see the roadmap.

## Command reference

Global flags: `--jobs PATH` (default `jobs.yaml`) and `--state-dir PATH` (default `~/.cones`).

| Command | What it does |
| --- | --- |
| `cones validate` | Compile every job's policy; print `name valid harness` per job or fail. |
| `cones install [--dry-run]` | Write and load LaunchAgents for enabled jobs, remove disabled ones; `--dry-run` prints the plists (with `env` values) and installs nothing. |
| `cones uninstall` | Remove every `local.cones.*` LaunchAgent; keep runs and transcripts. |
| `cones run JOB [--trigger manual\|schedule]` | Run a job now. launchd passes `--trigger schedule`. |
| `cones run --prompt "..." [JOB]` | One-off task in the current directory under the named job's policy (default: the first job), or under the read-only defaults when there is no jobs file. Named `adhoc-<8 hex>`. |
| `cones ls [--job NAME] [--status S] [--json]` | Runs newest first, then live sessions. Columns: id, job or cwd, status or state, fired or updated time, harness, dollars, reason or tokens in/out. `--status` takes `started`, `ok`, `failed`, `timeout`, `skipped`, `crashed`, `active`, `idle`, `blocked` or `exited`. `--job` hides sessions. `--json` prints one run record per line. |
| `cones logs ID [--follow] [--raw]` | A run's events rendered as tool calls and text, with the harness stderr tail appended; `--raw` prints the JSON events. For a session id, the last assistant lines of its transcript. Ctrl+C detaches, the run keeps going. |
| `cones stop ID` | A run ends `failed` / `interrupted` after cones checks the worker pid still belongs to its supervisor. A session ends through `claude stop` or SIGTERM as described under the fleet. Prints `stop requested` or `already finished`. |
| `cones attach ID [--print-command]` | A finished run or a session whose process is gone is resumed in the background and attached to, in its cwd; the archived transcript is restored into Claude's store if the native one is missing. A live session is attached directly. `--print-command` prints the command instead. A running headless run cannot be attached; follow its log. |
| `cones lock DIR -- COMMAND...` | Hold the directory writer lock while the command runs; exit with its status. |
| `cones doctor` | The checks above; `OK`/`WARN`/`FAIL` per line, exit 1 on any `FAIL`. |
| `cones tui` | The dashboard. |
| `cones hook [--install] [PID]` | `--install` writes the fleet hook into `~/.claude/settings.json`. Without it, `cones hook PID` records one hook event from stdin (Claude Code calls this). |

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

Tests use fake harness processes and spend no model tokens. Rules for agents working here are in [AGENTS.md](AGENTS.md).

Licensed under either Apache-2.0 or MIT, at your option.
