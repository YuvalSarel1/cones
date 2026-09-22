# Configuration and runs

[README](../README.md) · [Dashboard controls](dashboard.md) · [Commands](cli.md) · [Harness support](harness.md)

`jobs.yaml` configures the dashboard and supervised jobs. This guide follows the file's fields, then how schedules fire and what a run records. The dashboard's [config editor](dashboard.md#config) and [job wizard](dashboard.md#jobs) edit the same file.

## File structure

```yaml
version: 4
defaults:
  timeout_min: 15
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    model: sonnet
```

| Top-level field | Contents |
| --- | --- |
| `version` | Schema version, currently `4`. |
| `defaults` | Optional [policy defaults](#job-fields-and-defaults). |
| `jobs` | Job list; `[]` is valid for a dashboard with no scheduled work. |
| `columns`, `run_columns`, `job_columns`, `history_columns`, `whole_columns`, `highlight`, `confirm_secs` | [List settings](#list-settings). |
| `folders` | [Pinned folders](dashboard.md#add-a-folder). |
| `pane` | [Viewer position and size](#pane). |
| `start` | [Initial dashboard settings](#start). |
| `activity` | [Activity chart settings](#activity). |

Unknown fields and versions above `4` are rejected. Older files migrate on read, preserving unrelated comments and layout: version 1's `sparkline` column and block become `activity`; version 2's `budget_usd`, `daily_budget_usd` and `max_turns` fields are removed; version 3's `write` field is removed. If the file cannot be rewritten, it still loads and the next save migrates it.

## Job fields and defaults

A job's policy fields override `defaults` individually. The Default column below gives the built-in value used when neither sets one. `name`, `schedule`, `cwd`, `prompt` and `enabled` belong only to jobs. Composer settings belong in `defaults`; their exact keys are listed under [composer harnesses](#composer-harnesses). Unset values follow the harness's own configuration. The initial selection is the separate [`start.harness`](#start).

Claude is the only harness cones can supervise, so it is the only one a job may name. A job on any other harness, whether the job says so or `defaults.harness` does, is refused when the file is read, and the message names the job. The refusal is deliberate: a schedule cones cannot carry is worse than no schedule. Every other harness remains yours to start from the composer.

| Field | Default | Meaning |
| --- | --- | --- |
| `name` | required | Unique, 1-80 ASCII letters, digits, `-` or `_`. `catchup` is reserved. Used for the LaunchAgent label and Claude's session name. |
| `schedule` | required | Five-field local-time cron; see [schedules](#schedules). |
| `cwd` | required | Existing working directory. `~/` expands; relative paths resolve against the jobs file's directory. |
| `prompt` | required | Nonempty task, passed as the final command-line argument after `--`. |
| `enabled` | `true` | Saving a disabled job removes its LaunchAgent; attempts to run it record `skipped` / `disabled`. |
| `harness` | `claude` | Must be `claude`. Other [harness keys](#composer-harnesses) are valid in the composer settings and refused on a job. |
| `model` | harness's own | Overrides the per-harness default. Must be nonempty and contain no NUL. |
| `timeout_min` | `30` | Positive number of minutes, at most `10080` (one week). This is the only run limit; there is no dollar or turn cap. |
| `overlap` | `skip` | `skip`, `allow` or `replace`; see [overlap](#overlap). |
| `catch_up` | `skip` | `skip` or `once`; see [sleep-and-login behavior](#sleep-login-and-reboot). |
| `notify` | `false` | macOS notification through `osascript` when a run fails or times out. `CONES_NOTIFIER` can name a command receiving the title and message instead. |
| `archive_transcript` | `false` | Copy the native transcript into the [run's state directory](#stored-files) when it ends. |
| `env` | `[]` | Shell variable names to import. A job's nonempty list replaces the default list; an empty list inherits it, so opting out requires removing the name from `defaults.env`. See [environment](#environment). |
| `bedrock` | unset | `true` selects Amazon Bedrock; `false` selects the native endpoint; unset follows the harness configuration. Claude's definition is the only one that [names a Bedrock switch](harness-definitions.md#commands); the Codex daemon keeps the provider it started with, and pi and OpenCode select Bedrock as a provider through `pi_provider` and `opencode_model`. |
| `aws_profile` | unset | Sets `AWS_PROFILE` over any imported shell value. Required with `bedrock: true`. |
| `aws_region` | unset | Sets `AWS_REGION`; the chosen region must serve the model. Required with `bedrock: true`. |

`codex_full_access: true` is rejected on a Claude job; `false` has no effect. The setting is still read from `defaults`, where nothing consumes it yet: no run and no composer launch passes it on today.

### Composer harnesses

These are `defaults` fields. Harness keys also set `start.harness`; their order here is the composer's cycle order, followed by `terminal`.

| Harness key | Enabled switch | In picker | Model default |
| --- | --- | --- | --- |
| `claude` | `claude_enabled` | `claude_in_picker` | `model` |
| `codex` | `codex_enabled` | `codex_in_picker` | `codex_model` |
| `pi` | `pi_enabled` | `pi_in_picker` | `pi_model` |
| `opencode` | `opencode_enabled` | `opencode_in_picker` | `opencode_model` |
| `gemini` | `gemini_enabled` | `gemini_in_picker` | `gemini_model` |
| `cursor-agent` | `cursor_enabled` | `cursor_in_picker` | `cursor_model` |
| `copilot` | `copilot_enabled` | `copilot_in_picker` | `copilot_model` |
| `amp` | `amp_enabled` | `amp_in_picker` | No override; native configuration |
| `droid` | `droid_enabled` | `droid_in_picker` | No override; native configuration |
| `kimi` | `kimi_enabled` | `kimi_in_picker` | `kimi_model` |

The last six are [experimental terminal launchers](harness.md#additional-terminal-harnesses). They have no native history, activity or usage reports and cannot run supervised jobs. Their CLIs must be installed and authenticated separately; [executable lookup](cli.md#native-cli-lookup) explains where cones finds them.

An unset enabled switch means offered. `false` removes the harness from the composer, startup selection and discovery. An unset `_in_picker` key means offered too; `false` takes the harness out of the composer cycle and startup selection alone, so its sessions stay listed, discovery keeps reading its native home and a job that names it still runs it. The enabled switch wins: a harness that is off is out of the picker whatever its picker key says, and with every launcher hidden the composer comes up on the terminal. An already open viewer keeps running. These switches do not change a job's enabled state or grant it execution support. Config's harnesses group exposes the same switches, the Bedrock and AWS settings, and a connectivity check for each installed CLI's required launch flags. The model, effort and provider keys are not there: the dashboard's [`ctrl+o` picker](dashboard.md#launch-settings) sets them beside the composer, for the harness it names. A picker choice is written to `defaults` as it is made, so a job's next run uses it.

Pi also accepts `pi_provider`, and Codex accepts `codex_full_access`. OpenCode's `opencode_model` uses `provider/model`, as listed by `opencode models`. Reasoning effort has two keys: `effort` for Claude and `pi_thinking` for pi. Other harnesses have no effort override in cones.

Two of these keys are also run policy, since a job has no field of its own for either: `defaults.model` supplies a Claude job's omitted `model`, and `defaults.effort` is the reasoning effort of every Claude run. Changing what the composer starts with therefore changes what tonight's job does.

Bedrock profile and region must be explicit in the file, on the job or in `defaults`; shell values do not satisfy validation. Claude model aliases resolve through the selected provider, while a full model id must belong to that provider. `opus[1m]` and `sonnet[1m]` explicitly request the million-token window; the bare aliases do not.

## List settings

Each category has an independent column picker. Visible columns can be reordered; `[]` hides every optional column. The title or job name and row icons always remain. `harness` controls the name beside its permanent icon and is hidden by default. An explicit list preserves your choices when built-in defaults change.

| Field | Default |
| --- | --- |
| `columns` | `[state, context, activity, model, age, last_active, folder, last_reply]` |
| `run_columns` | `[status, started, duration, model, cost, folder, reason]` |
| `job_columns` | `[status, schedule, next_run, model, last_run, folder]` |
| `history_columns` | `[last_active, folder, model, context, last_reply]` |
| `whole_columns` | `true`: omit a column crossing the list's right edge; `false` draws its visible portion. Icons, state/status and the title/job are retained either way. |
| `highlight` | `magenta`: colour of a session title highlighted with `ctrl+p`. One of `magenta`, `cyan`, `blue`, `green`, `yellow`, `red`. |
| `confirm_secs` | `2`: seconds an armed removal waits for confirmation; `0` waits until another key. Valid range: `0` to `600`. |
| `folders` | `[]`: folders pinned in the session list, each absolute or under `~`. The config screen holds them one folder per row behind the folders setting, where `enter` edits the selected folder or adds one and `ctrl+x` removes it; `+ add folder` and `ctrl+x` on a pinned row in the session list write the same setting. A path carrying a comma, a quote or a bracket is refused. |

The agent `folder` column appears when grouped by state. Normal folder groups identify it in their headings. Agent defaults hide `last_reply` while the preview pane is open; explicitly selecting it in `columns` keeps it visible. Grouping never changes last reply into a folder. History always uses its own selection, independently of grouping and pane visibility.

| Column | Categories | Value |
| --- | --- | --- |
| `harness` | All | Harness name after the permanent icon. |
| `state` | Agents | Working, input, idle, done, failed or stopped. |
| `status` | Jobs, runs | Last run outcome for a job, or the run's supervision status. Disabled jobs show `off`. |
| `folder` | All | Reported working directory. |
| `branch` | Agents | Current Git branch of the displayed folder, or `@<commit>` for a detached checkout. Read once per distinct folder during background refresh, only when selected. |
| `model` | All | Reported model; jobs show their configured model. |
| `effort` | Agents | Reasoning effort as the harness reports it. Claude reports it through the [saved statusLine payload](harness.md#claude-code), Codex on each turn. A harness that reports none shows `-`. |
| `cpu` | Agents | Percent of one core the process running the session is using, as the kernel reports it. |
| `memory` | Agents | Resident memory of the process running the session. |
| `context` | Agents, runs, history | Latest reported prompt/window tokens; prompt alone if no window was reported. |
| `tokens` | Agents, runs, history | Input/output totals. Run terminal records take precedence over live usage. |
| `cost` | Agents, runs, history | Native dollars, or `~$…` for a catalog estimate from reported provider, model and usage. A subtotal with gaps shows only what it priced; unavailable totals show `-`. Finished runs keep their terminal-record cost. See [cost sources](harness.md#cost-estimates). |
| `activity` | Agents | Counts over time under the [activity settings](#activity). |
| `age` | Agents, history | Time since session start. |
| `last_active` | Agents, history | Time since the latest recorded activity. |
| `last_reply` | Agents, runs, history | Latest recorded reply or agent status text. |
| `started`, `ended` | Runs | Start and end time in the local timezone, including the date. |
| `duration` | Runs | Recorded duration, or elapsed seconds while running. |
| `reason` | Runs | Failure, timeout or skip reason. |
| `trigger` | Runs | `manual` or `schedule`. |
| `schedule` | Jobs | Configured local cron rule, separate from status. |
| `next_run` | Jobs | Next time matching the enabled job's configured calendar intervals. This is the configured schedule, not confirmation that its LaunchAgent is loaded. Disabled jobs show `-`. |
| `last_run` | Jobs | Time since the latest run started. |

Missing values show `-`. `cpu` and `memory` cover the agent's own process, not the commands it spawns, and a session with no process of its own shows `-`. A Codex thread runs inside the daemon that holds it, so its row reports that daemon's process and every thread of one daemon reports the same number; a thread the daemon has released shows `-`. History offers no live state, activity chart or process columns. Unknown column names are rejected; duplicate names are ignored. Older `last`, `dir` and `took` names remain accepted as aliases for `last_reply`, `folder` and `duration`; saves write the explicit names.

Claude run details come from saved output, falling back to an archived transcript. Context windows and live costs come from the saved status line when available. Current job settings do not supply historical model or context values. Ledger timestamps remain UTC; displayed times use the machine's local timezone, including daylight saving changes. [Harness reports](harness.md#reports) describe the native sources.

## Pane

| Field | Default | Meaning |
| --- | --- | --- |
| `pane.at` | `right` | `right` puts the list left of the viewer; `bottom` puts it above the viewer. Both have a divider. |
| `pane.ratio` | `50` | Viewer percentage of the frame, from `30` to `70`. The list takes the rest, less the divider. There is no minimum terminal size for a split. |

## Start

| Field | Default | Meaning |
| --- | --- | --- |
| `start.harness` | `claude` | Initially selected composer harness, using a key from [composer harnesses](#composer-harnesses). |
| `start.pane` | `true` | Open the viewer pane at dashboard startup. |
| `start.notify` | `false` | Desktop notifications for newly reported input requests and completions outside the focused session. Independent of a job's `notify`. |

Harness and pane values apply at startup. Runtime harness and pane keys change the current dashboard; the config editor saves their initial settings for future dashboards. Notification changes take effect on the next configuration refresh.

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

An enabled job with `catch_up: once` adds the shared login agent in the table above. For each such job, `cones catchup` walks its cron forward from the latest ledger record with trigger `schedule`, including a skipped run, looking back at most 31 days. No prior scheduled record means no catch-up, so first install cannot fire a job.

A missed tick requests `launchctl kickstart` of the job's agent, retaining that agent's environment and schedule trigger. However many ticks passed, it requests only one run per job, subject to overlap admission. The catch-up decision itself appears only in `catchup.out.log`. Saving the file removes the login agent when no enabled job requests it.

The walk also starts no earlier than the job's own LaunchAgent was written. A disabled job has no agent, so re-enabling one that last ran a fortnight ago catches up nothing: that window was paused, not missed. Editing a schedule rewrites the plist for the same reason, and ticks only the previous cron would have fired are dropped with it. A job left enabled and unchanged keeps its plist untouched, so its window is still the last tick launchd delivered.

### Overlap

Overlap is per job. Two jobs sharing a directory may both run; shared-file coordination belongs to the [coordinator skill](cli.md#coordinator).

| `overlap` | Behavior while a previous run is alive |
| --- | --- |
| `skip` | Record the new tick as `skipped` / `overlap`. |
| `allow` | Start another run. |
| `replace` | Send the previous supervisor SIGUSR1. It ends as `timeout` / `replaced`; start the new run only after shutdown is confirmed within 10 seconds, otherwise record `skipped` / `replace_unconfirmed`. |

## Run lifecycle

A run is one supervised harness process. `cones run` takes an admission lock for its job, reaps orphaned runs and applies skip rules. Replacement can wait for that job's old run without blocking admission for other jobs. It then creates a gated worker in its own process group, appends `started`, releases the worker, and reads events until completion or termination. Policy compilation errors are recorded as failed runs; dashboard saves compile every job and report the first error with its name.

On timeout, stop, replacement or permission denial, the whole worker process group receives SIGTERM, then SIGKILL after two seconds. The worker also ends itself one second beyond the configured timeout or when its supervisor disappears.

### What the harness is told

The compiled arguments, resolved policy and its hash are recorded at start. The launch PATH must contain a compatible `claude` binary.

A job is a scheduled launch of the agent the owner runs by hand. It reads their settings,
loads their MCP servers, answers no prompts and keeps every tool. cones adds no allowlist,
no sandbox and no second permission engine; `timeout_min` is the only limit it puts on a run.
A job can do anything the owner can do in that directory, so give a job a directory whose
blast radius you accept.

| Guarantee | Claude arguments |
| --- | --- |
| Headless, streamed events | `--print --output-format stream-json --verbose` |
| Unattended, no prompt to answer | `--dangerously-skip-permissions` |
| Pinned session identity | `--session-id <fresh uuid>` |
| Job name | `--name <job>` |
| Model override | `--model <m>` when set |
| Effort override | `--effort <level>` when set |
| Task | `-- <prompt>` |

### Environment

Each run starts with a cleared environment. Installed schedules capture values in the plist, so exporting a variable later does not update them. Changing environment defaults in `config` requires a subsequent job save to reinstall schedules. [The internal install command](cli.md#internal-commands) can print the compiled plist.

| Source | Variables passed |
| --- | --- |
| Installing or running shell | `HOME`, `USER`, `TMPDIR`, and names in `env` |
| Fixed values | `LANG=en_US.UTF-8`, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` |
| Launch PATH | `~/.local/bin`, `~/.cargo/bin`, `~/.opencode/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`; also used to resolve `claude` |
| `aws_profile` or `aws_region` | All shell `AWS_` variables for credentials, then the configured `AWS_PROFILE` and `AWS_REGION` over them; every harness resolves AWS the same way, so the pair is not Claude's alone |
| `bedrock: true` | The switch the harness definition names, `CLAUDE_CODE_USE_BEDROCK=1` for Claude |

`env` entries must be shell identifiers and cannot override execution policy: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, and prefixes `DYLD_`, `LD_` and `CLAUDE_CODE_` are rejected. Use the Bedrock fields for provider selection. An SSO profile still needs an authenticated session, such as one opened by `aws sso login --profile <name>`.

### Results

| Status | Reason | When | Exit |
| --- | --- | --- | --- |
| `started` | | Running; a saved status line may supply live cost. | |
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
| `prices.json` | Validated models.dev price snapshot with fetch time and SHA-256, refreshed in the background by the dashboard. Contains no session data. |
| `forks.json`, `forks.lock` | Confirmed conversation parent links, scoped by harness, native home and directory, with a lock for concurrent writers. See [fork controls](dashboard.md#fork-a-conversation). |
| `hidden` | Hidden run ids, one per line. Remove a line to restore the row without changing the ledger. |
| `output/<run_id>/events.jsonl` | Streamed events, capped at 64 MiB total and 1 MiB per line. |
| `output/<run_id>/stderr.log` | Captured stderr, capped at 1 MiB. |
| `transcripts/<run_id>/<session_id>.jsonl` | Archived transcript, when requested. |
| `logs/<job>.out.log`, `logs/<job>.err.log` | launchd's stdout and stderr. |
| `locks/admission/`, `locks/runs/` | Admission lock and one lease held for each live run. |
