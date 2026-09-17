# Harness definitions

Back to [harness behavior and sources](harness.md).

Each built-in harness has one definition under `assets/harnesses/`: `claude.yaml`, `codex.yaml` or `pi.yaml`. The binary embeds these files. Editing a definition takes effect after rebuilding cones; cones does not load user definitions or overrides from disk. `jobs.yaml` is unchanged.

The definitions collect the decisions used by discovery, commands, live reports, history and viewer input. `src/harness/spec.rs` reads them into typed Rust structures, rejects unknown fields and incompatible handler combinations, and exposes one registry in composer cycle order. Shared code handles OS parsing, directory walking, event matching, text extraction, caching and key dispatch. Definitions supply the harness-specific names, paths, mappings, conditions and strategy choices. Complex native identity matching, usage accounting and Claude's registry-state precedence remain named code mechanisms.

## Fields

| Field | Meaning |
| --- | --- |
| `version` | Definition schema version, currently `1`. |
| `kind`, `name` | Registered harness identity and executable name. They must agree. |
| `icon`, `colour` | The dashboard mark and optional RGB color. A null color uses the dashboard's dim style. |
| `home` | Native home environment variable, sibling default relative to the caller's Claude directory, and optional discovery of sibling homes with a named marker file. |
| `discovery` | Native discovery handler, registry location or process name, and process subcommands that are not sessions. pi erases its argv, so its subcommand list stays empty. |
| `state` | Ordered event guards, discriminator paths and state mappings, or the native Claude registry-precedence handler. Unknown values either preserve prior state or map explicitly to `-`. |
| `transcript` | Native metadata/usage handler; live and history roots, recursion depth and archive status; user/assistant message sources, identifiers, text block types, attachment labels and headline order. Claude also declares its saved statusline window source. |
| `launch` | Native launch handler, identity handover strategy, initial session kind, argument prefix, optional remote arguments, model source and prompt arguments. |
| `probe` | Capability command, exit-status requirement, required output strings, version extraction and diagnostic text. |
| `default_session`, `session_kinds` | Join, stop and lifetime behavior for the reported native session kind. The default preserves behavior for a missing or unrecognized kind. |
| `commands` | Native attach, historical resume, removal and unarchive arguments, the resume handler, and the viewer label. |
| `execution` | Whether native enforcement and result reporting are supported, unsupported or unknown. Only the compiled Claude execution adapter can authorize a supervised run. |
| `input` | Return-key bindings, their capture conditions, and the native empty-editor recognition profile. |
| `viewer` | Speculative-join mechanism and excluded states, entered-viewer retention, and input alignment. |

The OS `ps` arguments, column splitting, UTC timestamp format and unreadable-table errors are shared code in `fleet.rs`. They do not vary by harness. YAML only supplies the process name and subcommands to exclude; native argv interpretation, writer-lock binding and cwd attribution are mechanisms selected by the discovery handler.

`home.sibling: null` means the caller already supplied the Claude root. Other defaults replace the final component of that root: `.codex` or `.pi/agent`. A nonempty native environment override takes precedence for those homes, including a relative override. Empty overrides keep the default.

History canonicalizes native homes. Its identity remains `(harness, canonical native home, session id)`: aliases of one home collapse, separate homes remain distinct, and transcript copies under project directories within one home collapse by identity. Joining a Codex row uses the home its rollout belongs to. Historical resume carries the entry's saved home into the native environment.

## Reports and history

An event state rule names string-valued guards, a JSON pointer to the discriminator, mapped state words and an optional unknown-value result. Codex maps `task_complete` to `done`; unrelated events preserve earlier state. pi maps assistant `stopReason` and explicitly reports `-` for an unknown reason. Claude's multi-field precedence remains its native handler because it depends on registry and job state together.

Message sources name their event guards, boolean exclusion flags, content path and shape. Shared text extraction understands strings and arrays of content blocks. Definitions select block types and attachment labels. Codex selects the actual UI `UserMessage` records and its legacy `user_message` event, excluding injected model-history instructions. Its assistant headline uses the first text block; pi uses the last. The history preview retains the complete selected text and applies shared control-character stripping, bounded reads and caching. History and live viewers stay separate UI modes.

## Arguments and capabilities

Command templates are argument arrays. Placeholders occupy a whole argument:

