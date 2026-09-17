# Harness definitions

Back to [harness behavior and sources](harness.md).

Each built-in harness has one definition under `assets/harnesses/`: `claude.yaml`, `codex.yaml` or `pi.yaml`. The binary embeds them, so editing one takes effect after rebuilding cones; no definitions or overrides are read from disk. The definitions describe discovery, reports, native operations and viewer behavior. User choices, including model and provider defaults, belong in `jobs.yaml`.

## The line between YAML and code

A definition supplies paths, mappings, conditions and native arguments. Registration selects the discovery, transcript, launch and resume handlers from `kind`; these are not editable YAML fields. Shared code owns OS parsing, directory walking, event matching, text extraction, caching and key dispatch. Native handlers own identity matching, Claude's registry-state precedence, usage accounting and supervised execution.

`src/harness/spec.rs` validates the definition and compiles the values its consumers use. Unknown fields, inconsistent capabilities and invalid command operands are errors. An operation needs a native adapter: `rename: true` is accepted only for Claude, and only the compiled Claude execution adapter can authorize a supervised run. `unknown` execution support means unverified.

Identity checks remain native code. Reported-thread handover requires a unique new thread matching the launch's prompt, cwd and start time, with no competing launch. YAML cannot substitute a title for identity or weaken native permission enforcement.

