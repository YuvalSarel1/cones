<p align="center">
  <img src="assets/cones.svg" alt="cones: traffic control for coding agents" width="460">
</p>

<p align="center">
  <a href="https://github.com/YuvalSarel1/cones/releases"><img src="https://img.shields.io/badge/version-0.1.0-f97316" alt="version 0.1.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS-000000?logo=apple&logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/rust-2024_edition-b7410e?logo=rust&logoColor=white" alt="Rust 2024 edition">
  <img src="https://img.shields.io/badge/Claude_Code-2.1%2B-d97757" alt="Claude Code 2.1 or later">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-3b82f6" alt="MIT or Apache-2.0"></a>
</p>

<p align="center"><a href="assets/tui.svg"><img src="assets/tui.svg" alt="cones tui: the scheduled jobs, every Claude Code session on the Mac grouped by directory, and the selected job's policy" width="100%"></a></p>

## What it does

**cones shows every Claude Code session on your Mac, including the ones it did not start, schedules headless ones, and keeps them from writing over each other. One binary, no daemon.**

* **See every session:** View all Claude Code sessions in `cones ls` or the dashboard, including ones started from any terminal. Check status, attach, or stop.
* **Schedule jobs:** Run headless Claude tasks on a cron schedule using macOS launchd.
* **Prevent write conflicts:** Share one writer lock per directory across jobs, agents, and commands. Choose whether overlapping runs skip, run alongside, or replace the previous run.

Each run cones starts has enforced limits on spending, turns, tools, writes, and duration. No MCP servers or interactive prompts. Status, reason, and cost are recorded in a JSONL log. Sessions started elsewhere are visible, and you can attach to or stop them, but budgets and locks cover only runs cones starts and commands wrapped in `cones lock`.

Run a task immediately with `cones run`, or orchestrate agents in a folder with `cones coordinator start`.

Reference: [the job file](docs/jobs.md), [what a run does](docs/runs.md), [the fleet and the dashboard](docs/fleet.md), [what cones needs from a harness](docs/harness.md), [command reference](docs/cli.md).

## Install

Requires Rust and Claude Code 2.1 or later, on macOS.

```sh
cargo install --git https://github.com/YuvalSarel1/cones --tag v0.1.0
cones tui                        # the dashboard above, every Claude session on the Mac already in it
```

To schedule a job, describe it in `jobs.yaml` (`jobs.example.yaml` is a working read-only one), then `cones validate`, `cones run <job>` to try it now, and `cones install` to load it into launchd. `cones doctor` says what would break a scheduled run.

```yaml
version: 1
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"
    harness: claude
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    budget_usd: 2.00
    timeout_min: 30
    write: true
    tools: ["Read", "Grep", "Glob", "Edit", "Write"]
    overlap: skip
```

## Philosophy

cones owns the clock, supervision, budgets, locks and the ledger. The harness owns execution and permissions. Every tool call goes through Claude's own permission engine, and if Claude cannot enforce a guarantee natively, `cones validate` rejects the job. No best effort, no second permission engine. Coordination between agents on one tree is a skill cones ships and starts, not cones logic; [the fleet doc](docs/fleet.md#the-coordinator-one-session-per-folder) has the details.

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>
