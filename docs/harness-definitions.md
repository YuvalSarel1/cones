# Harness definitions

Back to [harness behavior and sources](harness.md).

Each built-in harness has one definition under `assets/harnesses/`: `claude.yaml`, `codex.yaml` or `pi.yaml`. The binary embeds them, so editing one takes effect after rebuilding cones; no definitions or overrides are read from disk, and `jobs.yaml` is unchanged. Between them they cover discovery, commands, live reports, history and viewer input.

## The line between YAML and code

A definition supplies names, paths, mappings, conditions and strategy choices. Shared code owns OS parsing, directory walking, event matching, text extraction, caching and key dispatch, none of which varies by harness: the `ps` arguments, column splitting, UTC timestamp format and unreadable-table errors all live in `fleet.rs`. A typed handler owns whatever needs the native protocol interpreted, which is why Claude's registry-state precedence, native identity matching, usage accounting and supervised execution are named mechanisms rather than fields. `src/harness/spec.rs` reads the files into typed structures, rejects unknown fields and incompatible handler combinations, and exposes one registry in composer cycle order.

Two consequences hold throughout, so the sections below do not restate them. A definition cannot weaken a check the code owns: `launch.identity`'s reported-thread handover keeps its requirement for a unique new thread matching the launch's prompt, cwd and start time with no competing launch, and nothing in YAML can substitute a title for identity. And a definition cannot claim a capability cones has no adapter for: declaring `execution` support for Codex or pi is rejected, because `Harness::compile`, result handling, permission validation and timeout supervision are code. `unknown` there means unverified, not unsupported.

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

## Homes

`home.sibling: null` means the caller already supplied the Claude root. Other defaults replace the final component of that root: `.codex` or `.pi/agent`. A nonempty native environment override takes precedence, including a relative one; an empty override keeps the default.

History canonicalizes these homes and keys every entry by `(harness, canonical native home, session id)`. Aliases of one home collapse, separate homes stay distinct, and transcript copies under project directories within one home collapse by identity. Joining a Codex row uses the home its rollout belongs to, and historical resume carries the entry's saved home into the native environment.

## State and messages

An event state rule names string-valued guards, a JSON pointer to the discriminator, mapped state words and an optional unknown-value result. Codex maps `task_complete` to `done` and lets unrelated events preserve the earlier state; pi maps assistant `stopReason` and reports `-` for a reason it does not know.

A message source names its event guards, boolean exclusion flags, content path and shape. Shared extraction understands strings and arrays of content blocks, and the definition selects block types and attachment labels. Codex selects the UI `UserMessage` records and its legacy `user_message` event while excluding injected model-history instructions, and takes its assistant headline from the first text block where pi takes the last. The history preview keeps the complete selected text under shared control-character stripping, bounded reads and caching; history and live viewers stay separate UI modes.

## Commands

Command templates are argument arrays, and a placeholder occupies a whole argument:

```yaml
resume: [--remote, "{remote}", resume, --, "{id}"]
```

Which placeholders are accepted depends on the command: `prompt`, `remote`, `cwd`, `id`, `short_id` and `transcript`. An unknown placeholder or a missing required operand is an error. Substitution keeps spaces, newlines and native path bytes inside one argument, and performs no shell expansion.

Launching, joining a live session and resuming history are separate operations. A typed handler sequences the pairs a native client needs, Claude's background resume then attach and Codex's unarchive then resume, passing command values as positional shell arguments; a failed first command prevents the second. pi has no live attach but resumes history through `--session <transcript>`.

## Viewers and input

`viewer.peek` says whether a row can be joined speculatively. Claude background sessions can; Codex's `existing_daemon` first checks that its daemon is running, since an explicit history resume may start one; pi's `unavailable` is why a composer pi returns to the viewer cones already owns. `viewer.retention: retain` keeps an entered Codex viewer even though a speculative one can be reopened. The shared pool manager applies the policy, and its pool sizes and the retention of resumed runs and historical viewers are dashboard invariants rather than fields.

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

The keys are `ctrl+z`, `tab` and `left`; the conditions are `always` and `empty_prompt`. Removing a binding passes that key through to the native client, and a conditional Tab lets a harness keep Tab while a draft is populated. Unknown keys, duplicate bindings and definitions with no unconditional way back are rejected. Quit, layout switching and emulator scroll keys stay dashboard controls, and Shift+Tab and modified arrows stay native keys. The defaults return on Tab and Ctrl+Z from any focused live viewer and keep the native process running; Left returns only when the harness's `empty_prompt` profile matches the rendered screen. An opened viewer records its harness identity, so input behavior never depends on a title or on which row discovery last selected.

The `marker` profile recognizes Claude and Codex's prompt marker before a visible terminal caret. The `bordered` profile recognizes pi's standard editor: one empty row between horizontal borders with an inverse-video software caret at the terminal cursor position, since pi normally hides that cursor and has no prompt marker. Text anywhere in the editor row, a multiline draft, a missing software caret or a non-editor screen all keep Left in pi. This follows the installed pi 0.85.1 `Editor.render` and `MainScreenTUI.positionHardwareCursor`; a custom editor that does not match can still return through Tab or Ctrl+Z.

## Changing or adding a harness

Start with the reported behavior in [harness.md](harness.md). Change a definition when a command, capability, source location or input profile changes; change its typed handler when the native protocol needs different interpretation. Keep the native report fixtures under `assets/harnesses/fixtures/` and the expected behavior in `src/harness/spec/tests.rs` current together.

A new built-in also needs a `HarnessKind` and a registration in `spec.rs`. Reuse an existing typed strategy where one fits and add a handler for a new protocol; the native-handler validation names the three supported integrations explicitly, so extending it takes evidence and tests for the fourth. User-installed packages are future work, and these embedded definitions are the contract such a package would have to satisfy.

Contract tests cover definition rejection, capabilities, event mappings, process filters, probes, argument boundaries, command sequencing, native-home resolution and all three transcript formats through both history readers, along with canonical aliases, separate homes, partial records, bounded reads, missing counters and ambiguous launches. Viewer and dashboard tests cover empty pi input, populated and multiline drafts, configurable return conditions, modified keys, and return and re-entry with the same process.

Give a worktree its own `CARGO_TARGET_DIR` while another checkout is building. A shared target directory can serve a test binary built from that other checkout, so a suite count you read here can come from code you are not looking at.
