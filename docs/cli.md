# Command reference

Back to the [README](../README.md). Jobs and runs are in [jobs.md](jobs.md), the dashboard in [dashboard.md](dashboard.md), where every session fact comes from in [harness.md](harness.md).

cones is the dashboard. Plain `cones` opens it, and the only other commands are `run`, which launchd fires for a scheduled job and you can call yourself for a one-off task, and `catchup`, which a login agent uses for the ticks a sleeping Mac missed. Everything else the old subcommands did at the shell is a key or a save in the dashboard.

Global flags: `--jobs PATH` (default `jobs.yaml`), `--state-dir PATH` (default `~/.cones`) and `--debug`.

| Command | What it does |
| --- | --- |
| `cones` | The dashboard, described in [dashboard.md](dashboard.md). It needs a terminal and says so when it does not have one. |
| `cones run JOB [--trigger manual\|schedule]` | Run a job now. launchd passes `--trigger schedule`. |
| `cones run --prompt "..." [JOB]` | One-off task in the current directory under the named job's policy (default: the first job), or under the read-only defaults when there is no jobs file. Named `adhoc-<8 hex>`. |
| `cones catchup [--dry-run]` | Start one run per job whose ticks passed while the Mac was off or logged out, for jobs with `catch_up: once`. The login agent runs this; `--dry-run` prints `name missed <local time>` per job and starts nothing. See [jobs.md](jobs.md#schedules-on-launchd-sleep-login-and-reboot). |
| `cones --debug` | `--debug` appends to `STATE_DIR/tui-debug.log`: the terminal's state at start, every input event, each viewer's open (with its pid and command; one opened while the cursor rested is a `prespawn` line, and its end carries the last line of its stderr), focus, leave, close and exit, the time from a viewer's spawn to its first text (`viewer_first_paint`), how long a viewer opened while the cursor rested had been running when `enter` took it (`viewer_prespawn_hit`), command durations, refresh reads and discarded snapshots, transition-to-draw timings (`startup_to_draw`, `input_to_draw`, `action_result_to_draw`, `opening_result_to_draw`, `return_to_draw`) and frames taking at least 16ms (`slow_draw`). |

## Run a prompt without a job

```sh
cones run --prompt "fix the flaky test"     # under the first job's policy, in the current directory
cones run nightly-triage --prompt "..."     # under a named job's policy
```

With no jobs file, or one that does not parse, the task runs under the read-only defaults (30 minutes, $2.00, Read/Grep/Glob). Each task gets a unique `adhoc-<8 hex>` name, so `overlap` is checked per task.

## Machinery

The dashboard runs cones again for the things that need their own process, and the runner runs itself for a supervised worker. These names start with `__`, are hidden from `--help`, and are not an interface to script against: `__ls [--job NAME] [--status S] [--json]`, `__logs ID`, `__attach ID`, `__install`, `__coordinator [DIR]`, `__list` and `__worker --run-id ID`. They change with the dashboard that calls them.

`__ls --json` is what the harness checks in [harness.md](harness.md) read, one object per line: `status`, `started` and `terminal` for runs, `status` and `session` for sessions.

`__coordinator [DIR]` is the one with nothing in the dashboard behind it yet: it launches the folder's coordinator, the start-orchestrator skill embedded in the binary, as one background Claude session. See [coordinator.md](coordinator.md).
