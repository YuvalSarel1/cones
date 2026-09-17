# The dashboard

[README](../README.md) · [Configuration and runs](jobs.md) · [Harness support](harness.md) · [Commands](cli.md)

`cones` shows sessions and recent runs, including sessions it did not start. No jobs file is needed. This guide follows the screen: navigation, menu, session list, viewer and composer. The [configuration reference](jobs.md) owns field names, defaults and accepted values.

## Navigation

The summary counts working, input, idle and done sessions, jobs and runs. `! stale` means a read failed: the previous counts and rows stay visible, with the reason in the result line, until a read succeeds. Background reads start one second after the previous read completes. Manual actions discard obsolete reads, so an older snapshot cannot undo a stop or removal.

| Key | Action |
| --- | --- |
| `↑ ↓` | Select rows; up past the first table reaches the menu. |
| `enter` | With an empty composer, act on the selection: open a session or finished run, follow a running run, start a job, or open a menu screen. With text, submit the instruction. |
| `shift+enter` | Open the selection over the whole frame once; returning restores the prior layout. With text, insert a line break. Terminals reporting alt+enter use the same action. |
| `tab` | Move focus between the list and pane. Forms keep tab for their own input. |
| `ctrl+z` | Return from a viewer or menu screen; the viewer keeps running. |
| `ctrl+\` | Toggle the pane from the list; switch split/full frame from a focused viewer or menu screen. Also recognized as ctrl+4. |
| `esc` | Back out one step: armed action, typed instruction, jobs screen, dashboard. Forms cancel an edit or close. Inside a native viewer it goes to the client. |
| `ctrl+c twice` | Quit within a 1.5-second confirmation window, from the list or focused viewer. |
| `ctrl+g` | Open help. |

Clicking a list row selects it and takes focus. `enter`, `tab` or a pane click gives the selected viewer focus. Split viewers retain their dimensions across focus changes. A full-frame viewer has a bottom strip with its title, fleet counts, other input requests and return keys.

## Menu

The menu has `folder`, `jobs`, `config` and `help`. Once there, use `← →` or click to choose a button. It opens in the pane when enabled, otherwise over the full frame, at any terminal size.

### Folder

Enter an existing directory. `~` expands and relative paths use the dashboard's cwd. `tab` completes directory names; a second tab lists remaining matches. Hidden names need a `.` prefix. `↑ ↓` recalls previously seen folders, newest first.

A newly pinned empty folder becomes the selection. Choosing a folder already containing sessions keeps the current selection. `ctrl+p` also pins the selected row's folder, or the dashboard cwd from the menu.

| File under the state directory | Contents |
| --- | --- |
| `folders` | Pinned paths, one per line. Sessions replace the empty-folder placeholder while present; it returns when they leave. |
| `recent` | The last 20 seen folders, newest first, used by folder recall. |

### Jobs

The jobs screen lists jobs in file order, then `new job`. Its fixed columns show enabled state, harness, last run status, schedule, name, model, age and directory. `enter` starts a job; `ctrl+e` edits it with an empty composer; confirmed removal stops its running run or deletes the idle job.

Open `new job`, or press `enter` on the menu's `jobs` button with an instruction typed. The wizard accepts answers in any order: `↑ ↓` select rows, `enter` accepts and advances, `← →` pick schedules, backspace on an empty answer goes back, and `esc` cancels. Untouched answers use the displayed defaults.

| Question | Answer |
| --- | --- |
| `what` | Task, seeded from the composer. |
| `where` | Directory, initially the selected row's. Tab completes paths. Resolution follows [`cwd`](jobs.md#job-fields-and-defaults). |
| `when` | `once`, `hourly`, `daily`, `weekdays`, `weekly` or `cron`. |
| `at` | Defaults to `09:00` for daily/weekdays, `mon 09:00` for weekly, or a five-field cron expression. Hourly uses `0 * * * *` and skips this row. |
| `name` | Task slug until edited; follows the [job name rules](jobs.md#job-fields-and-defaults). |

A `once` task uses the first job's policy, or built-in policy when no template is available, and writes no job. For scheduled jobs, the `runs` section below the answers exposes job overrides. It starts open when the job has overrides; `enter` or `→` opens it and `←` closes it. Each row names the key it writes, with the inherited value in its hint. It uses the [config controls](#config); backspace restores inheritance, and `enter` on a setting saves the job.

Save validates the whole file and changes only the selected job block, preserving surrounding formatting and comments. Invalid values keep the form open on the relevant row. Save and delete then [install schedules](jobs.md#schedules); errors appear in the result line.

### Config

The editor has three groups: `cones` for dashboard settings, `harnesses` for model/provider settings, and `runs` for shared run policy. Subheadings name actual config blocks or harnesses. `runs` starts collapsed: `enter` or `→` opens it and `←` closes it.

Each row shows its control and value. `↑ ↓` select a field; `← →` change a choice or step a number, validating and saving immediately. `backspace` restores the built-in value. `enter` advances, except on text or number fields where it first opens editing and a second press accepts. Escape restores the previous value. Leaving keeps already saved changes; validation errors focus the relevant field and leave the file untouched.

| Control | Interaction |
| --- | --- |
| Choices | Arrows cycle; an initial letter selects a matching option. An unset field brackets its effective built-in, such as `[claude]`; `default` is used when the harness chooses. |
| Choices or text | Model, AWS region and bucket rows have an empty slot after the choices. Arrows reach it or typing fills it; returning to a listed choice drops custom text. |
| Numbers | Confirmation time and bar count step by 1, timeout by 5 minutes. Steps stay on their grid and never go below zero; field validation may reject zero. Unset nonnumeric values step from zero. |
| Text | Type a value; environment names use commas. Empty values omit overrides. |
| Columns | Visible columns precede a separator and hidden columns. Arrows select, space shows/hides, `[` `]` reorder visible columns, and backspace restores defaults. The live table updates after each save. |

Config saves replace only `defaults`, `columns`, `whole_columns`, `activity`, `pane`, `start` and `confirm_secs`, preserving job blocks. A missing file is created with `jobs: []`. Config saves do not reinstall schedules; [captured environment changes](jobs.md#environment) require a job save afterward.

### Help

`help` and `ctrl+g` open the built-in guide. `↑ ↓` scroll; `esc`, `enter` or `ctrl+g` close it.

## Sessions and runs

Live sessions group by directory, sorted without case, or by state with input requests first. Within a group they sort by reported start, oldest first; unknown starts sort last, then by id. Pinned empty folders show their git branch and tree state. Runs show the newest 200 visible records, with job, status, fired time, duration, cost and reason.

Session rows have an activity icon, harness mark, title and [configured columns](jobs.md#list-settings). Headers are dim and unselectable; filtering hides them. Column widths grow during the dashboard session so changing values do not move adjacent cells. Narrow lists follow `whole_columns`.

| State | Icon | Label | Color |
| --- | --- | --- | --- |
| `active` | Animated `▁▂▃▄▅▆▇` and back | working | plain |
| `blocked` | `▇` | input | yellow |
| `idle` | `▁` | idle | dim |
| `done` | `✓` | done | green |
| `failed` | `✗` | failed | red |
| `stopped` | `▁` | stopped | dim |
| `-` | `–` | `-` | dim |

Harness marks and the mascot stay still. A coordinator has an orange title prefixed with `★`; its identity comes from the [skill's status record](harness.md#coordinator-identity). Sessions that need their original terminal say `own terminal` in the footer.

| Key | List action |
| --- | --- |
| `ctrl+x twice` | Confirm the selected row's removal or stop. |
| `ctrl+p` | Pin its folder. |
| `ctrl+s` | Group by state or directory. |
| `ctrl+f` | Filter; `enter` keeps the filter, `esc` clears it. |
| `ctrl+h` | Show or hide history. With an empty composer, opening selects the first history row when loaded. |
| `ctrl+n` | Rename a Claude session. The [native title record](harness.md#reports) is updated; a live client may overwrite it from memory. |
| `ctrl+r` | Reload now. |

The first `ctrl+x` marks the row red; another key cancels it. Expiry follows [`confirm_secs`](jobs.md#list-settings). Confirmed deletions disappear before the native command finishes and return if it fails. A successful action clears the hint; the disappearing row is its confirmation.

| Selected row | Confirmed action |
| --- | --- |
| Live session | The [native stop or removal](harness.md#native-actions) for its kind. |
| Run in flight, or job with a live run | Stop the run. |
| Finished run | Hide the row; retain its ledger, output and transcript. Restore through the [`hidden` file](jobs.md#stored-files). |
| Job with no live run | Delete the job and reinstall schedules. |
| Pinned empty folder | Remove its pin; leave the directory intact. |

### History

History appears below the live list, ordered by latest recorded activity regardless of grouping. It excludes live sessions and identified Claude ledger sessions. It uses the configured session columns, with state and activity `-`, and `last` showing the last recorded reply. [Historical sources](harness.md#historical-sessions) define which conversations appear.

Pages contain 50 rows. Arrows, page-up/down and the wheel over the list load older entries without wrapping. Filtering searches beyond loaded pages. `ctrl+r`, or hiding and reopening history, refreshes the snapshot; live polling does not rescan it.

Selecting a row loads its recent conversation in the pane after a short cursor rest. The pane says `transcript · read only` and starts at the latest text. The wheel scrolls it without changing the selection. When focused, arrows, page-up/down and home/end scroll; `tab`, `esc` or `ctrl+z` returns to the list. Typing and pasting are ignored. Focused `ctrl+r` rereads the preview; `ctrl+\` changes its layout.

With an empty composer, `enter` resumes the selected conversation, including from its preview. Its native viewer replaces the preview and is reused when the session appears live. History has no delete action. Browsing it starts no harness client.

## Viewer

The pane shows the focused viewer, otherwise the selected session's viewer. A live session never shows another session's output or a transcript substitute while its client starts. Other rows can retain the last focused viewer; menu rows preview their button, and historical rows use the read-only preview above.

Resting on a joinable live row can prepare its viewer before entry. This never starts a new agent: finished runs require an explicit open, and a saved Codex thread waits for entry if its daemon has exited. A speculative viewer says `attach` until entered, then `return`. A preview may be closed to make room for another; entered Codex clients, resumed runs and historical viewers are retained. A Claude client left in its own agents list closes so that list cannot appear under a session's name.

All viewers close with the dashboard. Whether that also ends the session depends on [native ownership](harness.md#native-actions); a composer pi ends with its viewer. Stopping or removing a row is a separate action.

| Input in a native viewer | Behavior |
| --- | --- |
| `tab`, `ctrl+z`, `ctrl+\` | Dashboard navigation, as above. |
| `←` | Return when the harness's standard empty editor is recognized; populated or multiline input keeps the arrow. Modified arrows stay native. |
| `ctrl+c` | Dashboard quit confirmation. It never reaches the client: these clients interpret two presses as quit, and Claude's first press leaves the conversation for its agents list. Use `esc` to interrupt a turn. |
| Other keys, including `esc` and `shift+tab` | Pass to the native client. Classic terminal encoding means some modified keys, including shift+enter, cannot be distinguished there. |
| Shift-page-up/down | Scroll emulator history. |
| Wheel | Scroll the viewer under the pointer, even without focus. Clients requesting mouse events receive them; otherwise emulator history scrolls. Shift-wheel always uses emulator history. |
| Text paste | Preserve bracketed paste when the client requests it. An empty paste, used for images by VS Code, becomes the client's ctrl+v. |

Typing returns to the live screen. Terminal text selection may need the terminal's modifier, such as option-drag in iTerm2. Split viewers use the dashboard's hint line so the harness retains its own bottom status row. For delays, see [diagnostics](cli.md#diagnostics).

## Composer

Type an instruction and press `enter` to start a native session in the selected row's directory. From the menu or without a selected directory, it uses the dashboard's cwd. On `jobs` or `new job`, submitting opens the job wizard instead. `shift+tab` cycles Claude Code, Codex and pi; the prefix names the selected harness, and each started row reports its model in the list.

The instruction wraps to at most eight text rows. In a side-by-side pane it aligns with Claude or pi's input box when that box is near the bottom; Codex and log viewers keep the composer at its normal position. Scrolling history does not move it.

A launch immediately selects a row containing the harness, directory and first instruction line, while preparation runs in the background. List focus stays available until the viewer is ready. Native discovery fills in reported details and replaces the launch identity without changing the viewer or taking selection back after you move away. Unrelated arrivals do not take selection while typing or viewing a session. `esc` cancels pending preparation; a cancellation or failure removes the launch row and restores its instruction unless you have typed new text.

Model and provider choices come from [policy defaults](jobs.md#job-fields-and-defaults). Native sessions retain their harness permissions; timeouts and tool restrictions belong to supervised runs. [Launch identity](harness.md#composer-identity) explains attribution limits.

### Text and images

Keys without another action type into the composer. Text prompts share these editing controls:

| Keys | Edit |
| --- | --- |
| `← →` | Move one character. |
| `alt+← alt+→`, `ctrl+← ctrl+→`, `alt+b alt+f` | Move one word. |
| `home end`, `ctrl+a ctrl+e` | Move to the line's ends. With an empty composer, `ctrl+e` edits a selected job. |
| `backspace delete` | Delete one character. |
| `alt+backspace`, `ctrl+w` | Delete the preceding word. |
| `alt+d` | Delete the following word. |
| `ctrl+u ctrl+k` | Delete before or after the cursor. |
| `ctrl+v` | Paste a clipboard image. |

Words are runs of non-space characters. macOS terminals commonly translate command/option shortcuts into these control/alt keys. Text pastes insert at the cursor. Images use macOS `osascript`, are saved as temporary PNGs and appear as `[Image #n]` markers; each marker deletes as one character and expands to its path at launch.
