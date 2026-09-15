<p align="center">
  <img src="assets/cones.svg" alt="cones: Traffic control for agents." width="520">
</p>

**A dashboard for coding agents on your Mac.** See what is running, move between sessions, and start, stop or schedule work.

Supports Claude Code, Codex and pi sessions. Scheduled jobs currently use Claude Code.

<p align="center">
  <img src="https://img.shields.io/badge/platform-macOS-000000?logo=apple&logoColor=white" alt="macOS">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-3b82f6" alt="MIT or Apache-2.0"></a>
</p>

[Install](#install) · [Quick start](#quick-start) · [Documentation](#documentation)

<p align="center"><a href="assets/tui.svg"><img src="assets/tui.svg" alt="cones dashboard: jobs and live sessions grouped by folder, and recent runs" width="100%"></a></p>

## What you can do

* **See what is running.** Find sessions grouped by folder, including ones cones did not start. See their state, recent activity and token usage as each harness reports them.
* **Move between sessions.** Preview output, open supported sessions and return to the dashboard while they keep working.
* **Schedule recurring work.** Run Claude Code jobs with budgets, timeouts and tool permissions, then review their output and cost.
* **Coordinate agents.** Start the bundled coordinator in a folder to help agents share findings and coordinate changes. It appears alongside the other sessions.

## Install

Requires macOS, Rust and Claude Code 2.1 or later, logged in. Claude Code must support background sessions (`--bg` and `attach`). For Codex sessions, use Codex 0.154 or later. pi sessions need nothing: any pi running in a terminal is a row.

```sh
cargo install --git https://github.com/YuvalSarel1/cones
```

From a checkout of this repository, `cargo install --path .` does the same.

## Quick start

Open the dashboard. Existing sessions appear automatically; no jobs file is needed.

```sh
cones tui
```

To start a session, choose `folder` in the menu and enter your project directory. Type an instruction and press `Enter` to start work there. `Tab` switches between Claude Code and Codex.

| Key | Action |
| --- | --- |
| `↑` `↓` | Select a row. |
| `Enter` | With no instruction typed, open a session, start a job or follow a run's output. |
| `Ctrl+Z` | Return from an opened session while it keeps working. |
| `Esc` | Clear the instruction, or quit the dashboard when it is empty. |

Sessions marked `own terminal` stay in their original terminal. See the [dashboard guide](docs/dashboard.md) for stopping sessions, previews and other controls.

### Run a task from the shell

```sh
cones run --prompt "Read this repo and summarize its TODOs."
```

This runs a supervised Claude Code task in the current directory. With no jobs file, its default policy allows reading only. With a valid jobs file, it uses the first job's policy.

### Schedule a task

Adapt [jobs.example.yaml](jobs.example.yaml) into `jobs.yaml`, setting `cwd` and `prompt` for your project. Check the configuration, run the example job once, then install its schedule:

```sh
cones validate
cones doctor
cones run readme-check
cones install
```

Schedules run through macOS launchd. The [jobs guide](docs/jobs.md) covers budgets, permissions and what happens when a previous run is still going.

### Start a coordinator

In a folder where agents are working:

```sh
cones coordinator start
```

It runs as a visible Claude Code session in that folder. Job policies apply to supervised runs; dashboard sessions and the coordinator use their harness's own permissions.

## Documentation

[Dashboard controls](docs/dashboard.md) · [Job configuration](docs/jobs.md) · [Coordinator](docs/coordinator.md) · [Harness support](docs/harness.md) · [CLI reference](docs/cli.md)

## Roadmap

<p align="center"><a href="assets/roadmap.svg"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></a></p>

[Roadmap source](assets/roadmap.py).
