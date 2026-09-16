# Command reference

Back to the [README](../README.md). Jobs and runs are in [jobs.md](jobs.md), the dashboard in [dashboard.md](dashboard.md), where every session fact comes from in [harness.md](harness.md).

Global flags: `--jobs PATH` (default `jobs.yaml`), `--state-dir PATH` (default `~/.cones`) and `--debug`.

| Command | What it does |
| --- | --- |
| `cones` | The dashboard, described in [dashboard.md](dashboard.md). No subcommand at all: everything below is machinery around it. |
| `cones validate` | Compile every job's policy; print `name valid harness` per job or fail. |
| `cones install [--dry-run]` | Write and load LaunchAgents for enabled jobs, remove disabled ones; `--dry-run` prints the plists (with `env` values) and installs nothing. |
| `cones uninstall` | Remove every `local.cones.*` LaunchAgent; keep runs and transcripts. |
| `cones run JOB [--trigger manual\|schedule]` | Run a job now. launchd passes `--trigger schedule`. |
| `cones run --prompt "..." [JOB]` | One-off task in the current directory under the named job's policy (default: the first job), or under the read-only defaults when there is no jobs file. Named `adhoc-<8 hex>`. |
| `cones ls [--job NAME] [--status S] [--json]` | Runs newest first, then live sessions oldest first by start time. Columns: id, job or cwd, status or state, fired time or the session's reported start (`-` when unavailable; sources in [harness.md](harness.md#observe)), harness, dollars, reason or tokens in/out. `--status` takes `started`, `ok`, `failed`, `timeout`, `skipped`, `crashed`, `active`, `idle`, `blocked`, `done`, `stopped` or `exited`. `--job` hides sessions. `--json` prints one object per line: `status`, `started` and `terminal` for runs; `status` and `session` for sessions. |
| `cones logs ID [--follow] [--raw]` | A run's events rendered as tool calls and text, with the harness stderr tail appended; `--raw` prints the JSON events. For Claude or Codex session ids, the last assistant headlines and optional new replies; `--raw` applies only to runs. pi message entries are not rendered. Ctrl+C detaches, the run keeps going. |
| `cones stop ID` | A run ends `failed` / `interrupted` after cones checks the worker pid still belongs to its supervisor. A session ends through `claude rm`, which also drops its record from `claude agents`, or SIGTERM, per the kinds table in [harness.md](harness.md#kinds). Prints `stop requested` or `already finished`. |
| `cones attach ID [--print-command]` | Resume a finished run in its cwd, restoring its archived Claude transcript if needed. Attach directly to a live Claude background session. Interactive Claude, Codex and pi sessions are refused by this CLI command; the dashboard can join Codex daemon threads. `--print-command` prints the command instead. A running headless run cannot be attached; follow its log. |
| `cones coordinator start [DIR]` | Launch the folder's coordinator: the start-orchestrator skill embedded in the binary, written to `~/.cones/coordinator/plugin` and loaded for one background Claude session in DIR (default: the current directory) with `--plugin-dir`; Claude prints the session id. When the skill's status file already names a live coordinator for that folder, print it and exit 0. Nothing is installed under `~/.claude`. See [coordinator.md](coordinator.md). |
| `cones doctor` | The checks listed below; `OK`/`WARN`/`FAIL` per line, exit 1 on any `FAIL`. |
| `cones --debug` | `--debug` appends to `STATE_DIR/tui-debug.log`: the terminal's state at start, every input event, each viewer's open (with its pid and command; one opened while the cursor rested is a `prespawn` line, and its end carries the last line of its stderr), focus, leave, close and exit, the time from a viewer's spawn to its first text (`viewer_first_paint`), how long a viewer opened while the cursor rested had been running when `enter` took it (`viewer_prespawn_hit`), command durations, refresh reads and discarded snapshots, transition-to-draw timings (`startup_to_draw`, `input_to_draw`, `action_result_to_draw`, `opening_result_to_draw`, `return_to_draw`) and frames taking at least 16ms (`slow_draw`). |

## Sessions at the shell

```sh
cones ls --status blocked        # sessions waiting on a permission, trust or user prompt
cones logs SESSION_UUID --follow # the session's transcript, Ctrl+C returns
cones attach SESSION_UUID        # the session in this terminal, Ctrl+Z comes back
cones stop SESSION_UUID          # ends the session
```

Every Claude Code, Codex and pi session on the Mac is a row, whoever started it; which rows `attach` and `stop` act on, and how, is the kinds table in [harness.md](harness.md#kinds). A pi session is seen and stopped, never joined: pi has no way to leave a session running.

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
| The dashboard can open each known harness and leave it running: `claude --help` lists `--bg` and `attach`; `codex app-server daemon version` runs (Codex 0.154 or later) | WARN |
| `jobs.yaml` parses and each job's policy compiles | FAIL |
| Each `env` name is set in this shell | FAIL |
| The installed plist has every `env` name and the full launchd PATH | FAIL, or WARN when the job is not installed |
| A job enables writes (with an additional note if it archives plaintext transcripts), or enables Codex full access | WARN |
| `claude` is on the launchd PATH and `claude --version` runs | FAIL |
| Claude version is inside the tested range `>=2.1, <3` | WARN |
| Every flag the compiler emits for a job that uses every option appears in `claude --help` | FAIL |
| `claude auth status --json` reports logged in; a scheduled job cannot prompt to log in | FAIL |
| `~/.claude/sessions`, Claude's session registry, and `~/.claude/projects`, its session store, exist | WARN |
| `~/.claude/settings.json` has no entries left from the removed `cones hook`; delete those whose command ends in ` hook $PPID` | WARN |
| `~/.cones/runs.jsonl` is readable and writable | FAIL |
