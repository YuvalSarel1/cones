# Commands

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md)

Plain `cones` opens the dashboard. `run` starts supervised work from the shell or launchd; `catchup` recovers missed schedules at login.

## Global flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--jobs PATH` | `jobs.yaml` | Configuration file. |
| `--state-dir PATH` | `~/.cones` | Relocate cones state, including stored runs and dashboard records. |
| `--debug` | off | Append [diagnostics](#diagnostics) to the state directory. |
| `--trace` | off | Enable debug diagnostics plus input text, commands and individual timing samples. |

## Commands

| Command | Effect |
| --- | --- |
| `cones` | Open the dashboard; requires a terminal. |
| `cones run JOB [--trigger manual\|schedule]` | Run a configured job. Trigger defaults to `manual`; launchd passes `schedule`. |
| `cones run --prompt "..." [JOB]` | Run a [one-off task](#one-off-tasks). |
| `cones catchup [--dry-run]` | Recover [missed schedules](jobs.md#sleep-login-and-reboot). `--dry-run` prints `name missed <local time>` for each candidate and starts nothing. |

### One-off tasks

```sh
cones run --prompt "fix the flaky test"
cones run nightly-triage --prompt "summarize the failures"
```

The task runs in the current directory under the named job's policy, otherwise the first job's. With no readable, valid jobs file or no template job, it uses the [built-in policy defaults](jobs.md#job-fields-and-defaults). An explicit unknown job name is an error. Each task gets a fresh `adhoc-<8 hex>` name, so overlap is checked per task.

## Coordinator launch

The bundled [start-orchestrator skill](../assets/coordinator/skills/start-orchestrator/SKILL.md) coordinates agents sharing a folder. Its launcher is currently internal, hidden from `--help`, with no dashboard button:

```sh
cones __coordinator
cones __coordinator ~/src/app
```

The default folder is the current directory. A live coordinator record for that folder is printed and the launcher exits; otherwise cones writes its embedded plugin to `STATE_DIR/coordinator/plugin` and launches `claude --bg --plugin-dir <plugin> /cones:start-orchestrator` there. The skill also checks for duplicates. Nothing is installed in the user's plugin directory.

The coordinator appears as a native Claude session; [the dashboard](dashboard.md#sessions-and-runs) marks it. To end its coordination role, tell it `stop orchestrator`. This removes its status record, while its background session remains until separately stopped. Coordination rules belong to the skill.

The embedded copy is under `assets/coordinator/`. Updates from the upstream orchestrator project must retain `__CONES_COORDINATOR_BIN__`; cones replaces it with the installed helper directory.

## Diagnostics

`--debug` writes JSONL records to `STATE_DIR/tui-debug.log`. Each record has `v`, a UTC `timestamp`, `pid`, `dashboard_id`, `level`, `event` and `data`. The dashboard ID separates simultaneous dashboards and restarts. Related operations carry an `operation_id`; row events include the row kind, native identity, harness and discovery source when known.

| Events | Contents |
| --- | --- |
| `dashboard.started`, `dashboard.stopped` | Executable path, version and SHA-256 fingerprint; terminal state, configuration path and exit reason. |
| `row.*`, `view.changed` | Added, removed and changed rows, native identity replacement, selection, focus and status changes. Sources distinguish registry rows, process rows, saved launches, daemon locks, history and the ledger. |
| `input.key`, `input.paste`, `terminal.*` | Navigation and shortcut keys with modifiers and their input route; paste size and whether it was empty; terminal size and colors. Ordinary typed characters and mouse movement require trace. |
| `viewer.*` | Preparation, spawn, first text, focus, leave, close, exit and refusal reasons, with row identity and viewer pid. First-text timing starts at spawn; operation timing also covers preparation. A prespawn hit records the viewer's age separately. |
| `launch.*`, `action.*` | Requests and outcomes for launches, stops, deletes and forgets, including native errors and elapsed time. |
| `refresh.*`, `load.failed`, `discovery.failed`, `configuration.*` | Read outcomes, discarded snapshots, retained stale rows and recovery. Configuration errors are recorded when they change. |
| `history.*`, `transcript.*` | Request and worker durations, indexing and hydration timings, file/read counts, bytes read, cache hits, errors and discarded results. A cached transcript reports zero bytes read for that request. |
| `timing`, `timing.summary` | Slow individual operations and periodic counts, mean and maximum durations per phase. Normal samples are summarized every 30 seconds and on exit. Drawing, input-to-draw and viewer pumping are slow at 16 ms; other measured phases at 250 ms. |

`--trace` includes every timing sample, ordinary input text, mouse events and viewer commands. It implies `--debug`; it does not change harness execution or permissions. Both flags affect dashboard diagnostics only.

The debug file is capped at 10 MiB. When an append would exceed that bound, cones keeps roughly the newest 5 MiB of complete lines. A single oversized record retains its identity and a marked preview instead of invalid JSON. Writers open the file for each append; a file lock coordinates compaction across dashboards.

Every prompt submitted to start a harness session from the dashboard is also recorded once in `STATE_DIR/launches.jsonl`, including with debug off. These recovery records contain the launch operation ID, harness, folder and submitted prompt, so a launch that fails before creating a native session can still be recovered. This file uses the same 10 MiB bound. Debug events reference the operation ID without repeating the prompt.

Older text records can remain in the retained log tail. For example, this prints failures from the structured records and skips older lines:

```sh
jq -R 'fromjson? | select(.level == "error")' ~/.cones/tui-debug.log
```

## Internal commands

The dashboard and runner start these subprocesses. They are hidden from `--help` and may change with their callers.

| Command | Purpose |
| --- | --- |
| `__ls [--job NAME] [--status S] [--json]` | Read runs and live sessions. Text timestamps use your local timezone and include its UTC offset. JSON keeps UTC timestamps and is one object per line: `status`, `started`, `terminal` for runs; `status`, `session` for sessions. Native model ids are preserved. |
| `__logs ID [--follow] [--raw]` | Read captured output. The current session renderer does not handle pi message entries. |
| `__attach ID [--print-command]` | Open a background session or resume a finished run. |
| `__install [--dry-run]` | Compile and install schedules. Dry run prints plist XML, including imported credentials; stderr warns when a job imports values. |
| `__coordinator [DIR]` | [Launch the bundled coordinator](#coordinator-launch). |
| `__list` | Render dashboard rows for a subprocess caller. |
| `__worker --run-id ID` | Run the supervised worker. |
