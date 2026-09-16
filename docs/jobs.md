# Scheduled jobs: the file and what a run does

Back to the [README](../README.md). The dashboard is in [dashboard.md](dashboard.md), commands in [cli.md](cli.md), what each flag asks of the harness in [harness.md](harness.md#trigger).

`jobs.yaml` contains `version: 1`, optional policy `defaults`, and `jobs`. Dashboard settings (`columns`, `sparkline`, `pane`, `start`, `confirm_secs`) are described in [dashboard.md](dashboard.md#columns). Unknown fields are rejected.

Jobs override policy defaults individually. Claude uses `defaults.model` and `max_turns`; Codex uses `defaults.codex_model` and `codex_full_access`, though Codex execution is currently unavailable. A job's own `model` is its override. The composer shares model and provider defaults but starts on `start.harness`; jobs inherit `defaults.harness`.

The dashboard's `config` button edits defaults and dashboard settings. The `jobs` screen adds jobs through `new job`, edits with `ctrl+e`, and deletes with `ctrl+x twice`; see [the wizard](dashboard.md#the-wizard).

```yaml
version: 1
defaults:
  timeout_min: 30
  budget_usd: 2.00
  daily_budget_usd: 10.00
  write: false
columns: [state, context, sparkline, model, age, last]   # dashboard session columns, see dashboard.md
pane:                 # which side the viewer pane sits on, see dashboard.md
  at: right
start:                # what a new cones terminal comes up with, see dashboard.md
  harness: claude
  pane: true
sparkline:            # the sparkline column's window, metric and scale, see dashboard.md
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
    max_turns: 5
    overlap: skip                  # skip | allow | replace
    notify: true                   # macOS notification when a run fails, times out or is skipped on budget
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
| `enabled` | `true` | `false` records each tick as `skipped` with reason `disabled`, and `cones install` removes that job's LaunchAgent. |
| `archive_transcript` | `false` | Copy Claude's transcript into `~/.cones/transcripts/<run_id>/<session_id>.jsonl` when the run ends. |
| `env` | `[]` | Names of shell variables to pass through. Values are read at install or run time; installed schedules retain them in the plist. Standard variables and Bedrock credentials are handled separately below. |
| `timeout_min` | `30` | Runner timeout. Positive, at most 10080 (one week). |
| `budget_usd` | `2.00` | Per-run cap, passed as `--max-budget-usd`. |
| `daily_budget_usd` | none | Rolling 24-hour cap per job. At least `budget_usd`. |
| `write` | `false` | `false` allows Read, Grep and Glob. `true` adds Edit, Write and Bash, and turns on Claude's sandbox. There is no per-tool list: Claude treats a scoped `Bash(pattern)` rule as a pre-approval, not an exclusive allowlist, so cones cannot promise one. |
| `max_turns` | none | Passed as `--max-turns`. |
| `overlap` | `skip` | `skip`, `allow` or `replace`: what a tick does while the previous run is still going. |
| `notify` | `false` | macOS notification (`osascript`) when a run is `failed` or `timeout`, or `skipped` with reason `budget`. `CONES_NOTIFIER` names a command that receives the title and message instead. |
| `codex_full_access` | `false` | Codex only. Rejected on a Claude job; as a default it reaches Codex jobs alone. |
| `bedrock` | none | `true` runs the job on Amazon Bedrock: Claude gets `CLAUDE_CODE_USE_BEDROCK=1` and every `AWS_` variable of the installing shell. A Codex job is refused, because the app-server daemon keeps the provider of the Codex configuration it started with and ignores what a thread asks for. `true` needs `aws_profile` and `aws_region` beside it and is rejected without them. `false` asks for the harness's own endpoint. Unset leaves it to the harness's own settings. `model` aliases such as `sonnet` resolve on either provider; a full model id is the provider's. |
| `aws_profile` | none | Required by `bedrock: true`, ignored without it. The profile, as named in `~/.aws/config`, that the run gets as `AWS_PROFILE`. It is set over an `AWS_PROFILE` the installing shell carried in, since it is the one the job was checked against. |
| `aws_region` | none | Required by `bedrock: true`, ignored without it. The region the run gets as `AWS_REGION`, as in `us-east-1`. A model id is answered only by the regions that carry it. |

## Validation

`cones validate` compiles every job's policy and prints `<name>  valid  <harness>`, or the first error with the job's name. Beyond the per-field rules it rejects:

- A schedule that restricts both day and weekday while one uses a wildcard step. launchd ORs the two fields where cron ANDs them.
- `bedrock: true` with no `aws_profile` or no `aws_region`, on the job or in `defaults`. These fields must be explicit in the file; shell values do not satisfy this check.
- `bedrock` on a Codex job, from the job or from `defaults`. cones cannot hold a Codex session to it, so it is a validation error rather than a setting that does nothing.
- An `env` name that could change execution policy: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, or anything starting with `DYLD_`, `LD_` or `CLAUDE_CODE_`. Names must be valid shell identifiers. Bedrock is the `bedrock` field, not an `env` name.
- `version` other than `1`, a duplicate name, a `cwd` that is not a directory, `daily_budget_usd` below `budget_usd`, `max_turns` on a job that is not Claude's, `codex_full_access` on a job that is not Codex's, a `claude` binary missing from the launchd PATH.

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
| Tool allowlist | `--tools` and `--allowedTools`, both `Read,Grep,Glob` or with `write` `Read,Grep,Glob,Edit,Write,Bash` |
| Pinned session ID | `--session-id <fresh uuid>`. cones generates it, so the ledger, the transcript, `attach` and the fleet agree; a mismatch ends the run with reason `session_mismatch`. |
| Dollar budget | `--max-budget-usd <budget_usd>` |
| Job name | `--name <job>` |
| Sandbox when `write` is true | `--settings` with `sandbox.enabled` and `sandbox.failIfUnavailable` true, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands` false, `excludedCommands` empty |
| Model, turn cap | `--model <m>`, `--max-turns <n>` when set |
| The task | `-- <prompt>` as the final positional argument |

The timeout is the runner's: at `timeout_min` the whole process group gets SIGTERM, then SIGKILL after two seconds. The worker also ends itself one second past the timeout or when its supervisor disappears.

The harness starts with a cleared environment: `HOME`, `USER` and `TMPDIR` from the installing shell, a fixed `PATH` (`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`) from which `claude` is resolved, `LANG=en_US.UTF-8`, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, and the names listed in `env`; with `bedrock: true`, also `CLAUDE_CODE_USE_BEDROCK=1`, every `AWS_` variable the installing shell has for the credentials themselves, and `AWS_PROFILE` and `AWS_REGION` from the job's own `aws_profile` and `aws_region` over them. `cones install --dry-run` prints the plist XML including those values, so it can contain secrets; it warns on stderr when a job imports any.

## What a run records

A run is one supervised harness process. `cones run` takes a global admission lock, reaps orphaned runs, applies skip rules, spawns a gated worker in its own process group, appends `started`, releases the worker, and reads the harness's JSON events until the process exits, the timeout passes, a permission is denied, or a stop arrives. On timeout, stop, replace or permission denial the whole process group is terminated: SIGTERM, two seconds, SIGKILL.

| Status | Reason | When | Exit |
| --- | --- | --- | --- |
| `started` | | Running. Dollars show as `-` until it ends. | |
| `ok` | | Claude exited 0 with a `success` result and a reported cost. | 0 |
| `skipped` | `disabled` | `enabled: false` | 0 |
| `skipped` | `overlap` | `overlap: skip` and a previous run is still going | 0 |
| `skipped` | `budget` | The daily reservation would exceed `daily_budget_usd` | 0 |
| `skipped` | `replace_unconfirmed` | `overlap: replace` and the previous run did not confirm shutdown within 10 seconds | 0 |
| `timeout` | `timeout` | The runner's clock ran out | 124 |
| `timeout` | `replaced` | This run was the old one under `overlap: replace`; it received SIGUSR1 and stopped | 124 |
| `failed` | `interrupted` | `cones stop`, or SIGTERM / SIGINT to the runner | 1 |
| `failed` | `permission` | Claude reported a permission denial: a `permission_denials` entry on the result or a `permission_denied` system event. A sandboxed command the OS refuses is not one; the sandbox blocks it and the run goes on | 1 |
| `failed` | `session_mismatch` | An event carried a session id other than the pinned one | 1 |
| `failed` | `missing_result`, `missing_cost` | Claude exited without a result event, or with one that had no `total_cost_usd` | 1 |
| `failed` | `validation: ...`, `spawn: ...`, `runner: ...` | The policy did not compile, the worker could not start, or cones hit an error while supervising | 1 |
| `failed` | `exit`, or Claude's result subtype | Nonzero exit with no other explanation, or a result other than `success`, recorded verbatim | 1 |
| `failed` | `orphan` | The supervisor died; written when the next run of that job reaps it | |
| `crashed` | | Derived at read time: a `started` record with no terminal record past its timeout plus five seconds. | |

The `started` record has `trigger` (`manual` or `schedule`), `session_id`, `cwd`, `pid`, `pgid`, `timeout_s`, `budget_usd`, the compiled `policy` and its SHA-256 `policy_hash` (session id, job name and prompt normalized out, so a compiler flag change changes the hash). The terminal record has `duration_s`, `exit`, `tokens_in`, `tokens_out`, `cost_usd`, `reason` and, when archived, `transcript`.

State lives in `~/.cones` (`--state-dir` to isolate). Directories are created `0700` and files `0600`.

| Path under `~/.cones` | Contents |
| --- | --- |
| `runs.jsonl` | The ledger: one `started` and one terminal record per run. Appends hold an exclusive lock; a partial last line from a killed writer is repaired on the next append. |
| `hidden` | One id per line: runs and Codex daemon threads `ctrl+x` hid or forgot in the dashboard. The ledger and the thread are untouched; delete a line to show the row again. |
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

## Budgets: a per-run cap Claude enforces and a rolling daily reservation

`budget_usd` is Claude's own `--max-budget-usd`. `daily_budget_usd` is a rolling 24-hour reservation per job: the ledger sums the job's runs from the last 24 hours, counting a run's actual cost when its record has one and its `budget_usd` while it is still going or when it ended without a reported cost, and a tick whose own `budget_usd` would push that sum over the cap is `skipped` / `budget`. The reservation is checked before a `replace` sends SIGUSR1, so a budget skip leaves the previous run going.

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
| `EnvironmentVariables` | The environment described under [What the harness is told](#what-the-harness-is-told), including the values of variables named in `env`, so a variable exported after install needs a reinstall |

`cones install` rewrites and re-bootstraps only plists whose content changed, bootstraps ones that are on disk and not loaded, and boots out the LaunchAgent of any job now disabled. `cones uninstall` boots out and deletes every `local.cones.<name>.plist` whose `Label` matches its file name, refuses to continue when one does not, and keeps every ledger record and transcript.

Per launchd.plist(5), ticks missed while the Mac sleeps coalesce into one launch on wake, so a wake starts at most one run per job and `overlap` decides if the previous run is still going; nothing runs at login or on `cones install`. Ticks that pass while the Mac is powered off or you are logged out are lost, and launchd does not wake the Mac.

Tests check the plist configuration; physical sleep/wake and reboot behavior has not been verified. A live check should show one `schedule` record after a slept-through tick and none for a tick missed while powered off.

## Codex and pi

`harness: codex` and `harness: pi` parse, but no execution adapters are available. Validation and installation fail because cones cannot enforce their dollar budgets natively. Running such a job records `failed` with a `validation: ...` reason. `max_turns` is Claude-only, and `codex_full_access` is Codex-only.

Native Codex and pi sessions still appear in the fleet. Codex daemon threads can be joined through the dashboard; pi stays in its own terminal. See [harness.md](harness.md#kinds).
