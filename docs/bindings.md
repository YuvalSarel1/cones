# Dashboard bindings

The built-in shortcuts live in [`assets/bindings.yaml`](../assets/bindings.yaml).
The dashboard embeds this file at build time and uses it for key lookup and
the searchable Help screen. Rebuilding is required after changing it. There is
no user override file, `jobs.yaml` setting or binding editor yet.

## Format

The file has `version: 1` and a `states` list. Each state has:

| Field | Meaning |
| --- | --- |
| `id` | State selected by the dashboard's input router. |
| `title` | Section name in Help. |
| `when` | Explanation of when the state applies, shown in Help. |
| `extends` | Optional state whose bindings are inherited. |
| `bindings` | Keys, action names and explanations for this state. |
| `sections` | Optional Help sections grouping actions by selected row type or task. |

Each binding has a `keys` list, an `action` and a `description`. For example:

```yaml
- keys: ["ctrl+g"]
  action: guide
  description: "Open Help."
```

Aliases share one action and description. In a state with `extends`, a local
key replaces that inherited key; other inherited keys remain available.
Help shows list controls once, split into navigation, sessions, terminals,
folders, jobs, runs and the composer. Menu, history and folder-entry sections
show their specific controls and refer back to shared navigation.

A Help section has a `title`, a `when` explanation and `bindings` entries
referencing an `action` in the state's bindings. Each entry can override the
Help `description` for that context. Shortcut labels come from the referenced
action's keys, so remapping an action updates every section that uses it.
Sections only organize Help; they do not change input handling.

Keys use lowercase names: `up`, `down`, `left`, `right`, `home`, `end`,
`pageup`, `pagedown`, `enter`, `esc`, `tab`, `backspace`, `delete`, `space`
and `f1`, or a single character. Prefix modifiers with `ctrl+`, `alt+` or
`shift+`. `shift+tab` matches the terminal's BackTab event.
Shifted printable characters retain their character, such as `?`.
The `ctrl+\` binding also lists `ctrl+4`, because terminals can report the
same input that way. Unlisted modifier combinations do not match a binding.

Loading rejects unknown fields, states, actions and key names, unsupported
versions, repeated modifiers, duplicate states or keys within a state, missing
parent states and inheritance cycles. Keys and descriptions cannot be empty;
states and Help sections also need a title and condition. Section references
must name an action defined in that state. Action names refer to Rust handlers:
adding a new behavior still requires code.

## Input precedence

The YAML describes the bindings. Rust selects the active state, applies
conditions such as an empty draft or selected row type, and executes actions.
The `when` text documents these conditions; it is not an expression language.

1. A focused native viewer handles its native return keys first, then cones
   viewer shortcuts. Remaining input reaches the native client unchanged.
2. A session/copy menu or focused transcript handles its own bindings.
3. Pending-launch cancellation and panel return/layout shortcuts are checked
   before the active dashboard screen.
4. Each form selects its substate, such as a choice list, text edit, explanation
   or group buttons.
5. In the list, menu navigation and empty-prompt pane entry precede text editing.
   Folder completion and history search use their own input. Text editing gets
   first refusal before the remaining list shortcuts.

For example, `ctrl+e` moves to the end of nonempty composer text and edits a job
when the composer is empty. `shift+enter` inserts a line break in a draft and
opens a viewer fullscreen without a draft. The same action can have different
descriptions in different states.

The `text` state owns cones text-editing shortcuts. Unhandled printable
characters are inserted into cones prompts. Mouse actions and bracketed paste
are separate input paths.

Native return-to-list keys and their empty-prompt conditions remain in each
[`assets/harnesses/`](../assets/harnesses/) definition's `viewer.input.return_to_list`.
They depend on the harness's reported screen, so the dashboard binding file
does not replace them. Shell terminals keep native `ctrl+c`; agent viewers use
it for cones quit confirmation.

The zsh terminal integration also wraps native Left and Tab widgets to request
a return when its command line is empty. Those wrappers live in
`src/terminal.rs`; they preserve the original widgets for drafts and completion.
Other shells keep their native Left and Tab behavior.

## Maintaining bindings

Change the applicable state and action description together. Help is generated
from these records; compact footer hints and the prose in
[`dashboard.md`](dashboard.md) still need to be kept consistent when changing a
shortcut. Run `scripts/check` to validate parsing, shortcut behavior, guide
rendering and the rest of the repository.
