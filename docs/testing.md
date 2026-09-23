# Testing and resource use

Use `scripts/check` for validation. It queues work across cones checkouts on the
same machine by what it costs. Formatting and the JavaScript reporter contract run
first and take no slot, so a formatting failure returns at once. Compiling (Clippy,
`cargo test --no-run` and `install`) takes a build slot: two builds may run at
once, under background QoS (`taskpolicy -b`), which keeps them on the efficiency
cores. Running the built tests takes the test slot, which is exclusive, because
wall-clock tests flake under load; it waits for running builds, and a waiting test
run holds back new builds so they cannot starve it. Tests run at normal priority.
Waiting checks print the holder's PID and checkout. Cancellation stops the gate's
process group. Each lease is inherited by Cargo, so killing the wrapper cannot
admit another check while its child still runs. The lock files under `~/.cones`
stay in place; the kernel releases a lease when its last owner exits. Builds hold
`check.lock` shared and test runs hold it exclusively, which is how older copies of
the wrapper, which hold it exclusively for a whole gate, still queue with this one.

The slots control calls through this script. Direct Cargo invocations and other
applications are outside them. There is no FIFO ordering guarantee or hard machine
CPU limit.

Inside a build, `CARGO_BUILD_JOBS` defaults to the number of efficiency cores
(`hw.perflevel1.logicalcpu`, at least `2`); `RUST_TEST_THREADS` defaults to `2`.
Explicit values take precedence. Each checkout retains its own build output;
sharing a queue does not share build artifacts. `CONES_CHECK_STATE_DIR` overrides
the lock directory for isolated fixtures. Fixtures that invoke `scripts/check`
must use a temporary directory to avoid waiting on their outer gate.

