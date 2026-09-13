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

## What it does

One binary, no daemon.

| | |
| --- | --- |
| **Schedule** | A cron line becomes a launchd LaunchAgent that starts a headless Claude run on time. Nothing runs between ticks. |
| **See the fleet** | Every Claude Code session on the Mac, whichever terminal started it, in `cones ls` and the dashboard. Read from the registry Claude keeps itself, nothing installed. Stop or attach from either. |
| **Keep them apart** | One writer lock per directory, shared by jobs and `cones lock . -- git commit`. When a job's last run is still going, the next one skips, runs alongside or replaces it. You pick, per job. |

Each run carries a policy Claude enforces itself: budget, turns, tools, writes, timeout. No MCP servers, no prompts. It ends in a JSONL ledger with a status, a reason and a cost.

`cones run --prompt "fix the flaky test"` runs one task now under the same policy. `cones coordinator start` starts one orchestrator session for the agents in a folder.

Reference: [the job file](docs/jobs.md), [what a run does](docs/runs.md), [the fleet and the dashboard](docs/fleet.md), [command reference](docs/cli.md).

## What existing tools leave out

| Gap | cones |
| --- | --- |
| Fleet tools see only the sessions they launched. | Reads the registry Claude Code writes for every session. |
| Two agents in one tree collide at commit time. | One writer lock per directory, shared by jobs, agents and `cones lock`. |
| Budgets and tool policy are on you. | `budget_usd`, `tools` and `write` become Claude's own flags. A run cannot prompt, load settings or reach an MCP server. |
| Scheduling needs the tool's own daemon. | Cron becomes a per-user LaunchAgent. |

## Philosophy

cones owns the clock, supervision, budgets, locks and the ledger. The harness owns execution and permissions. Every tool call goes through Claude's own permission engine. If Claude cannot enforce a guarantee natively, `cones validate` rejects the job. No best effort, no second permission engine.

Coordination between agents on one tree, greetings, commit gating, relayed findings, is a skill, not cones logic. cones ships it and starts it, and stays a kernel. Claude Code is the harness today. Codex jobs parse and wait on a native dollar budget. Rules for agents working on cones are in [AGENTS.md](AGENTS.md). Tests use fake harness processes and spend no model tokens.

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>
