# The job file

Back to the [README](../README.md). Runtime behavior is in [runs.md](runs.md), commands in [cli.md](cli.md).

`jobs.yaml` is `version: 1`, an optional `defaults` block, a list of jobs, and an optional `columns` list for the dashboard. `defaults` accepts the policy fields `timeout_min`, `budget_usd`, `daily_budget_usd`, `write`, `tools`, `max_turns`, `overlap`, `notify` and `codex_full_access`; each job may override them. Unknown fields anywhere in the file are rejected. The dashboard's wizard (`ctrl+n`, `ctrl+e` and `ctrl+x` in `cones tui`) adds, edits and deletes a job by rewriting only its block; see [fleet.md](fleet.md#the-job-wizard-ctrln-ctrle-ctrlx).

```yaml
version: 1
defaults:
  timeout_min: 30
  budget_usd: 2.00
  daily_budget_usd: 10.00
  write: false
columns: [state, model, activity, context, last]   # dashboard session columns, see below
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
| `harness` | required | `claude`. `codex` parses and is refused at validation (see [Codex](#codex-parsed-refused-at-validation) below). |
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

## Dashboard columns

`columns` picks what a session row shows after its icon, harness and title. Order is kept. An unknown name fails validation, so a column cones cannot fill never renders as a dash. Each cell reads one line of Claude's own transcript or registry, named in [fleet.md](fleet.md#the-fleet-every-claude-session-on-the-mac); it is `-` until that line exists, never an estimate.

| Column | Cell | Default |
| --- | --- | --- |
| `state` | working, needs input, idle or exited | yes |
| `model` | The bare API model id on the last message with usage, `claude-fable-5-1` | yes |
| `age` | Time since the transcript's first timestamp, `4s`, `6m`, `2h` | no |
| `activity` | Time since the transcript's last timestamp | yes |
| `context` | `98k`: the prompt size Claude reported on the last message. No window, so no percentage | yes |
| `last` | First line of the last reply, or the directory when grouped by state | yes |
| `tokens` | `49.2M/201k`: input and output tokens summed over the session | no |

Cost is not a column: Claude Code writes tokens to the transcript and no price, so live sessions have no dollars to show. Run rows take theirs from the ledger.

`cones validate` compiles every job's policy and prints `<name>  valid  <harness>`, or the first error with the job's name. Beyond the per-field rules it rejects:

- `Bash(pattern)` rules other than `Bash(*)` with `write: true`. Claude treats a scoped Bash rule as a pre-approval, so other commands still reach ordinary permission checks; use `Bash` for sandboxed Bash, or stay read-only. A read-only job strips the rules with the rest of Bash.
- A schedule that restricts both day and weekday while one uses a wildcard step. launchd ORs the two fields where cron ANDs them.
- An `env` name that could change execution policy: `HOME`, `PATH`, `SHELL`, `BASH_ENV`, `ENV`, `NODE_OPTIONS`, `CLAUDE_CONFIG_DIR`, or anything starting with `DYLD_`, `LD_` or `CLAUDE_CODE_`. Names must be valid shell identifiers.
- `version` other than `1`, a duplicate name, a `cwd` that is not a directory, `daily_budget_usd` below `budget_usd`, `max_turns` or `tools` on a Codex job, an unknown tool name, a `claude` binary missing from the launchd PATH.

## Codex: parsed, refused at validation

`harness: codex` is parsed: Codex jobs take no `tools` list, choose `write: false` (read-only) or `write: true` (workspace-write), and may set `codex_full_access`. Status: no Codex adapter exists in v0.1.0. `cones validate` and `cones install` stop with `codex execution is not available in v0.1; its dollar budget cannot yet be enforced`, `cones doctor` reports the job as FAIL, and `cones run` of such a job records a `failed` run with reason `validation: ...`. The blocker is a native dollar budget for Codex; see the roadmap in the [README](../README.md).
