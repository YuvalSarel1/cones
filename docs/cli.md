# Commands

[README](../README.md) · [Dashboard controls](dashboard.md) · [Configuration and runs](jobs.md)

Plain `cones` opens the dashboard. `launch` starts a session in a folder; `run` starts supervised work from the shell or launchd; `catchup` recovers missed schedules at login.

## Global flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--jobs PATH` | `~/.cones/jobs.yaml` | Configuration file. One per machine, like the state directory; the dashboard reads the same one from any folder. |
| `--state-dir PATH` | `~/.cones` | Relocate cones state, including stored runs and dashboard records. |
| `--debug` | off | Append [diagnostics](#diagnostics) to the state directory. |
| `--trace` | off | Enable debug diagnostics plus input text, commands and individual timing samples. |

## Native CLI lookup

Install and authenticate each CLI separately. cones searches `~/.local/bin`, `~/.cargo/bin`, `~/.opencode/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin`, in that order. Shell aliases and additional shell PATH entries are not used. Make executables installed elsewhere available in one of these directories. Config's connectivity check reports missing binaries or required flags.

## Commands

| Command | Effect |
| --- | --- |
| `cones` | Open the dashboard; requires a terminal. |
| `cones run JOB [--trigger manual\|schedule]` | Run a configured job. Trigger defaults to `manual`; launchd passes `schedule`. |
| `cones run --prompt "..." [JOB]` | Run a [one-off task](#one-off-tasks). |
| `cones launch [PROMPT] [--dir PATH] [--harness NAME] [--print-command]` | [Start a session](#starting-a-session) the way the dashboard's composer does. |
| `cones catchup [--dry-run]` | Recover [missed schedules](jobs.md#sleep-login-and-reboot). `--dry-run` prints `name missed <local time>` for each candidate and starts nothing. |
| `cones ls [--dir PATH] [--job NAME] [--status S] [--json]` | [Read runs and live sessions](#reading-runs-and-sessions). |

### Reading runs and sessions

`cones ls` prints the dashboard's rows for a script: the ledger's runs, then the live sessions no run owns. Text timestamps use your local timezone and include its UTC offset. `--job` and `--status` narrow the read; naming a job leaves the sessions out, since a session belongs to no job.

`--dir` keeps the rows whose folder is that path or sits under it. This includes worktrees stored inside the directory; linked worktrees elsewhere are not included merely because they share a repository. Both sides are resolved first, so a folder reached through a symlink still matches. A row that reports no folder is not in any folder, so a scoped read leaves it out.

`--json` writes one object per line. `kind` is `run` or `session` and says which of the two shapes follows: `status`, `started` and `terminal` for a run; `status` and `session` for a session. Timestamps stay UTC and native model ids are preserved.

```sh
cones ls --dir ~/src/app --json
```

The [coordinator](#coordinator-launch) reads its folder this way instead of walking the harness registries itself.

### Starting a session

```sh
cones launch "fix the flaky test"
cones launch --dir ~/src/app --harness codex "rebase onto main"
cones launch --print-command --harness claude
```

The session starts in `--dir`, otherwise the current directory. An explicit `--harness` selects that harness and errors if it is disabled. Without it, an enabled `defaults.harness` wins, otherwise the first enabled launchable harness is used. No enabled harness is an error. The model, effort, provider and Bedrock settings come from `defaults`.

This CLI selection is separate from the dashboard's `start.harness`. The `_in_picker` switches affect the dashboard cycle, not `cones launch`. With no prompt the session opens waiting for input.

Claude starts as a background session, prints its identifier and returns, so the row is there for the dashboard to attach. Every other harness is its own terminal client and takes over this terminal, as a resumed session does. `--print-command` prints the folder, environment and command instead of starting anything.

Unlike the composer, this launcher writes no recovery record, so a prompt it fails to deliver is the one in your shell history.

### One-off tasks

```sh
cones run --prompt "fix the flaky test"
cones run nightly-triage --prompt "summarize the failures"
```

The task runs in the current directory under the named job's policy, otherwise the first job's. With no readable, valid jobs file or no template job, it uses the [built-in policy defaults](jobs.md#job-fields-and-defaults). An explicit unknown job name is an error. Each task gets a fresh `adhoc-<8 hex>` name, so overlap is checked per task.

## Coordinator launch

The bundled [start-orchestrator skill](../assets/coordinator/skills/start-orchestrator/SKILL.md) resolves overlapping work, shares relevant findings and integrates completed changes in a folder. Task scope stays with the owner and each worker. Its launcher is currently internal, hidden from `--help`, with no dashboard button:

```sh
cones __coordinator
cones __coordinator ~/src/app
```

The default folder is the current directory. A live coordinator record for that folder is printed and the launcher exits; otherwise cones writes its embedded plugin to `STATE_DIR/coordinator/plugin` and launches `claude --bg --plugin-dir <plugin> /cones:start-orchestrator` there. The skill also checks for duplicates. Nothing is installed in the user's plugin directory.

The coordinator appears as a native Claude session; [the dashboard](dashboard.md#sessions-and-runs) marks it. To end its coordination role, tell it `stop orchestrator`. This removes its status record, while its background session remains until separately stopped. Coordination rules belong to the skill.

Its roster is `cones ls --dir <folder> --json`, so who counts as a worker is decided here: unclaimed spares, Codex thread attribution, viewer and daemon processes and each harness's reported state. The skill does not read the native homes itself. An install whose `cones` predates the command makes the coordinator report the failed read instead of deriving a roster of its own.

The embedded copy is under `assets/coordinator/`. The upstream orchestrator's `bin/sync.py /path/to/cones` copies the skill and helpers, retaining `__CONES_COORDINATOR_BIN__` for cones to fill at launch; `--check` detects drift. Start a fresh coordinator after updating because an existing session retains its loaded instructions.

Codex delivery requires a running local app-server with native queue add/list/delete support. Requests name an exact thread and an active task. Completion closes the task; the watcher withdraws expired pending requests. Already consumed requests cannot be recalled. The helper preserves owner messages and owner-edited queue entries. Installation, cleanup and publishing are project-specific assignments, not generic coordinator duties.

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

Every prompt submitted to start a harness session from the dashboard is also recorded once in `STATE_DIR/launches.jsonl`, including with debug off, as `launch.submitted`. Its `data` contains `operation_id`, `harness`, `cwd` and the submitted `prompt`, so a launch that fails before creating a native session can still be recovered.

Reviving a conversation from history records `resume.submitted` in the same file. For that event, `data.operation_id` contains the source session id and `data.prompt` contains its saved title, or an empty string; the title is not sent as a new instruction. The record describes the resume request, not proof that it succeeded. This file uses the same 10 MiB bound. Debug launch events reference the operation ID without repeating the prompt.

Older text records can remain in the retained log tail. For example, this prints failures from the structured records and skips older lines:

```sh
jq -R 'fromjson? | select(.level == "error")' ~/.cones/tui-debug.log
```

## Internal commands

The dashboard and runner start these subprocesses. They are hidden from `--help` and may change with their callers.

| Command | Purpose |
| --- | --- |
| `__logs ID [--follow] [--raw]` | Read captured output. The current session renderer does not handle pi message entries. |
| `__attach ID [--print-command]` | Open a background session or resume a finished run. |
| `__install [--dry-run]` | Compile and install schedules. Dry run prints plist XML, including imported credentials; stderr warns when a job imports values. |
| `__coordinator [DIR]` | [Launch the bundled coordinator](#coordinator-launch). |
| `__list` | Render dashboard rows for a subprocess caller. |
| `__worker --run-id ID` | Run the supervised worker. |

Session JSON includes `cost_info` when cost was reported or estimation was attempted. It identifies the source and coverage of `cost_usd`, including pricing snapshot metadata for calculated estimates. See [cost estimates](harness.md#cost-estimates).