```yaml
resume: [--remote, "{remote}", resume, --, "{id}"]
```

The accepted placeholders depend on the command: `prompt`, `remote`, `cwd`, `id`, `short_id` and `transcript`. Unknown placeholders and missing required operands are errors. Substitution retains spaces, newlines and native path bytes in one argument. It performs no shell expansion. A typed handler sequences Claude's background resume followed by attach, or Codex's unarchive followed by resume; command values travel as positional shell arguments and a failed first command prevents the second.

Launching, joining a live session and resuming history are separate operations. pi has no live attach, but can resume history through `--session <transcript>`. A composer pi returns to the viewer cones already owns. Claude background sessions and daemon-held Codex threads can be joined speculatively. Codex's `viewer.peek: existing_daemon` checks that its daemon is running; explicit history resume may start it. Its `viewer.retention: retain` keeps an entered viewer even though a speculative one can be reopened. The shared pool manager applies the policy; its pool sizes and the retention of resumed runs and historical viewers remain dashboard invariants.

`launch.identity` selects background-id, client-pid or reported-thread handover. The reported-thread mechanism keeps the existing requirement for a unique new thread matching the launch's prompt, cwd and start time, with no competing launch. A definition cannot weaken that ambiguity check or substitute a title for identity.

The definitions do not supply supervised command flags. `Harness::compile`, result handling, permission validation and timeout supervision remain code. Declaring Codex or pi execution support in YAML is rejected because cones has no native execution adapter for them. `unknown` means unverified, not unsupported.

## Viewer input

Each harness declares which keys return to cones and when they are captured:

```yaml
input:
  return_to_list:
    - {key: ctrl+z, when: always}
    - {key: tab, when: always}
    - {key: left, when: empty_prompt}
  empty_prompt: bordered
  markers: ""
  ignore_braille: false
```

The current keys are `ctrl+z`, `tab` and `left`; conditions are `always` and `empty_prompt`. Removing a binding passes that key through to the native client. A conditional Tab binding lets the harness keep Tab while a draft is populated. Unknown keys, duplicate bindings and definitions with no unconditional way back are rejected. Quit, layout switching and emulator scroll keys remain dashboard controls.

The defaults return on Tab and Ctrl+Z for all focused live viewers, keeping the native process running. Left returns only when that harness's empty-editor profile matches the rendered screen. Shift+Tab and modified arrows remain native keys. The opened viewer records its harness identity so input behavior does not depend on a title or on which row discovery most recently selected.

The marker profile recognizes Claude and Codex's prompt marker before a visible terminal caret. The bordered profile recognizes pi's standard editor: one empty row between horizontal borders, with an inverse-video software caret at the terminal cursor position. pi normally hides that terminal cursor and has no prompt marker. Text anywhere in the editor row, a multiline draft, a missing software caret or a non-editor screen keeps Left in pi. This profile follows the installed pi 0.85.1 `Editor.render` and `MainScreenTUI.positionHardwareCursor` behavior. A custom editor that does not match it can still return through Tab or Ctrl+Z.

## Changing or adding a harness

Start with the reported behavior in [harness.md](harness.md). Change the definition when a command, capability, source location or input profile changes. Change its typed handler when the native protocol needs different interpretation. Keep the native report fixtures under `assets/harnesses/fixtures/` and expected behavior in `src/harness/spec/tests.rs` current together.

A new built-in also needs a `HarnessKind` and a registration in `spec.rs`. Select existing typed strategies where they fit; add a handler for a new protocol. The current native-handler validation explicitly checks the three supported integrations. Extending it requires evidence and tests for the new one. User-installed packages are future work; the embedded definitions establish the contract they would need to satisfy.

Contract tests cover definition rejection, capabilities, event mappings, process filters, probes, argument boundaries, command sequencing, native-home resolution and all three transcript formats through both history readers. Existing tests cover canonical aliases, separate homes, partial records, bounded reads, missing counters and ambiguous launches. Viewer and dashboard tests cover empty Pi input, populated and multiline drafts, configurable return conditions, modified keys, return and re-entry with the same process. All tests use fixtures or fake commands and spend no model tokens.

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` before committing. Use the worktree's own `CARGO_TARGET_DIR` while another checkout is being built.
