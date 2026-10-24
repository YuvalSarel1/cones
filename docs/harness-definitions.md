# Harness definitions

Back to [harness behavior and sources](harness.md).

Each built-in harness has a YAML definition under `assets/harnesses/`, embedded at build time.
Rebuild to change it; runtime definitions and overrides are not read. User settings belong in
`jobs.yaml`.

## The line between YAML and code

YAML supplies paths, event mappings, native arguments and viewer policy. Registration selects
native discovery, transcript, launch and resume handlers from `kind`. Shared code owns parsing,
caching and input dispatch; native handlers own identity, protocol interpretation and execution.

`src/harness/spec.rs` rejects unknown fields, inconsistent capabilities and invalid operands.
Capabilities require registered adapters: a declaration cannot add native support or weaken
permissions. Cost adapters supply native records to the shared [accounting component](harness.md#cost-estimates).

## Fields

| Field | Meaning |
| --- | --- |
| `version` | Schema version `2`. |
| `kind` | Registered harness identity and executable name. |
| `icon`, `colour` | Dashboard mark and RGB color; omitted color is dim. |
| `home` | Native environment variable, default base and optional sibling discovery. |
| `discovery` | Registry/process match, daemon/lock paths and excluded subcommands. |
| `state` | Ordered event guards, discriminator pointers and state mappings. Claude uses native code instead. |
| `transcript` | Scan roots, message sources and saved statusLine source. `window_pointer`, `cost_pointer`, `effort_pointer` and `dir_pointer` select reported values. |
| `operations` | Launch, attach, resume, fork, btw, stop, remove, unarchive, message, rename, compact and session-kind behavior. |
| `execution` | Native enforcement and result-reporting support; both default to `unknown`. |
| `viewer` | Label, peek, retention, alignment and native input overrides. |

Optional empty lists/maps can be omitted. Nested structures also reject unknown fields.

## Homes

```yaml
home:
  env: NATIVE_HOME
  default: {base: user, path: .local/share/native}
```

| `home.default.base` | Resolution |
| --- | --- |
| `provided` | Caller-supplied resolved native root, authoritative. |
| `provided_parent` | `path` under that root's parent. |
| `user` | `path` under the operating system's user home. |
| `xdg_data` | Application `path` under the environment value or `~/.local/share`. |

A nonempty native environment override takes precedence where applicable, including relative
paths; empty values keep the default. `home.siblings` finds extra homes by prefix and marker
file. A transcript root's own `env` replaces its complete storage directory, separately from
configuration home, as with pi's `PI_CODING_AGENT_SESSION_DIR`.

Live/history readers share storage resolution. Identity remains `(harness, canonical native
home, session id)` even with a storage override. Resume restores that home; XDG restores its
base directory, and pi also receives the recorded transcript path.

## State and messages

State rules contain string guards, a discriminator JSON pointer, mapped words and optional
unknown-value result. Native mappings are listed under [state](harness.md#state).

Message sources specify guards, exclusion flags, content path and shape. Extraction supports
strings and content-block arrays, with selected block types and attachment labels. Empty `types`
accepts all text blocks. `headline` defaults to `first`; pi uses `last` for replies. Declared user
sources also supply Claude/pi title fallbacks, preserving native saved-name precedence. Empty
or excluded messages do not suppress later titles; image-only prompts can supply `[image]`.

Codex selects UI/legacy user events to exclude injected instructions. Shared code handles
control-character stripping, bounds and caching; native handlers retain title and usage rules.

## Commands

```yaml
operations:
  attach:
    args: [attach, "{short_id}"]
  resume:
    args: [--bg, --resume, "{id}"]
  rename: true
  sessions:
    kinds:
      bg: {join: attach, remove: native, lifetime: daemon}
```

Omitted operations are unavailable. Launch declares arguments, identity handover, initial kind,
model/provider/effort bindings and a required probe. Other commands have `args` and optional
`probe`. `sessions.default` covers unknown kinds, defaulting to an own-terminal client with
no native remove; `kinds` overrides it. Joining requires attach; background resume also
requires resume. Native handlers sequence multi-step operations and stop on failure.

Stop and remove are separate declarations. `cones rm` stops a session, then removes it.

| Declaration | Effect |
| --- | --- |
| `operations.stop` | Native CLI stop of a daemon session, preserving conversation history. |
| Session `remove: none` | No list beyond the process; a terminal client is stopped by a verified signal. |
| Session `remove: native` | After the stop, `operations.remove`. |
| Session `remove: forget_client` | After the stop, forget/hide the row and close its client; retain the daemon thread. |

Owned-terminal stop uses the host separately. `operations.message` supplies native delivery,
as a command, for Claude only `peer_inbox: {protocols: [...]}`, or for pi and OpenCode
`reporter: <extension call>`; `unavailable: <reason>` states why a harness has none
([message delivery](harness.md#message-delivery)).
`rename: true` enables a registered native rename, currently Claude, Codex, pi and OpenCode
([how each is named](cli.md#renaming-a-session)); YAML supplies no file-writing program.
`compact: true` likewise enables a registered native compaction
([how each is compacted](cli.md#compacting-a-session)).

`operations.btw` is the headless, read-only fork a [side question](cli.md#asking-a-side-question)
runs. It takes `{id}` or `{transcript}`, `{text}` for the question, and optionally `{scratch}`,
a private directory, or `{answer}`, a file in it, both removed afterwards. Native handlers read
the answer and delete what the harness keeps of the fork, so it requires a native adapter.

Command arguments are arrays with whole-argument placeholders. Allowed names depend on the
operation: `prompt`, `remote`, `cwd`, `id`, `short_id`, `transcript`, fork `new_id`, message
and btw `text`, and btw `scratch` and `answer`. Unknown placeholders or missing operands fail. Substitution preserves bytes and performs
no shell expansion. Positional prompts follow `--`; see [other prompt forms](#terminal-only-definitions-and-forks).

Probes declare command, required output, version format and diagnostics. `output` defaults to
`stdout`; `stderr` supports CLIs such as OpenCode. `require_success` defaults to true. Declared
probes run before their operation. A missing declared text/JSON version is an error.
`minimum_version` accepts major/minor with optional patch and requires a version-bearing probe.
Native version requirements are separate from this YAML schema version.

Launch `model`, `provider` and `effort` bind native flags to [configured defaults](jobs.md#composer-harnesses).
Unset values pass no flag. `name` is the flag `cones launch --name` passes. `bedrock` names the native switch, such as Claude's
`CLAUDE_CODE_USE_BEDROCK`; omit it when the harness uses its own provider selection. Shared
`AWS_PROFILE`/`AWS_REGION` are passed to all harnesses independently of these bindings.
Codex daemon provider selection remains native adapter behavior.

## Viewers and input

| Field | Values/default |
| --- | --- |
| `viewer.peek` | `unavailable` by default; Claude `join`, Codex `existing_daemon`. Attach is required; failed/stopped rows are excluded by default. |
| `viewer.retention` | `retain` by default; Claude uses `evict_live`. |
| Input alignment | `bottom_rule` by default; Codex uses `fixed`. |
| Viewer label | Harness name by default; Claude uses `attach`. |

Pool sizes and historical retention are dashboard invariants. Native return keys default to:

```yaml
viewer:
  input:
    return_to_list:
      - {key: ctrl+z, when: always}
      - {key: tab, when: empty_prompt}
      - {key: left, when: empty_prompt}
```

A replacement list must retain an unconditional return key. Unknown/duplicate keys fail;
omitted keys pass through. Quit, layout and emulator controls stay in
[dashboard bindings](bindings.md). Input routing uses recorded harness identity.

| Input profile | Empty-editor recognition |
| --- | --- |
| `marker` | Claude/Codex prompt markers and visible caret; ignores braille spinners. |
| `bordered` | Pi's single empty row between borders with inverse-video caret; no braille filtering. |
| `opencode` | Standard editor border, padding, model row, block underline and initial caret. |

Drafts, multiline input, missing carets and unrecognized layouts keep editing keys native.
OpenCode keeps Tab native; Ctrl+Z always returns. See [viewer controls](dashboard.md#viewer).

## Changing or adding a harness

Change YAML for paths, mappings, native arguments or input policy; change native adapters for
protocol interpretation. New harnesses need a `HarnessKind`, registration and handler selection
in `spec.rs`. Keep fixtures in `assets/harnesses/fixtures/` and tests in
`src/harness/spec/tests.rs` current. Follow [native acceptance](testing.md#harness-acceptance)
before declaring support.

A full reader implements `cost::Adapter` and registers in `Native::accounting`, supplying
reported identities, disjoint usage and explicit gaps. `cost::Accounting` owns pricing,
deduplication and coverage; `cost::prefer_native` handles native session totals. Extend
`tests/cost.rs` for live/history fallback, native precedence, duplicates, gaps and catalog
arrival/change/expiry. Terminal-only adapters expose no accounting until a reader exists.

## Terminal-only definitions and forks

Terminal-only definitions are listed by their process, without state or accounting.
`home.env` may be empty when no native override is verified; never invent one. Most set
`transcript.available: false` with empty roots and message sources. One whose saved
conversations are verified declares roots and message sources instead, as Copilot and Cursor
Agent do, and `agents::meta` reads each file's id, folder and start; that adds history, search
and `cones show`, and nothing live beyond a reported `native_id`.

`discovery.aliases` must resolve to the canonical executable. `entrypoints` matches immediate
Node/Bun script operands by relative suffix, excluding names in prompt arguments.
`process_title` matches the complete replaced title, as for Kimi. `unsaved` lists options
that make a run keep no conversation, such as the fork `cones btw` starts; a process launched
with one before `--` is not listed.

A launch that omits `prompt`, `prompt_flag` and `stdin_prompt` has no interactive
instruction, and starting it with one is refused.
`operations.launch.prompt_flag` sends one `--flag=value` argument. Mutually exclusive
`stdin_prompt: true` uses an anonymous input file while stdout remains a terminal;
cancellation closes that file.

`operations.fork` requires native fork semantics and its own probe. Placeholders include
`id`, `transcript`, `remote` and pi's `new_id`, passed through native `--session-id`.
The UI saves parentage only after confirming the new native identity.
