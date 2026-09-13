<p align="center">
  <img src="assets/cones.svg" alt="cones" width="420">
</p>

<p align="center">
  <b>Traffic control for Claude Code.</b> Schedule repeated jobs, see every agent running on your Mac, and keep them out of each other's way.
</p>

<p align="center">
  <a href="https://github.com/YuvalSarel1/cones/releases"><img src="https://img.shields.io/badge/version-0.1.0-f97316" alt="version 0.1.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS-000000?logo=apple&logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/rust-2024_edition-b7410e?logo=rust&logoColor=white" alt="Rust 2024 edition">
  <img src="https://img.shields.io/badge/Claude_Code-2.1%2B-d97757" alt="Claude Code 2.1 or later">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-3b82f6" alt="MIT or Apache-2.0"></a>
</p>

```
$ cones validate
readme-check	valid	claude
$ cones run readme-check
173c4d8b-...	started	readme-check
173c4d8b-...	ok
$ cones logs 173c4d8b-...
Read  ~/personal/cones/README.md
1	# cones
...
Result: success  $0.018371
$ cones ls
173c4d8b-...	readme-check	ok	2026-09-12T09:09:25+00:00	claude	$0.02	-
2b2aa8d2-...	~/personal/cones	active	2026-09-12T13:54:25+00:00	claude	-	8.1M/45k
8077985c-...	~/personal/cones	idle	2026-09-12T13:54:23+00:00	claude	-	40.6M/157k
```

Real output, ids and paths shortened. One global hook puts every Claude session on the Mac, scheduled or interactive, into `cones ls` and a dashboard. launchd fires each job on a cron schedule, and a writer lock per directory keeps jobs and agents from editing the same tree at once. Each run goes out headless under a policy Claude itself enforces, dollar budget, turn cap, tool allowlist, read-only or sandboxed write, no MCP, no prompts, plus a timeout, and lands in a JSONL ledger with a status and a reason. cones runs no process between ticks.

## Why

Three things the agent fleet tools around it leave out:

| Gap | What cones does |
| --- | --- |
| Fleet tools list only the sessions they launched. | A global Claude Code hook writes one state file per session, so every Claude session on the Mac shows in `cones ls` and the dashboard, including sessions started from a terminal. |
| Dollar budgets and tool policy are left to the user. | `budget_usd` compiles to Claude's own `--max-budget-usd`; `tools` and `write` compile to `--tools` and `--allowedTools`; a run cannot prompt, load settings or reach an MCP server. |
| Scheduling needs the tool's own daemon. | Five-field cron compiles to `StartCalendarInterval` in a per-user LaunchAgent. |

The ownership rule, rule 1 of [AGENTS.md](AGENTS.md): cones owns the clock, supervision, budgets, locks and the ledger. Claude owns execution and permissions. Every tool call goes to Claude's own permission engine, and a guarantee Claude cannot enforce natively is a `cones validate` error.

## Install

Requires Rust and Claude Code.

```sh
cargo install --git https://github.com/YuvalSarel1/cones --tag v0.1.0
cones --version                  # cones 0.1.0
```

Or `cargo install --path .` from a checkout. Verified against Claude Code 2.1.269 on macOS 26.6.1; `cargo test --all-targets` and `cones doctor` repeat the check.

## Quick start

A job is one prompt, run in one directory, on one cron schedule, under one policy. `jobs.example.yaml` is a working read-only job; `jobs.yaml` is gitignored.

```sh
cp jobs.example.yaml jobs.yaml
cones validate                   # compile every job's policy
cones doctor                     # what would break a scheduled run
cones run readme-check           # run one now, read it back with cones logs <id>
cones install                    # write and load the LaunchAgents
cones hook --install             # every Claude session on the Mac in cones ls
cones tui                        # jobs, sessions and runs on one screen
```

`cones install --dry-run` prints the plists instead of writing them, `cones uninstall` removes them and keeps history, and both are idempotent. `cones run --prompt "fix the flaky test"` runs a one-off task in the current directory under the first job's policy.

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

## Docs

| | |
| --- | --- |
| [The job file](docs/jobs.md) | Every field, its default, and what `cones validate` rejects. |
| [What a run does](docs/runs.md) | The flags Claude is given, every status and reason in the ledger, overlap, budgets, the writer lock, launchd behavior across sleep and reboot. |
| [The fleet and the dashboard](docs/fleet.md) | The hook, the session state file, `cones ls`, stop and attach, `cones tui` keys. |
| [Command reference](docs/cli.md) | Every command and flag, one-off prompts, the `cones doctor` checks. |

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

Tests use fake harness processes and spend no model tokens. Rules for agents working here are in [AGENTS.md](AGENTS.md).

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
