# cones

Scheduled coding-agent jobs on a local Mac, with explicit execution policy and a durable run ledger.

`v0.1.0-headless` runs governed Claude jobs, exposes live output through `logs --follow`, and resumes finished sessions in Claude's native TUI. Native interactive background sessions are a different execution mode and do not provide the same budget contract. Physical sleep/wake verification remains pending.

## Build and try

Requires Rust and Claude Code. The tested Claude version is 2.1.268.

```sh
source "$HOME/.cargo/env"
cargo test --all-targets
cargo install --path .
cp jobs.example.yaml jobs.yaml
cones validate
cones doctor
cones run readme-check
cones ls
cones logs RUN_UUID --follow
cones stop RUN_UUID
```

`jobs.yaml` is ignored by Git. Relative working directories resolve against the jobs file, and `~/` expands before launchd receives any paths.

After reviewing the generated schedule:

```sh
cones install --dry-run
cones install
cones uninstall
```

`install --dry-run` writes the generated XML to stdout. It warns on stderr when that output includes named environment values, which may be secrets. Actual installation writes private plists under `~/Library/LaunchAgents`, using the absolute path of the running cones binary. Install from `~/.cargo/bin/cones` for a stable executable path.

Installation is idempotent. Disabled definitions unload their existing agents. `uninstall` removes all `local.cones.*` LaunchAgents and retains history. Removing a job from YAML does not unload its existing agent; run `uninstall` before installing a reduced set.

LaunchAgents survive reboots and resume scheduling after the user logs in. They coalesce calendar events missed during sleep on wake. They do not wake the Mac, run before login, or replay events missed while powered off or logged out. Numeric five-field cron supports lists, ranges and steps. A wildcard day step combined with a restricted other day field is rejected because launchd cannot represent its cron semantics faithfully.

## Jobs and policy

```yaml
version: 1
defaults:
  timeout_min: 30
  budget_usd: 2.00
  daily_budget_usd: 10.00
  write: false
jobs:
  - name: nightly-triage
    schedule: "0 2 * * *"
    harness: claude
    cwd: ~/src/myrepo
    prompt: "Read the TODOs and draft TRIAGE.md."
    write: true
    tools: ["Read", "Grep", "Glob", "Edit", "Write"]
    model: sonnet
    max_turns: 5
    overlap: skip
    enabled: true
    archive_transcript: true
    # Import only named environment variables. Keychain login needs no entry.
    # env: ["ANTHROPIC_API_KEY"]
```

Unknown fields, duplicate names, invalid schedules, missing working directories and invalid budgets fail validation. There is no permissive fallback for an unsupported harness policy.

Claude runs with:

- A pinned UUID, print mode and streaming JSON.
- `dontAsk` and `--permission-prompts none`. Actual permission denials terminate the run.
- Native tool availability and permission settings. `write: false` removes Edit, Write and Bash. Supported tools are Read, Grep, Glob, Edit, Write and Bash. Scoped Bash rules are rejected when Bash is enabled: Claude treats them as pre-approvals and can also allow built-in read-only commands, so they are not an exclusive command allowlist. Choose read tools or explicitly grant sandboxed Bash.
- Safe mode, restricted mode, no inherited user/project settings sources, no MCP servers and disabled slash commands. User and project customizations are disabled; administrative managed policy remains authoritative.
- A native dollar budget, runner timeout, and optional `max_turns`. Dollar limits have Claude's native enforcement granularity, not a prepaid billing guarantee.
- A strict native Bash sandbox when Bash is exposed. Sandbox auto-approval and unsandboxed retries are disabled. Claude's sandbox allows its working directory and session temp directory; file tools are confined by restricted mode. Failure to initialize the sandbox is fatal.

The subprocess receives a small explicit environment. `PATH` contains `~/.local/bin`, `~/.cargo/bin`, Homebrew and system directories. Authentication variables must be named in `env`; launchd does not inherit the shell. Secrets are not included in the policy hash or ledger.

Cones does not intercept tool calls or implement the harness SDK control protocol. A guarantee unsupported by native flags is a validation error. Run output and stderr are captured in private files under `output/<run_uuid>/` for diagnosis.

`daily_budget_usd` reserves the full per-run budget before starting a job. Completed runs use reported cost; missing cost retains the full reservation. Accounting covers a rolling 24 hours, including runs that ended within that window. An insufficient remainder creates `skipped / budget`.

Codex and Pi are recognized but execution is rejected in this version: dollar-budget enforcement is not implemented for them. A Codex `tools` declaration receives the specific sandbox-selection error. No token-to-dollar prices are guessed.

A model that declines a request without attempting a tool may complete successfully. Permission failure is based on an observed denied tool call, not an inference about the prompt's meaning.

