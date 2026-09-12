# cones

Scheduled coding-agent jobs on a local Mac. Each job runs under an explicit policy the harness enforces natively, and every run lands in a durable ledger.

`v0.1.0` runs Claude Code jobs on a launchd schedule, streams their output live, shows jobs, every live Claude session and runs in a native dashboard, and resumes finished sessions in Claude's own TUI. Codex is recognized but not yet executed.

## Install

Requires Rust and Claude Code.

```sh
cargo install --git https://github.com/YuvalSarel1/cones --tag v0.1.0
```

Or from a checkout:

```sh
git clone https://github.com/YuvalSarel1/cones && cd cones
cargo install --path .
cones --version                  # cones 0.1.0
```

Verified against Claude Code 2.1.269 on macOS 26.6.1. To repeat the check, run `cargo test --all-targets` in the checkout and `cones doctor` after installing.

## Try it

```sh
cp jobs.example.yaml jobs.yaml   # gitignored
cones validate
cones doctor
cones run readme-check
cones ls
cones logs RUN_UUID --follow
cones tui                        # dashboard: jobs, live sessions, runs
```

Happy with the schedule? `cones install --dry-run` prints the launchd plists, `cones install` writes them, `cones uninstall` removes them and keeps history. Installs are idempotent.

## A job

```yaml
version: 1
defaults:
  timeout_min: 30
  budget_usd: 2.00
  daily_budget_usd: 10.00
  write: false
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

What the job gets: a pinned session ID, print mode, a native dollar budget, a runner timeout, no prompts, no user or project settings, no MCP servers, and a strict sandbox when Bash is enabled. `daily_budget_usd` is a rolling 24-hour reservation. Anything the harness cannot enforce natively fails `cones validate`; there is no best-effort fallback.

`overlap` decides what happens when a job is still running at its next tick. Writers on the same directory are serialized across jobs regardless. `cones lock . -- git commit -m msg` takes that same writer lock from a shell or another agent, waiting until scheduled writers on the directory finish, and exits with the command's status.

## Runs

```sh
cones tui                        # dashboard: claude agents keys (enter runs a job, follows a running run, or opens a session in this terminal, ctrl+z comes back; ctrl+x twice stops a run, ctrl+s regroups, esc quits) plus n new task, / filter, r refresh
cones run --prompt "fix the flaky test"   # one-off task under the first job's policy (or read-only defaults), in the cwd
cones ls --status failed         # tab-separated; --json for records
cones logs RUN_UUID --follow     # Ctrl+C detaches, the run keeps going
cones stop RUN_UUID_OR_SESSION_ID   # a cones run, or any Claude session the fleet hook saw
cones attach RUN_UUID            # resume the finished session in Claude's TUI
```

With the fleet hook installed (`cones hook --install`), every Claude Code session on the Mac appears in `cones ls` and the dashboard too, whether cones started it or not: working directory, `active`, `idle`, `blocked` or `exited`, age, tokens in/out and estimated cost. Sessions that belong to a cones run collapse into that run's row, and a session whose process is gone is not shown. `cones attach SESSION_UUID` opens a live session in this terminal (Claude's own `claude attach`, Ctrl+Z comes back) and resumes one whose process is gone; `cones logs SESSION_UUID --follow` follows a session's transcript; `cones ls --status blocked` lists the ones waiting on a prompt.

A run is `ok`, `failed`, `timeout`, `skipped` or `crashed`, with a reason such as `permission`, `budget`, `overlap` or `workspace`. A finished run's row in the dashboard and in `cones ls` shows the dollars it spent, from the ledger; a running one shows none yet. On timeout, stop or permission denial the whole process group is terminated. State lives in `~/.cones` (`--state-dir` to isolate): the `runs.jsonl` ledger, per-run events and stderr, archived transcripts, launchd logs.

Schedules compile to `StartCalendarInterval` entries in a per-user LaunchAgent with `RunAtLoad` false. Per launchd.plist(5), ticks missed while the Mac sleeps coalesce into one launch on wake, so a wake starts at most one run per job and `overlap` decides if the previous run is still going; nothing runs at login or on `cones install`. Ticks that pass while the Mac is powered off or you are logged out are lost, and launchd does not wake the Mac. This has not been observed through a real lid-close or reboot yet; the check is `cones ls --json` showing one `schedule` record fired after a slept-through tick and none after a reboot past one.

## Fleet

```sh
cones hook --install             # one global Claude Code hook in ~/.claude/settings.json
cones doctor                     # hook installed, Claude login, job env vars, Claude version and flag drift
```

`cones doctor` fails when Claude is not logged in or a variable a job imports with `env` is unset in the shell or missing from the installed plist (launchd bakes the value at install time, so a variable exported later needs a reinstall), since a scheduled job can prompt for neither, and warns when the installed Claude Code is outside the tested range (`>=2.1, <3`). The flags it checks against `claude --help` are the ones the compiler emits, so that list cannot drift from the compiler.

The hook runs on SessionStart, UserPromptSubmit, PostToolUse, Notification, Stop and SessionEnd and writes one JSON file per session to `~/.cones/fleet/<session_id>.json`: cwd, pid, state (`active`, `idle`, `blocked` on a permission or elicitation prompt, `exited`; the idle-prompt notification Claude sends a minute after a turn ends keeps a session `idle`), the last event and tool, and input/output tokens summed from the transcript at the end of each turn. Every Claude session on the Mac becomes visible, whether cones started it or not. The hook only observes; it is not on PreToolUse and never gates a tool call. Re-running `--install` replaces the earlier entry, so a moved binary or `--state-dir` is picked up. To remove it, delete the entries ending in `hook $PPID` from the settings file.

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

Tests use fake harness processes and spend no model tokens. Rules for agents working here are in [AGENTS.md](AGENTS.md).

Licensed under either Apache-2.0 or MIT, at your option.
