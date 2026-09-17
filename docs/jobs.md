# Scheduled jobs: the file and what a run does

Back to the [README](../README.md). The dashboard is in [dashboard.md](dashboard.md), commands in [cli.md](cli.md), what each flag asks of the harness in [harness.md](harness.md#trigger).

`jobs.yaml` contains `version: 3`, optional policy `defaults`, and `jobs`. Dashboard settings (`columns`, `whole_columns`, `activity`, `pane`, `start`, `confirm_secs`) are described in [dashboard.md](dashboard.md#columns). Unknown fields are rejected.

An older file is migrated on the first read: cones renames what the version it declares called a setting, deletes the lines of a setting a later version stopped having, bumps the version line and writes the file back, keeping comments and layout. Version 1 called the activity column and its block `sparkline`. Version 2 had `budget_usd`, `daily_budget_usd` and `max_turns`, which version 3 drops. A file cones cannot rewrite still loads, and the next save migrates it instead.

Jobs override policy defaults individually, and every field a run has takes a default. Claude uses `defaults.model`; Codex uses `defaults.codex_model` and `codex_full_access`, though Codex execution is currently unavailable. A job's own `model` is its override. The composer shares model and provider defaults but starts on `start.harness`; jobs inherit `defaults.harness`.

The dashboard's `config` button edits defaults and dashboard settings. The `jobs` screen adds jobs through `new job`, edits with `ctrl+e`, and deletes with `ctrl+x twice`; see [the wizard](dashboard.md#the-wizard).

```yaml
version: 3
defaults:
  timeout_min: 30
  write: false
columns: [harness, state, context, activity, model, age, last]   # dashboard session columns, see dashboard.md
whole_columns: true   # leave out a column the list's right edge would cut through
pane:                 # where the viewer pane sits and how much of the frame it takes
  at: right
  ratio: 50
start:                # what a new cones terminal comes up with, see dashboard.md
  harness: claude
  pane: true
activity:              # the activity column's window, metric and scale, see dashboard.md
  bars: 16
  bucket: 1m
  metric: lines
  bound: fleet
confirm_secs: 2       # seconds an armed ctrl+x waits for its second press, 0 until a key
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"          # five-field cron, compiled to launchd
    harness: claude
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    write: true                    # adds Edit, Write and sandboxed Bash to Read, Grep, Glob
    model: sonnet
    overlap: skip                  # skip | allow | replace
    catch_up: once                 # skip | once: one run at login for ticks missed while off
    notify: true                   # macOS notification when a run fails or times out
    # env: ["ANTHROPIC_API_KEY"]   # additional shell variables to import
```

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | 1-80 ASCII letters, digits, `-` or `_`; unique in the file. Becomes the launchd label `local.cones.<name>` and Claude's session `--name`. |
| `schedule` | required | Five-field local-time cron: minute, hour, day, month, weekday (0 or 7 is Sunday). Lists, ranges and steps work. At most 4096 launchd intervals. |
| `harness` | `defaults.harness`, else `claude` | `claude`. `codex` and `pi` parse and are refused at validation (see [Codex and pi](#codex-and-pi) below). |
| `cwd` | required | Working directory. `~/` expands, a relative path resolves against the jobs file's directory, and it must exist. |
| `prompt` | required | The task. Nonempty; passed after `--` on the command line. |
| `model` | `defaults.model` on a Claude job, `defaults.codex_model` on a Codex job, the harness's own otherwise; a pi job takes no default | Passed as `--model`. |
| `enabled` | `true` | `false` records each tick as `skipped` with reason `disabled`, and saving the job removes its LaunchAgent. |
| `archive_transcript` | `defaults.archive_transcript`, else `false` | Copy Claude's transcript into `~/.cones/transcripts/<run_id>/<session_id>.jsonl` when the run ends. |
| `env` | `defaults.env`, else `[]` | Names of shell variables to pass through. A job's own list replaces the default one; there is no per-name removal, and `env: []` on a job still takes `defaults.env`, so a job cannot opt out of an inherited list. Drop the name from `defaults.env` instead. Values are read at install or run time; installed schedules retain them in the plist. Standard variables and Bedrock credentials are handled separately below. |
| `timeout_min` | `30` | Runner timeout. Positive, at most 10080 (one week). |
| `write` | `false` | `false` allows Read, Grep and Glob. `true` adds Edit, Write and Bash, and turns on Claude's sandbox. There is no per-tool list: Claude treats a scoped `Bash(pattern)` rule as a pre-approval, not an exclusive allowlist, so cones cannot promise one. |
| `overlap` | `skip` | `skip`, `allow` or `replace`: what a tick does while the previous run is still going. |
| `catch_up` | `skip` | `skip` or `once`: what to do about ticks that passed while the Mac was off or logged out. `once` starts one run at the next login however many ticks were missed; `skip` leaves them lost. Three things bound the burst: one run per job whatever the number of missed ticks, a lookback that stops at 31 days, and `overlap`, since a catch-up is admitted like any other tick. A tick slept through needs neither value: launchd already fires it on wake. See [Schedules on launchd](#schedules-on-launchd-sleep-login-and-reboot). |
| `notify` | `false` | macOS notification (`osascript`) when a run is `failed` or `timeout`. `CONES_NOTIFIER` names a command that receives the title and message instead. |
| `codex_full_access` | `false` | Codex only. Rejected on a Claude job; as a default it reaches Codex jobs alone. |
| `bedrock` | none | `true` runs the job on Amazon Bedrock: Claude gets `CLAUDE_CODE_USE_BEDROCK=1` and every `AWS_` variable of the installing shell. A Codex job is refused, because the app-server daemon keeps the provider of the Codex configuration it started with and ignores what a thread asks for. `true` needs `aws_profile` and `aws_region` beside it and is rejected without them. `false` asks for the harness's own endpoint. Unset leaves it to the harness's own settings. `model` aliases such as `sonnet` resolve on either provider; a full model id is the provider's. |
| `aws_profile` | none | Required by `bedrock: true`, ignored without it. The profile, as named in `~/.aws/config`, that the run gets as `AWS_PROFILE`. It is set over an `AWS_PROFILE` the installing shell carried in, since it is the one the job was checked against. |
| `aws_region` | none | Required by `bedrock: true`, ignored without it. The region the run gets as `AWS_REGION`, as in `us-east-1`. A model id is answered only by the regions that carry it. |

## Validation

Saving in the dashboard compiles every job's policy and reports the first error with the job's name. Beyond the per-field rules it rejects:

- A schedule that restricts both day and weekday while one uses a wildcard step. launchd ORs the two fields where cron ANDs them.
- `bedrock: true` with no `aws_profile` or no `aws_region`, on the job or in `defaults`. These fields must be explicit in the file; shell values do not satisfy this check.
- `bedrock` on a Codex job, from the job or from `defaults`. cones cannot hold a Codex session to it, so it is a validation error rather than a setting that does nothing.
- An `env` name that could change execution policy, on a job or in `defaults`. The config editor refuses it on its own row, before the line reaches the file: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, or anything starting with `DYLD_`, `LD_` or `CLAUDE_CODE_`. Names must be valid shell identifiers. Bedrock is the `bedrock` field, not an `env` name.
- A `version` above `3`, since an older one is migrated on read, a duplicate name, a `cwd` that is not a directory, `codex_full_access` on a job that is not Codex's, a `claude` binary missing from the launchd PATH.

## What the harness is told

The job compiles to one `claude` command with a fixed argv, compiled again whenever the file is saved. The compiled argument list, its hash and the resolved policy are stored in the run's `started` record.

| Guarantee | Claude flags |
| --- | --- |
| Headless, streamed events | `--print --output-format stream-json --verbose` |
| No prompts | `--permission-mode dontAsk --permission-prompts none`. A denied tool call ends the run with reason `permission`. |
| Safe mode, restricted | `--safe-mode --restricted` |
| No user or project settings | `--setting-sources ""` |
| No MCP servers | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'` |
| No slash commands | `--disable-slash-commands` |
| Tool allowlist | `--tools` and `--allowedTools`, both `Read,Grep,Glob` or with `write` `Read,Grep,Glob,Edit,Write,Bash` |
| Pinned session ID | `--session-id <fresh uuid>`. cones generates it, so the ledger, the transcript, `attach` and the fleet agree; a mismatch ends the run with reason `session_mismatch`. |
| Job name | `--name <job>` |
| Sandbox when `write` is true | `--settings` with `sandbox.enabled` and `sandbox.failIfUnavailable` true, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands` false, `excludedCommands` empty |
| Model | `--model <m>` when set |
| The task | `-- <prompt>` as the final positional argument |

The timeout is the runner's: at `timeout_min` the whole process group gets SIGTERM, then SIGKILL after two seconds. The worker also ends itself one second past the timeout or when its supervisor disappears.

The harness starts with a cleared environment: `HOME`, `USER` and `TMPDIR` from the installing shell, a fixed `PATH` (`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`) from which `claude` is resolved, `LANG=en_US.UTF-8`, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, and the names listed in `env`; with `bedrock: true`, also `CLAUDE_CODE_USE_BEDROCK=1`, every `AWS_` variable the installing shell has for the credentials themselves, and `AWS_PROFILE` and `AWS_REGION` from the job's own `aws_profile` and `aws_region` over them. `cones __install --dry-run` prints the plist XML including those values, so it can contain secrets; it warns on stderr when a job imports any.

## What a run records

A run is one supervised harness process. `cones run` takes a global admission lock, reaps orphaned runs, applies skip rules, spawns a gated worker in its own process group, appends `started`, releases the worker, and reads the harness's JSON events until the process exits, the timeout passes, a permission is denied, or a stop arrives. On timeout, stop, replace or permission denial the whole process group is terminated: SIGTERM, two seconds, SIGKILL.

| Status | Reason | When | Exit |
| --- | --- | --- | --- |
| `started` | | Running. Dollars show as `-` until it ends. | |
| `ok` | | Claude exited 0 with a `success` result and a reported cost. | 0 |
| `skipped` | `disabled` | `enabled: false` | 0 |
| `skipped` | `overlap` | `overlap: skip` and a previous run is still going | 0 |
| `skipped` | `replace_unconfirmed` | `overlap: replace` and the previous run did not confirm shutdown within 10 seconds | 0 |
| `timeout` | `timeout` | The runner's clock ran out | 124 |
| `timeout` | `replaced` | This run was the old one under `overlap: replace`; it received SIGUSR1 and stopped | 124 |
| `failed` | `interrupted` | `ctrl+x` on the run's row, or SIGTERM / SIGINT to the runner | 1 |
| `failed` | `permission` | Claude reported a permission denial: a `permission_denials` entry on the result or a `permission_denied` system event. A sandboxed command the OS refuses is not one; the sandbox blocks it and the run goes on | 1 |
| `failed` | `session_mismatch` | An event carried a session id other than the pinned one | 1 |
| `failed` | `missing_result`, `missing_cost` | Claude exited without a result event, or with one that had no `total_cost_usd` | 1 |
| `failed` | `validation: ...`, `spawn: ...`, `runner: ...` | The policy did not compile, the worker could not start, or cones hit an error while supervising | 1 |
| `failed` | `exit`, or Claude's result subtype | Nonzero exit with no other explanation, or a result other than `success`, recorded verbatim | 1 |
| `failed` | `orphan` | The supervisor died; written when the next run of that job reaps it | |
| `crashed` | | Derived at read time: a `started` record with no terminal record past its timeout plus five seconds. | |

The `started` record has `trigger` (`manual` or `schedule`), `session_id`, `cwd`, `pid`, `pgid`, `timeout_s`, the compiled `policy` and its SHA-256 `policy_hash` (session id, job name and prompt normalized out, so a compiler flag change changes the hash). The terminal record has `duration_s`, `exit`, `tokens_in`, `tokens_out`, `cost_usd`, `reason` and, when archived, `transcript`.

State lives in `~/.cones` (`--state-dir` to isolate). Directories are created `0700` and files `0600`.

| Path under `~/.cones` | Contents |
| --- | --- |
| `runs.jsonl` | The ledger: one `started` and one terminal record per run. Appends hold an exclusive lock; a partial last line from a killed writer is repaired on the next append. |
| `hidden` | One run id per line, hidden with `ctrl+x` in the dashboard. The ledger is untouched; delete a line to show the row again. |
| `folders` | One path per line: folders the dashboard's menu picked, each a row of its own while nothing runs there. `ctrl+x` twice on the row removes its line. |
| `recent` | One path per line, newest first, at most 20: every folder the dashboard has seen a session in. The menu's `folder` prompt recalls them with `↑` `↓`. |
| `output/<run_id>/events.jsonl`, `output/<run_id>/stderr.log` | Claude's stream-json events (64 MiB cap, 1 MiB per line) and stderr (1 MiB cap). |
| `transcripts/<run_id>/<session_id>.jsonl` | The archived transcript when `archive_transcript: true`. |
| `logs/<job>.out.log`, `logs/<job>.err.log` | launchd's stdout and stderr for the scheduled `cones run`. |
| `locks/admission/`, `locks/runs/` | The admission lock `cones run` holds while it applies the skip rules, and one lease per run that stays held while the run is alive. |

## When a job is still running at its next tick

`overlap` is per job. Jobs that share a directory do not see each other: two writers on one directory both run, and keeping them out of each other's changes is the coordinator's business.

| `overlap` | Behavior |
| --- | --- |
| `skip` | The tick is recorded as `skipped` / `overlap`. |
| `allow` | Both run. |
| `replace` | The previous run gets SIGUSR1 and ends as `timeout` / `replaced`; the new run starts once that is confirmed within 10 seconds, else it is `skipped` / `replace_unconfirmed`. |

The planned `overlap: continue` would stop run 1 and start run 2 with `claude --resume` on run 1's session id. It is not an accepted configuration value yet.

## What a run costs

A run has no dollar or turn cap: cones records what the harness reports and stops a run on the clock alone. `timeout_min` is the only limit a run carries. Each terminal record keeps the run's `cost_usd` as Claude reported it, the dashboard shows it per run, and a run whose result carries no `total_cost_usd` is `failed` / `missing_cost`. Claude's `--max-budget-usd` and `--max-turns` are in [harness.md](harness.md#trigger) as flags cones does not pass.

## Schedules on launchd: sleep, login and reboot

Saving the jobs file writes one per-user LaunchAgent per enabled job and loads it with `launchctl bootstrap` in the `gui/<uid>` domain.

| plist key | Value |
| --- | --- |
| `Label` | `local.cones.<name>`, at `~/Library/LaunchAgents/local.cones.<name>.plist` |
| `StartCalendarInterval` | One entry per cron tick |
| `RunAtLoad` | `false` |
| `ProcessType` | `Background` |
| `WorkingDirectory` | The job's `cwd` |
| `ProgramArguments` | The installed `cones` binary with `--jobs <file> --state-dir <dir> run <name> --trigger schedule` |
| `StandardOutPath`, `StandardErrorPath` | `~/.cones/logs/<name>.out.log`, `.err.log` |
| `EnvironmentVariables` | The environment described under [What the harness is told](#what-the-harness-is-told), including the values of variables named in `env`, so a variable exported after install needs a reinstall |

It rewrites and re-bootstraps only plists whose content changed, bootstraps ones that are on disk and not loaded, and boots out the LaunchAgent of any job now disabled. Every ledger record and transcript is kept.

Per launchd.plist(5), ticks missed while the Mac sleeps coalesce into one launch on wake, so a wake starts at most one run per job and `overlap` decides if the previous run is still going; no job's own agent runs at login or when the agents are installed. Ticks that pass while the Mac is powered off or you are logged out are lost to launchd, and launchd does not wake the Mac.

`catch_up: once` on any enabled job adds one further LaunchAgent, the only one that is not a job. `catchup` is a reserved job name.

| plist key | Value |
| --- | --- |
| `Label` | `local.cones.catchup`, at `~/Library/LaunchAgents/local.cones.catchup.plist` |
| `RunAtLoad` | `true`, and there is no `StartCalendarInterval`: login is its only trigger |
| `ProcessType` | `Background` |
| `WorkingDirectory` | The state directory |
| `ProgramArguments` | The installed `cones` binary with `--jobs <file> --state-dir <dir> catchup` |
| `StandardOutPath`, `StandardErrorPath` | `~/.cones/logs/catchup.out.log`, `.err.log` |
| `EnvironmentVariables` | None. Each run comes from the job's own agent, so it carries the environment `install` captured for that job |

For every job that asked to catch up, `cones catchup` takes the newest `schedule` record's fired time as the mark and walks that job's cron forward from it in local time. The first tick that should already have fired makes it ask launchd to start the job with `launchctl kickstart`, which is why the run's environment and its `schedule` trigger are the same as any tick's; the catch-up decision itself is only in `catchup.out.log`. One run per job however many ticks passed, and admission still applies, so a catch-up can be recorded `skipped` like any other tick. A job with no `schedule` record behind it has no mark and is left alone, so a first install never fires one. Lookback stops at 31 days. `cones catchup --dry-run` names what it would start without starting it, and saving the jobs file drops the agent again once no enabled job asks for it.

Tests check the plist configuration, the missed-tick walk against weekday and month constraints, and what `--dry-run` names; physical sleep/wake, reboot and login behavior has not been verified. A live check should show one `schedule` record after a slept-through tick, none for a tick missed while powered off with `catch_up: skip`, and one after the next login with `catch_up: once`.

## Codex and pi

`harness: codex` and `harness: pi` parse, but no execution adapters are available, so validation and installation fail. Running such a job records `failed` with a `validation: ...` reason. `codex_full_access` is Codex-only.

Native Codex and pi sessions still appear in the fleet. Codex daemon threads can be joined through the dashboard; a pi can be joined only when the dashboard's own composer started it, and stays in its own terminal otherwise. See [harness.md](harness.md#kinds).