Permission classification uses structured denial events. OS error text is considered only for a known Bash call with `is_error: true`, matching exact errno endings. Reading a file that discusses permissions does not fail the run.

## Overlap and workspace ownership

`overlap` is a per-job setting, also available in `defaults`:

| Value | Behavior |
| --- | --- |
| `skip` | Compatibility default. An active run of this job produces `skipped / overlap`. |
| `allow` | Read-only runs of the same job may execute concurrently. Dollar reservations still apply. |
| `replace` | Request shutdown of this job's prior runs, wait for confirmation, then start fresh. Replaced runs record `timeout / replaced`. |

Writers also take a lock on their canonical `cwd`, shared across job names. A different writer already using that directory produces `skipped / workspace`. A replacement never stops another job. Aliases through symlinks resolve to the same lock; distinct paths inside a repository should use a common job `cwd` if they need serialization.

`allow` with `write: true` fails validation until worktree-per-run exists. A short admission lock makes overlap decisions and budget reservations atomic; a separate per-run lease identifies live owners without relying on PID existence alone. If replacement cannot confirm shutdown within ten seconds, it records `skipped / replace_unconfirmed` and starts no new agent.

## Run lifecycle and re-entry

```sh
cones run nightly-triage
cones ls --job nightly-triage
cones ls --status failed
cones ls --json
cones logs RUN_UUID --follow
cones logs RUN_UUID --raw
cones stop RUN_UUID
cones attach RUN_UUID
cones attach SESSION_UUID --print-command
```

`ls` outputs tab-separated run ID, job, status, fire time, harness, cost and reason. `ls --json` includes both original records and the derived status.

The runner starts a supervisor in a separate process group, durably appends `started`, then opens an execution gate. Before that gate, the supervisor cannot launch the harness. A normal run appends one terminal record. The full compiled policy is stored alongside its hash.

On timeout, cancellation or permission denial, cones sends SIGTERM to the process group, waits two seconds, then sends SIGKILL. Remaining group members are cleaned up after successful runs too. The supervisor also watches for runner death and enforces its own timeout. A subprocess that deliberately creates a different process group is outside that cleanup boundary.

An unterminated start becomes `crashed` in `ls` after its recorded timeout plus five seconds. Admission reaps abandoned runs of that job. A writer also reaps abandoned writers from other jobs using its `cwd` before starting. Reaping appends `failed / orphan` and verifies the supervisor's run UUID before signalling a surviving group.

Closing a CLI output pipe does not stop a run. `logs --follow` displays captured events as they arrive; Ctrl+C detaches the follower. `stop` verifies the supervisor UUID and its parent PID before requesting termination.

Headless runs can be resumed only after completion. `attach` restores an archived transcript when the native file is missing, then executes `claude --resume <pinned UUID>` from the original directory. Resume is an interactive session under the user's current harness settings; the completed scheduled run's timeout and budget do not govern that new interaction.

## State and control plane

The default state directory is `~/.cones`. Use `--state-dir PATH` for an isolated instance and `--jobs PATH` for a different jobs file.

```text
~/.cones/config.toml
~/.cones/runs.jsonl
~/.cones/locks/admission/global.lock
~/.cones/locks/runs/<run_uuid>.lock
~/.cones/locks/workspaces/<cwd_hash>.lock
~/.cones/logs/<job>.out.log
~/.cones/logs/<job>.err.log
~/.cones/transcripts/<run_uuid>/<session_uuid>.jsonl
~/.cones/output/<run_uuid>/events.jsonl
~/.cones/output/<run_uuid>/stderr.log
```

The ledger is the authoritative run state. Writes are locked and synced. A torn final line is repaired under that lock; corruption in a complete line fails explicitly. State and archive directories use `0700`, files use `0600`. Archives are plaintext. Locks, launchd logs and optional archives are additional filesystem artifacts, so the literal “one state file” design claim has been narrowed.

Configuration:

```toml
control_plane = "none"
agent_console_url = "http://127.0.0.1:7878"
```

The default runs headless without probing another application. Selecting `control_plane = "agent-console"` probes its health endpoint and records why a run falls back to `live: false`.

The event stream and ledger are the control plane; agent-console is an optional viewer for interactive sessions.

## Roadmap

<p align="center"><img src="assets/roadmap.svg" alt="cones roadmap: Now, Next, Later" width="100%"></p>

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Tests use isolated fake harness processes, including TERM-resistant descendants, permission failures, malformed output, overlap, runner crashes and native transcript lookup. Model calls are not part of `cargo test`.

The library separates job configuration, harness compilation, control-plane selection, launchd generation, the ledger and the runner. It imports no agent-console implementation.

Licensed under either Apache-2.0 or MIT, at your option.
