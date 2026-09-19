# Testing and resource use

Use `scripts/check` for validation. It queues one gate at a time across cones
checkouts on the same machine, holding `~/.cones/check.lock` through formatting,
Clippy, Rust tests and the JavaScript reporter contract. Waiting checks print the
holder's PID and checkout. Cancellation stops the gate's process group. The lock
is inherited by Cargo, so killing the wrapper cannot admit a second check while
its child still runs. The lock file stays in place; the kernel releases the
lease when its last owner exits.

The slot controls calls through this script. Direct Cargo invocations and other
applications are outside it. It is a shared admission slot, with no FIFO ordering
guarantee or hard machine CPU limit.

`CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` default to `2` inside the gate.
Explicit values take precedence. Each checkout retains its own build output;
sharing a gate does not share build artifacts. `CONES_CHECK_STATE_DIR` overrides
the lock directory for isolated fixtures. Fixtures that invoke `scripts/check`
must use a temporary directory to avoid waiting on their outer gate.

Full output stays in the printed log directory. The terminal shows stage results
and counts per Rust suite; failures retain their exit status and bounded diagnostics.
`usage.json` records queue wait, execution time, worker limits, child CPU time and
maximum child RSS. Maximum child RSS is not the combined memory of the process tree.
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
scripts/check coordinator /absolute/path/to/orchestrator
```

The coordinator mode runs the upstream Python suite and verifies that its runtime
files match the embedded copy. Run it when changing embedded coordinator helpers.
Tests remain in the upstream repository; this check takes an explicit source
checkout and does not depend on a developer's personal directory layout.
The normal gate requires Python 3 and Node.js 22 or later alongside Rust.
CI uses the same `scripts/check` entry point.

## Resource and native checks

```sh
scripts/check stress
CONES_STRESS_CYCLES=100 scripts/check stress
python3 scripts/check-opencode.py target/debug/cones /absolute/path/to/opencode
```

Stress is separate from the default gate. It runs one ignored macOS test alone,
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

Run stress for changes to worker lifetimes, output pumping, caches or concurrency,
and native checks for changes to harness integration. The OpenCode script uses
a loopback provider and requires tmux. Its results complement fixture tests.
Investigate a failed budget before adjusting it. Correctness tests synchronize
with explicit readiness signals; performance budgets belong in stress checks.

## Maintaining coverage

Prefer strengthening an existing test to adding another for the same behavior.
Combine repetitive inputs when the expected behavior is the same. Remove a
duplicate only after checking that another test detects the same regression.
For questionable assertions, temporarily break the relevant behavior in an
isolated checkout and confirm that the test fails. Never run mutation checks
against a tree shared with other agents.
