# Configuration and runs

[README](../README.md) · [Dashboard controls](dashboard.md) · [Commands](cli.md) · [Harness support](harness.md)

`jobs.yaml` configures the dashboard and supervised jobs. This guide follows the file's fields, then how schedules fire and what a run records. The dashboard's [config editor](dashboard.md#config) and [job wizard](dashboard.md#jobs) edit the same file.

## File structure

```yaml
version: 3
defaults:
  timeout_min: 15
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    write: true
    model: sonnet
```

| Top-level field | Contents |
| --- | --- |
| `version` | Schema version, currently `3`. |
| `defaults` | Optional [policy defaults](#job-fields-and-defaults). |
| `jobs` | Job list; `[]` is valid for a dashboard with no scheduled work. |
| `columns`, `run_columns`, `whole_columns`, `confirm_secs` | [List settings](#list-settings). |
| `pane` | [Viewer position and size](#pane). |
| `start` | [Initial dashboard settings](#start). |
| `activity` | [Activity chart settings](#activity). |

Unknown fields and versions above `3` are rejected. Older files migrate on read, preserving comments and layout: version 1's `sparkline` column and block become `activity`; version 2's `budget_usd`, `daily_budget_usd` and `max_turns` fields are removed. If the file cannot be rewritten, it still loads and the next save migrates it.

## Job fields and defaults

A job's policy fields override `defaults` individually. The Default column below gives the built-in value used when neither sets one. `name`, `schedule`, `cwd`, `prompt` and `enabled` belong only to jobs. `defaults` also accepts per-harness composer settings: `codex_model`, `pi_model` and `pi_provider`. Claude uses `defaults.model`. A matching model default also supplies a job's omitted `model`; pi and Codex jobs remain unavailable. Unset model and provider values follow the harness's own configuration. The composer's initial harness is the separate [`start.harness`](#start).

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | Unique, 1-80 ASCII letters, digits, `-` or `_`. `catchup` is reserved. Used for the LaunchAgent label and Claude's session name. |
| `schedule` | required | Five-field local-time cron; see [schedules](#schedules). |
| `cwd` | required | Existing working directory. `~/` expands; relative paths resolve against the jobs file's directory. |
| `prompt` | required | Nonempty task, passed as the final command-line argument after `--`. |
| `enabled` | `true` | Saving a disabled job removes its LaunchAgent; attempts to run it record `skipped` / `disabled`. |
| `harness` | `claude` | `claude`, `codex` or `pi`. Only Claude currently has a supervised execution adapter; [native support](harness.md#supervised-execution) determines which jobs validate. |
| `model` | harness's own | Overrides the per-harness default. Must be nonempty and contain no NUL. |
| `timeout_min` | `30` | Positive number of minutes, at most `10080` (one week). This is the only run limit; there is no dollar or turn cap. |
| `write` | `false` | `false` permits Read, Grep and Glob. `true` adds Edit, Write and sandboxed Bash. No per-tool rules: Claude treats a scoped `Bash(pattern)` as pre-approval, so cones cannot enforce it as an exclusive allowlist. |
| `overlap` | `skip` | `skip`, `allow` or `replace`; see [overlap](#overlap). |
| `catch_up` | `skip` | `skip` or `once`; see [sleep-and-login behavior](#sleep-login-and-reboot). |
| `notify` | `false` | macOS notification through `osascript` when a run fails or times out. `CONES_NOTIFIER` can name a command receiving the title and message instead. |
| `archive_transcript` | `false` | Copy the native transcript into the [run's state directory](#stored-files) when it ends. |
| `env` | `[]` | Shell variable names to import. A job's nonempty list replaces the default list; an empty list inherits it, so opting out requires removing the name from `defaults.env`. See [environment](#environment). |
| `codex_full_access` | `false` | Codex option; defaults reach only Codex jobs. `true` is rejected on Claude. |
| `bedrock` | unset | `true` selects Amazon Bedrock for Claude; `false` selects the native endpoint; unset follows the harness configuration. Rejected on Codex jobs because the daemon keeps its configured provider and ignores a thread's override. Native pi composer sessions ignore this field. |
| `aws_profile` | unset | Required with `bedrock: true`, ignored otherwise. Sets `AWS_PROFILE` over any imported shell value. |
| `aws_region` | unset | Required with `bedrock: true`, ignored otherwise. Sets `AWS_REGION`; the chosen region must serve the model. |

Bedrock profile and region must be explicit in the file, on the job or in `defaults`; shell values do not satisfy validation. Claude model aliases resolve through the selected provider, while a full model id must belong to that provider. `opus[1m]` and `sonnet[1m]` explicitly request the million-token window; the bare aliases do not.

## List settings

| Field | Default | Meaning |
| --- | --- | --- |
| `columns` | `[harness, state, context, activity, model, age, last]` | Visible session columns. `harness` and `state` always sit before the title in that order; other columns follow it in list order. Unknown names are rejected. |
| `run_columns` | `[harness, status, started, took, context, model, cost, reason]` | Visible run columns, independent of session columns. The harness icon and job always show; `harness` adds its name and `status` sits before the job. Other columns follow in list order. `[]` hides all optional columns. Unknown names are rejected. |
| `whole_columns` | `true` | Omit a column that would cross the list's right edge. `false` draws its visible portion. The mark, harness, state and title are drawn either way so a narrow row still identifies itself. |
| `confirm_secs` | `2` | Seconds an armed removal waits for confirmation; `0` waits until another key. Valid range: `0` to `600`. |

| Column | Value |
| --- | --- |
| `harness` | Name after the harness mark, such as `✻ claude`. Without the column only the mark appears. |
| `state` | working, input, idle, done, failed or stopped. |
| `context` | Prompt/window tokens, such as `98k/200k`; prompt alone when the window is unavailable. |
| `activity` | Counts over time under the [activity settings](#activity). |
| `model` | The reported model's display name. |
| `age` | Time since session start; never resets between turns. |
| `last` | Latest reply or status text; directory instead when grouped by state. |
| `tokens` | Session input/output totals, such as `49.2M/201k`. |

Missing values show `-`. [Harness reports](harness.md#reports) define each source and model naming.

Run columns:

| Column | Value |
| --- | --- |
| `harness` | Name after the always visible harness icon. |
| `status` | Recorded run outcome, or current supervision status. |
| `started`, `ended` | Start and end time in your local timezone, including the date. |
| `took` | Duration in seconds, or elapsed time while running. |
| `context`, `model` | Latest reported prompt/window tokens and model, using the same formatting as sessions. |
| `tokens` | Input/output totals from the terminal record, or reported usage while running. |
| `cost` | Cost reported by the harness. |
| `reason` | Failure, timeout or skip reason. |
| `dir` | Run's working directory. |
| `trigger` | `manual` or `schedule`. |
| `last` | Latest recorded reply. |

Claude run details come from saved run output, falling back to an archived transcript when the output is unavailable. Context windows come from the saved status line when available. Missing reports stay `-`; current job settings do not supply historical model or context values. Ledger timestamps remain UTC; displayed start and end times use the machine's local timezone, including daylight saving changes.

## Pane

| Field | Default | Meaning |
| --- | --- | --- |
| `pane.at` | `right` | `right` puts the list left of the viewer; `bottom` puts it above the viewer. Both have a divider. |
| `pane.ratio` | `50` | Viewer percentage of the frame, from `30` to `70`. The list takes the rest, less the divider. There is no minimum terminal size for a split. |

## Start

| Field | Default | Meaning |
| --- | --- | --- |
| `start.harness` | `claude` | Initially selected composer harness: `claude`, `codex` or `pi`. |
| `start.pane` | `true` | Open the viewer pane at dashboard startup. |

These values apply at startup. Runtime harness and pane keys change the current dashboard; the config editor saves settings for future dashboards.

## Activity

| Field | Default | Meaning |
| --- | --- | --- |
| `activity.bars` | `16` | Number of buckets, from `1` to `64`, oldest left and newest right. |
| `activity.bucket` | `1m` | Positive duration in `s`, `m` or `h`, at most `24h`. The chart covers bars × bucket. |
| `activity.metric` | `lines` | `lines`, `messages`, `tools` or `tokens` (output tokens). [Harness mappings](harness.md#activity) define the events counted. |
| `activity.bound` | `fleet` | `fleet` scales to the busiest loaded bucket; `row` to each row's busiest bucket; `log` uses logarithmic fleet scaling. A positive number fixes the count for a full bar and is set in the file. |

Buckets align to the clock. Bars move left once per bucket; only the newest grows between boundaries, and future timestamps count there. Empty buckets use the lowest bar, and rows with no activity in the window are dim. The chart counts reports independently of session state, so low bars beside `working` mean little has been written recently.

## Schedules

Cron fields are minute, hour, day, month and weekday; `0` and `7` both mean Sunday. Lists, ranges and steps work, up to 4096 compiled launchd intervals. A schedule restricting both day and weekday while one uses a wildcard step is rejected: launchd ORs those fields where cron ANDs them.

Saving or deleting a job in the dashboard installs schedules for the file. Each enabled job gets a per-user LaunchAgent loaded with `launchctl bootstrap` in `gui/<uid>`. Only changed plists are rewritten and re-bootstrapped; agents present on disk but unloaded are bootstrapped, and disabled or removed jobs are booted out. Existing run records and transcripts remain.

| plist key | Job agent | Catch-up agent, when needed |
| --- | --- | --- |
| `Label` | `local.cones.<name>` | `local.cones.catchup` |
| plist path | `~/Library/LaunchAgents/local.cones.<name>.plist` | `~/Library/LaunchAgents/local.cones.catchup.plist` |
| `StartCalendarInterval` | One entry per compiled cron tick | absent |
| `RunAtLoad` | `false` | `true` |
| `ProcessType` | `Background` | `Background` |
| `WorkingDirectory` | Job's `cwd` | State directory |
| `ProgramArguments` | Installed binary with `--jobs <file> --state-dir <dir> run <name> --trigger schedule` | Installed binary with `--jobs <file> --state-dir <dir> catchup` |
| `StandardOutPath`, `StandardErrorPath` | `~/.cones/logs/<name>.out.log`, `.err.log` | `~/.cones/logs/catchup.out.log`, `.err.log` |
| `EnvironmentVariables` | [Environment captured at install](#environment) | None; each catch-up starts the job's own agent |

### Sleep, login and reboot

launchd coalesces ticks missed during sleep into one launch on wake; it does not wake the Mac. Ticks missed while powered off or logged out are lost. Job agents do not run merely because they were installed or the user logged in. These are launchd's documented semantics; physical sleep, reboot and login have not been verified by this project's checks.

An enabled job with `catch_up: once` adds the shared login agent in the table above. For each such job, `cones catchup` walks its cron forward from the latest ledger record with trigger `schedule`, including a skipped run, looking back at most 31 days. No prior scheduled record means no catch-up, so first install cannot fire a job. A missed tick requests `launchctl kickstart` of the job's agent, retaining that agent's environment and schedule trigger. However many ticks passed, it requests only one run per job, subject to overlap admission. The catch-up decision itself appears only in `catchup.out.log`. Saving the file removes the login agent when no enabled job requests it.

### Overlap

Overlap is per job. Two jobs sharing a directory may both run; shared-file coordination belongs to the [coordinator skill](cli.md#coordinator-launch).

| `overlap` | Behavior while a previous run is alive |
| --- | --- |
| `skip` | Record the new tick as `skipped` / `overlap`. |
| `allow` | Start another run. |
| `replace` | Send the previous supervisor SIGUSR1. It ends as `timeout` / `replaced`; start the new run only after shutdown is confirmed within 10 seconds, otherwise record `skipped` / `replace_unconfirmed`. |

## Run lifecycle

A run is one supervised harness process. `cones run` takes a global admission lock, reaps orphaned runs and applies skip rules. It then creates a gated worker in its own process group, appends `started`, releases the worker, and reads events until completion or termination. Policy compilation errors are recorded as failed runs; dashboard saves compile every job and report the first error with its name.

On timeout, stop, replacement or permission denial, the whole worker process group receives SIGTERM, then SIGKILL after two seconds. The worker also ends itself one second beyond the configured timeout or when its supervisor disappears. A sandboxed command rejected by the OS is not a reported permission denial; the sandbox blocks it and the run continues.

### What the harness is told

The compiled arguments, resolved policy and its hash are recorded at start. The launch PATH must contain a compatible `claude` binary.

| Guarantee | Claude arguments |
| --- | --- |
| Headless, streamed events | `--print --output-format stream-json --verbose` |
| No permission prompts | `--permission-mode dontAsk --permission-prompts none` |
| Safe mode, restricted | `--safe-mode --restricted` |
| No user or project settings | `--setting-sources ""` |
| No MCP servers | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'` |
| No slash commands | `--disable-slash-commands` |
| Allowed tools | `--tools` and `--allowedTools`, both set from `write` |
| Pinned session identity | `--session-id <fresh uuid>` |
| Job name | `--name <job>` |
| Sandbox for write jobs | `--settings` sets `sandbox.enabled` and `sandbox.failIfUnavailable` true, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands` false, and `excludedCommands` empty |
| Model override | `--model <m>` when set |
| Task | `-- <prompt>` |

### Environment

Each run starts with a cleared environment. Installed schedules capture values in the plist, so exporting a variable later does not update them. Changing environment defaults in `config` requires a subsequent job save to reinstall schedules. [The internal install command](cli.md#internal-commands) can print the compiled plist.

| Source | Variables passed |
| --- | --- |
| Installing or running shell | `HOME`, `USER`, `TMPDIR`, and names in `env` |
| Fixed values | `LANG=en_US.UTF-8`, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` |
| Launch PATH | `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`; also used to resolve `claude` |
| `bedrock: true` | `CLAUDE_CODE_USE_BEDROCK=1`, all shell `AWS_` variables for credentials, then the configured `AWS_PROFILE` and `AWS_REGION` over them |

`env` entries must be shell identifiers and cannot override execution policy: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, and prefixes `DYLD_`, `LD_` and `CLAUDE_CODE_` are rejected. Use the Bedrock fields for provider selection. An SSO profile still needs an authenticated session, such as one opened by `aws sso login --profile <name>`.

### Results

| Status | Reason | When | Exit |
| --- | --- | --- | --- |
| `started` | | Running; cost is not yet available. | |
| `ok` | | Exit 0 with a `success` result and reported cost. | 0 |
| `skipped` | `disabled`, `overlap`, `replace_unconfirmed` | Admission refused the run for the reason above. | 0 |
| `timeout` | `timeout` | The clock ran out. | 124 |
| `timeout` | `replaced` | A later run replaced this one. | 124 |
| `failed` | `interrupted` | Dashboard stop, or SIGTERM/SIGINT to the supervisor. | 1 |
| `failed` | `permission` | A result has `permission_denials`, or a system event has subtype `permission_denied`. | 1 |
| `failed` | `session_mismatch` | An event's session id differs from the pinned id. | 1 |
| `failed` | `missing_result`, `missing_cost` | No result, or no `total_cost_usd` on it. | 1 |
| `failed` | `validation: ...`, `spawn: ...`, `runner: ...` | Compilation, worker launch or supervision failed. | 1 |
| `failed` | `exit`, or native result subtype | Nonzero exit without another explanation, or a result other than `success`, recorded verbatim. | 1 |
| `failed` | `orphan` | A later run of the job reaped a dead supervisor. | |
| `crashed` | | Derived on read: no terminal record more than five seconds beyond the run's timeout. | |

The start record contains `trigger` (`manual` or `schedule`), `session_id`, `cwd`, `pid`, `pgid`, `timeout_s`, `policy` and its SHA-256 `policy_hash`. Session id, job name and prompt are normalized out of the hash, so compiler flag changes can be compared across runs. The terminal record contains `duration_s`, `exit`, `tokens_in`, `tokens_out`, `cost_usd`, `reason` and, when archived, `transcript`. Cost is the harness's reported value, with no price calculation by cones.

### Stored files

State defaults to `~/.cones`; [`--state-dir`](cli.md) relocates it. Directories are created `0700` and files `0600`.

| Path under the state directory | Contents |
| --- | --- |
| `runs.jsonl` | One start and one terminal record per admitted run. Appends hold an exclusive lock; the next append repairs a partial trailing line left by a killed writer. |
| `hidden` | Hidden run ids, one per line. Remove a line to restore the row without changing the ledger. |
| `output/<run_id>/events.jsonl` | Streamed events, capped at 64 MiB total and 1 MiB per line. |
| `output/<run_id>/stderr.log` | Captured stderr, capped at 1 MiB. |
| `transcripts/<run_id>/<session_id>.jsonl` | Archived transcript, when requested. |
| `logs/<job>.out.log`, `logs/<job>.err.log` | launchd's stdout and stderr. |
| `locks/admission/`, `locks/runs/` | Admission lock and one lease held for each live run. |
