# Commands

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md)

Plain `cones` opens the dashboard. `run` starts supervised work from the shell or launchd; `catchup` recovers missed schedules at login.

## Global flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--jobs PATH` | `jobs.yaml` | Configuration file. |
| `--state-dir PATH` | `~/.cones` | Relocate cones state, including stored runs and dashboard records. |
| `--debug` | off | Append [diagnostics](#diagnostics) to the state directory. |

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

`--debug` writes `STATE_DIR/tui-debug.log`. Use it to distinguish a slow read, viewer startup and dashboard drawing.

| Records | Contents |
| --- | --- |
| Terminal and input | Initial terminal state and every input event. |
| Viewer lifetime | Open command and pid, focus, leave, close and exit. `prespawn` marks a viewer opened while the cursor rested; its end includes the last stderr line. |
| Viewer startup | `viewer_first_paint`: spawn to first text; `viewer_prespawn_hit`: age of a speculative viewer when entered. |
| Commands and reads | Command durations, refresh reads and discarded snapshots. |
| Drawing | `startup_to_draw`, `input_to_draw`, `action_result_to_draw`, `opening_result_to_draw`, `return_to_draw`; `slow_draw` for frames taking at least 16 ms. |

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