Full output stays in the printed log directory. The terminal shows stage results
and counts per Rust suite; failures retain their exit status and bounded diagnostics.
`usage.json` records total queue wait, execution time excluding it, worker limits,
child CPU time, maximum child RSS and each lease's stage, kind and wait. Maximum child RSS is not the combined memory of the process tree.
Tests run without inherited native-home overrides, including `CODEX_HOME`,
`CLAUDE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, `OPENCODE_DB`, `OPENCODE_TUI_CONFIG`
and `XDG_DATA_HOME`. Tests of custom configuration set it explicitly on their
fixture subprocesses.
Dashboard unit fixtures keep terminal-only clients only when they belong to the
test process tree. Archive-backed readers retain their explicit fixture homes.
Integration assertions select every identity they create, including identities
expected to be excluded, so unrelated live clients cannot change their counts.

## Focused checks

```sh
scripts/check test --lib viewer::tests
scripts/check test --test runner
scripts/check reporting
scripts/check test --test core coordinator
```

The coordinator is part of the binary, so its checks are Cargo's. `cones coordinator`
covers the claim, the wake gate, mail and delivery; the skill that reads them ships as
prose alone and has no helpers of its own to test.
The normal gate requires Python 3 and Node.js 22 or later alongside Rust.
CI uses the same `scripts/check` entry point.
One test compiles Claude's argv, which resolves the executable on
`harness::launch_path()`, so the suite needs a `claude` on that path. The
assertion is the argument list rather than anything the binary does, so CI
installs a stub for it.

## Installing HEAD

```sh
scripts/check install
```

This builds the current commit in a detached worktree under `~/.cones` and installs
it with `cargo install --root ~/.local`, so the binary carries committed code only.
It takes a build slot, at background priority like any other build. The
release artifacts persist in `~/.cones/install-target`, so a later install rebuilds
the crate rather than its dependencies. `CONES_INSTALL_ROOT` overrides the install
prefix.

## Resource and native checks

```sh
scripts/check stress
CONES_STRESS_CYCLES=100 scripts/check stress
CONES_STRESS_SECONDS=3600 CONES_STRESS_INTERVAL_MS=1000 scripts/check stress
python3 scripts/check-opencode.py target/debug/cones /absolute/path/to/opencode
python3 scripts/check-terminals.py target/debug/cones
python3 scripts/check-owned-harnesses.py target/debug/cones /absolute/path/to/claude /absolute/path/to/pi
python3 scripts/check-rename.py
python3 scripts/check-stop.py target/debug/cones /absolute/path/to/claude
```

Stress is separate from the default gate. It runs two ignored macOS tests sequentially,
using disposable homes, Python terminal fixtures and the real dashboard
preparation, cancellation, input and close paths. History and transcript workers
are repeatedly refreshed and dropped. No native agent or model is started.

The printed `resources.json` records process RSS, descriptors, threads, cumulative
CPU, average CPU core use, pump, close and input acknowledgement latency. These
measure the fixture dashboard process, not total machine or native harness usage.
The first four cycles warm caches. Subsequent cycles must release descriptors and
threads and leave no owned viewer process; final RSS may retain up to 16 MiB over
the warm baseline. Maximum pump, close and input latency budgets are 500 ms,
1000 ms and 1000 ms.
The wrapper stops a stalled stress gate after five minutes.

The second stress test measures observation rather than lifecycle: several dashboards
refreshing a populated fixture fleet at once, with pinned folders and worktrees.
Every refresh is checked against a fixed budget as it happens, so a regression names
the pass that broke it: one whole-table read, no subprocess for a database or for
liveness, and no more than three processes plus one per folder. The printed
`observation.json` records passes, spawns per pass and per second by operation,
worst refresh latency, CPU and retained resources.

`CONES_STRESS_SECONDS` sets the duration, `CONES_STRESS_DASHBOARDS` how many
refresh at once, and `CONES_STRESS_INTERVAL_MS` how long each waits between
refreshes; the defaults keep the gate short and unpaced, which is what checks the
per-refresh budget. A soak sets the interval to the cadence a dashboard really
refreshes at, because what it measures is what accumulates over an hour. The soak is an hour, which spans
the interval a churning dashboard wedged this machine over, and the wrapper's
five-minute cap extends with the duration asked for.

Run stress for changes to worker lifetimes, output pumping, caches or concurrency,
and native checks for changes to harness integration. The OpenCode script requires
tmux and uses isolated homes with a loopback provider, without inherited credentials
or external model calls. It covers launch, native identity and reported columns,
empty/drafted input and menus, viewer reuse, stop, preview and resume. Screens and
diagnostics stay in its printed temporary directory. Its results complement fixture tests.
Investigate a failed budget before adjusting it. Correctness tests synchronize
with explicit readiness signals; performance budgets belong in stress checks.

The terminal integration suite starts the real detached host and exercises native
process identity, unsent input, detached output, resize, explicit stop, malformed
clients, concurrent attachment and custom stdin/environment. Its dashboard check
uses an isolated zsh and verifies quit/reopen and crash/reopen with the same PID
and draft. The OpenCode check also reconnects its native editor after dashboard
closure. The owned-harness check uses a loopback Anthropic fixture for native
Claude forks and pi, checking their identity, model, transcript, PID and draft.
Fixture cleanup explicitly stops hosted terminals; closing a dashboard
alone is no longer sufficient cleanup for those fixtures.

The stop check launches a real background Claude session in a disposable non-default native
home against a loopback provider, then stops it through `cones stop`. It verifies the session
ends, the job record reports `stopped`, the transcript keeps its turn, the cones row survives as
stopped, a repeated stop stays the native answer, and an unknown id is refused. Run it for
changes to stop, to the native stop operation or to session discovery.

The rename check uses the installed Claude and Codex CLIs with disposable homes
and a loopback provider. It exercises Ctrl+N through the dashboard handler for
manual names and empty submissions, accepts Codex's generated suggestion, and
checks the persisted native names for the exact discovered session IDs.

## Harness acceptance

Use disposable native homes and loopback providers. Follow a fresh dashboard launch through
identity, every promised column, empty and drafted input, menus, stop, history and resume.
Check reporting loss, concurrent clients and custom configuration, including the configured
AWS profile/region. Assert the actual dashboard data path and reject missing promised values.

Record the tested CLI version and distinguish support for owned viewers, external clients,
CLI listings and supervised jobs. State unsupported and unverified cases explicitly.
The default gate does not run the native scripts above; run the relevant ones for integration
changes. Validate the exact delivered diff, including its tests.

## Maintaining coverage

Prefer strengthening an existing test to adding another for the same behavior.
Combine repetitive inputs when the expected behavior is the same. Remove a
duplicate only after checking that another test detects the same regression.
For questionable assertions, temporarily break the relevant behavior in an
isolated checkout and confirm that the test fails. Never run mutation checks
against a tree shared with other agents.
