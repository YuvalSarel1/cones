# Command reference

Back to the [README](../README.md). Job fields are in [jobs.md](jobs.md), run behavior in [runs.md](runs.md), the fleet and dashboard in [fleet.md](fleet.md).

Global flags: `--jobs PATH` (default `jobs.yaml`) and `--state-dir PATH` (default `~/.cones`).

| Command | What it does |
| --- | --- |
| `cones validate` | Compile every job's policy; print `name valid harness` per job or fail. |
| `cones install [--dry-run]` | Write and load LaunchAgents for enabled jobs, remove disabled ones; `--dry-run` prints the plists (with `env` values) and installs nothing. |
| `cones uninstall` | Remove every `local.cones.*` LaunchAgent; keep runs and transcripts. |
| `cones run JOB [--trigger manual\|schedule]` | Run a job now. launchd passes `--trigger schedule`. |
| `cones run --prompt "..." [JOB]` | One-off task in the current directory under the named job's policy (default: the first job), or under the read-only defaults when there is no jobs file. Named `adhoc-<8 hex>`. |
| `cones ls [--job NAME] [--status S] [--json]` | Runs newest first, then live sessions oldest first by start time. Columns: id, job or cwd, status or state, fired time or the session's start (its transcript's first timestamp, `-` when it has none), harness, dollars, reason or tokens in/out. `--status` takes `started`, `ok`, `failed`, `timeout`, `skipped`, `crashed`, `active`, `idle`, `blocked` or `exited`. `--job` hides sessions. `--json` prints one run record per line. |
| `cones logs ID [--follow] [--raw]` | A run's events rendered as tool calls and text, with the harness stderr tail appended; `--raw` prints the JSON events. For a session id, the last assistant lines of its transcript. Ctrl+C detaches, the run keeps going. |
| `cones stop ID` | A run ends `failed` / `interrupted` after cones checks the worker pid still belongs to its supervisor. A session ends through `claude stop` or SIGTERM as described in [fleet.md](fleet.md). Prints `stop requested` or `already finished`. |
| `cones attach ID [--print-command]` | A finished run or a session whose process is gone is resumed in the background and attached to, in its cwd; the archived transcript is restored into Claude's store if the native one is missing. A live session is attached directly. `--print-command` prints the command instead. A running headless run cannot be attached; follow its log. |
| `cones coordinator start [DIR]` | Launch the folder's coordinator: the start-orchestrator skill embedded in the binary, written to `~/.cones/coordinator/plugin` and loaded for one background Claude session in DIR (default: the current directory) with `--plugin-dir`; Claude prints the session id. When the skill's status file already names a live coordinator for that folder, print it and exit 0. Nothing is installed under `~/.claude`. See [fleet.md](fleet.md#the-coordinator-one-session-per-folder). |
| `cones doctor` | The checks listed below; `OK`/`WARN`/`FAIL` per line, exit 1 on any `FAIL`. |
| `cones tui` | The dashboard. |

## Run a prompt without a job

```sh
cones run --prompt "fix the flaky test"     # under the first job's policy, in the current directory
cones run nightly-triage --prompt "..."     # under a named job's policy
```

With no jobs file, or one that does not parse, the task runs under the read-only defaults (30 minutes, $2.00, Read/Grep/Glob); run `cones validate` first when you expect a job's policy. Each task gets a unique `adhoc-<8 hex>` name, so `overlap` is checked per task.

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
| `~/.claude/sessions`, Claude's session registry, and `~/.claude/projects`, its session store, exist | WARN |
| `~/.claude/settings.json` has no entries left from the removed `cones hook`; delete those whose command ends in ` hook $PPID` | WARN |
| `~/.cones/runs.jsonl` is readable and writable | FAIL |