Native accounting passes reported costs through, or supplies disjoint request usage to the shared cost calculator. Provider/model prices live in a separate cached catalog, outside these definitions. Calculated dollars carry their source and coverage; see [cost estimates](harness.md#cost-estimates).

## Fields

| Field | Meaning |
| --- | --- |
| `version` | Definition schema version, currently `2`. |
| `kind` | Registered harness identity and executable name. |
| `icon`, `colour` | Dashboard mark and optional RGB color. An omitted color uses the dim style. |
| `home` | Native home environment variable, explicit default base and optional sibling-home discovery. |
| `discovery` | Registry location or process name, daemon pid and lock paths, and process subcommands that are not sessions. |
| `state` | Ordered event guards, discriminator paths and state mappings. Claude uses its native registry handler and omits this block. |
| `transcript` | Scan roots, message sources and an optional saved statusline source. `window_pointer` selects the reported context window; optional `cost_pointer` selects reported dollars. |
| `operations` | Launch, attach, resume, remove, unarchive, rename availability and session-kind behavior. |
| `execution` | Native enforcement and result-reporting support. Both default to `unknown`. |
| `viewer` | Label, speculative joins, retention, input alignment and optional native input overrides. |

Empty optional lists and mappings can be omitted. Each nested structure rejects unknown fields, including the removed `name` and handler selectors.

## Homes

`home.default` declares its base. `provided` uses the native root supplied by the caller. `provided_parent` joins `path` to that root's parent, preserving the existing Codex and pi defaults beside the Claude home. `user` joins `path` to the operating system's user home, independently of the supplied root:

```yaml
home:
  env: NATIVE_HOME
  default: {base: user, path: .local/share/native}
```

A nonempty native environment override takes precedence, including a relative one; an empty override keeps the default. A `provided` root is already resolved by its caller and remains authoritative. `home.siblings` can identify additional homes by a directory prefix and marker file.

A transcript root can declare its own `env`. This override names the complete directory, separately from the native configuration home. Pi uses `PI_CODING_AGENT_SESSION_DIR`; without it, pi uses the declared `sessions` root and its native cwd subdirectory. History and live discovery use the same resolved storage location.

History keys every entry by `(harness, canonical native home, session id)`. Aliases of one home collapse and separate homes stay distinct. A storage override does not replace the native home in that identity. Historical resume carries the recorded home into the native environment; pi also receives the recorded transcript path.

## State and messages

An event state rule names string-valued guards, a JSON pointer to the discriminator, mapped state words and an optional unknown-value result. Codex maps `task_complete` to `done` and lets unrelated events preserve the earlier state; pi maps assistant `stopReason` and reports `-` for a reason it does not know.

A message source names its event guards, boolean exclusion flags, content path and shape. Shared extraction understands strings and arrays of content blocks. The definition selects block types and attachment labels; an empty `types` list accepts text from any block type. `headline` defaults to `first`; pi selects `last` for assistant replies.

Claude and pi history-title fallbacks use the declared user source, including attachment labels. An image-only first instruction can supply `[image]`, and empty or excluded messages do not prevent a later instruction from supplying the title. Hydration uses the same selection. Native saved names keep their precedence.

Codex selects UI `UserMessage` records and its legacy `user_message` event, excluding injected model-history instructions. History previews retain complete selected text under shared control-character stripping, bounded reads and caching. Usage accounting and native title precedence remain in code.

## Commands

All native operations live under `operations`. Launch includes its identity handover, initial session kind, arguments, model/provider flag bindings and capability probe. The other command operations contain `args` and an optional `probe`:

```yaml
operations:
  attach:
    args: [attach, "{short_id}"]
  resume:
    args: [--bg, --resume, "{id}"]
  rename: true
  sessions:
    kinds:
      bg: {join: attach, stop: remove, lifetime: daemon}
```

An omitted launch or resume operation is unavailable. Discovery and history can be registered before those operations exist, and the composer cycles only through harnesses with a launch operation. `operations.sessions.default` covers missing or unknown native session kinds; it defaults to an own-terminal session with signal-based stop. `kinds` overrides that behavior for reported kinds. A join requires an attach operation, and background resume requires both resume and attach.

`stop: signal`, `remove` and `forget_client` describe different effects. They terminate a verified client, invoke native removal, or forget a thread's saved row and close its client respectively. Forgetting a Codex row does not terminate its daemon thread. Rename availability selects the existing native rename adapter; the definition supplies no file-writing program.

Command templates are argument arrays, and a placeholder occupies a whole argument. Accepted placeholders depend on the operation: `prompt`, `remote`, `cwd`, `id`, `short_id` and `transcript`. Unknown placeholders and missing operands are errors. Substitution preserves spaces, newlines and native path bytes and performs no shell expansion. Launch passes one prompt after `--`.

Launching, joining and resuming history remain separate operations. Native handlers sequence Claude's background resume then attach, and Codex's unarchive then resume. A failed first command prevents the second. Pi has no live attach and resumes history through `--session <transcript>`.

A probe declares its command, success requirement, required output strings, version format and diagnostic text. Launch requires a probe; attach, resume, remove and unarchive may each have one. A declared probe runs before that operation. `require_success` defaults to `true`; output without a declared text or JSON version is an error. `minimum_version` accepts a numeric major/minor version with an optional patch and requires a version-bearing probe. Codex launch requires `0.154.0` or later. This is separate from definition schema versioning and supervised execution validation.

`operations.launch.model` and `provider` name native flags. Their values come from the selected harness's configuration: Claude uses `defaults.model`, Codex uses `defaults.codex_model`, and pi uses `defaults.pi_model` and `defaults.pi_provider`. Unset values pass no flag. Claude's Bedrock environment and Codex's daemon provider remain native adapter behavior; a pi provider binding cannot be claimed for those adapters.

## Viewers and input

`viewer.peek` declares whether a row can be joined speculatively. Claude background sessions use `join`; Codex uses `existing_daemon`, which checks that its daemon is already running. The default is `unavailable`. Failed and stopped states are excluded by default, and speculative joins require an attach operation.

`viewer.retention` defaults to `retain`. Claude selects `evict_live` because its entered viewer can be rejoined; Codex retains entered viewers. Input alignment defaults to `bottom_rule`, with Codex selecting `fixed`. The viewer label defaults to the harness name; Claude uses `attach`. Pool sizes and historical-viewer retention remain dashboard invariants.

Return bindings default to Ctrl+Z unconditionally, and Tab and Left when the native editor is empty. Tab passes through for completion with text entered or when the editor is not recognized. A definition can replace that list under `viewer.input.return_to_list`:

```yaml
viewer:
  input:
    return_to_list:
      - {key: ctrl+z, when: always}
      - {key: tab, when: empty_prompt}
      - {key: left, when: empty_prompt}
```

Omitted keys pass through to the native client. Unknown keys, duplicate bindings and lists with no unconditional way back are rejected. Quit, layout switching and emulator scroll keys remain dashboard controls; Shift+Tab and modified arrows remain native keys. An opened viewer records its harness identity, so input behavior does not depend on its title or the selected row.

The default `marker` profile recognizes Claude and Codex's prompt markers before a visible terminal caret and ignores their braille spinner cells. Pi selects `bordered` and disables braille filtering. Its profile recognizes one empty row between horizontal borders with an inverse-video software caret at the terminal cursor. Text, multiline drafts, missing carets and non-editor screens keep Tab and Left in pi. A custom editor can still return through Ctrl+Z.

## Changing or adding a harness

Start with the reported behavior in [harness.md](harness.md). Change a definition when native arguments, capability requirements, source locations or input profiles change. Change the adapter when native protocol interpretation changes. Keep fixtures under `assets/harnesses/fixtures/` and behavior tests in `src/harness/spec/tests.rs` current together.

A new built-in needs a `HarnessKind`, a registration and native handler selection in `spec.rs`. Begin with its implemented discovery and transcript behavior, then declare operations as their adapters become available. Storage formats beyond the existing JSONL readers need native code; a YAML capability claim cannot create a reader or execution adapter.

Give a worktree its own `CARGO_TARGET_DIR` while another checkout is building. A shared target directory can serve another checkout's test binary, so a suite count alone does not establish which code ran.
