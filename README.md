<p align="center">
  <img src="assets/cones.svg" alt="cones" width="420">
</p>

<p align="center">
  <b>Traffic control for coding agents.</b>
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
cones hook --install             # every Claude session on the Mac shows in cones ls and cones tui
cones tui                        # the dashboard above
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

Traffic control means three things, from one binary with no daemon:

| | |
| --- | --- |
| **Schedule** | A five-field cron per job becomes a launchd LaunchAgent that starts a headless Claude run. cones runs no process between ticks. |
| **See the fleet** | One global hook puts every Claude Code session on the Mac, scheduled or interactive, into `cones ls` and the dashboard: title, state, age, context fill, last message. Stop or attach from either. |
| **Keep them apart** | A writer lock per directory. Jobs take it, `cones lock . -- git commit` takes it around any command, and a job whose previous run is still going skips, runs alongside or replaces it, per job. |

Around those: each run goes out under a policy Claude itself enforces, a dollar budget, a turn cap, a tool allowlist, read-only or sandboxed write, a timeout, no MCP servers and no prompts, and lands in a JSONL ledger with a status, a reason and the dollars spent. `cones run --prompt "fix the flaky test"` is a one-off task in the current directory under the same policy. `cones coordinator start` launches one orchestrator session for a folder's agents, running a skill that ships inside the binary.

Reference: [the job file](docs/jobs.md), [what a run does](docs/runs.md), [the fleet and the dashboard](docs/fleet.md), [command reference](docs/cli.md).

## What existing tools leave out

| Gap | What cones does |
| --- | --- |
| Fleet tools list only the sessions they launched. | The hook writes one state file per session, so sessions started from any terminal show up too. |
| Two agents in one tree find out about each other at commit time. | The writer lock is per directory and the same lock for jobs, agents and `cones lock`. |
| Dollar budgets and tool policy are left to the user. | `budget_usd` compiles to Claude's own `--max-budget-usd`; `tools` and `write` compile to `--tools` and `--allowedTools`; a run cannot prompt, load settings or reach an MCP server. |
| Scheduling needs the tool's own daemon. | Cron compiles to `StartCalendarInterval` in a per-user LaunchAgent. |

## Philosophy

cones owns the clock, supervision, budgets, locks and the ledger. The harness owns execution and permissions. Every tool call goes to Claude's own permission engine, and a guarantee Claude cannot enforce natively is a `cones validate` error, never a best effort and never a second permission engine.

Coordination between agents on one tree, greetings, commit gating, relayed findings, is a skill rather than cones logic: cones ships it and starts it, and stays a kernel. Claude Code is the harness that runs today; Codex jobs parse and wait on a native dollar budget. Rules for agents working on cones are in [AGENTS.md](AGENTS.md); tests use fake harness processes and spend no model tokens.

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>
