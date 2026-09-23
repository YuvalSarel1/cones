# The dashboard

[README](../README.md) · [Configuration and runs](jobs.md) · [Harness support](harness.md) · [Commands](cli.md)

`cones` shows native sessions and recent runs, including sessions started elsewhere. No jobs
file is needed. This guide covers controls; [configuration](jobs.md) owns field names,
defaults and accepted values.

## Navigation

The summary counts working, input, idle and done sessions, jobs and runs. `! stale` means a
read failed: previous rows remain visible with the error until a refresh succeeds.

`●` marks an unread completion. Viewing its painted live pane or focusing its loaded output
preview clears it. Merely selecting a row before its content loads does not. Input requests
remain until the harness reports resolution. Read markers are shared across dashboards and
survive restart; existing completions form the initial baseline. Turns that begin and end
between reads with identical final replies cannot be distinguished.

Use `ctrl+f` with `:attention` for input requests and unread completions, or `:unread` for
completions alone. Append text to narrow the results. Optional desktop notifications use
`start.notify`, default off; the focused session stays quiet. Job failure notifications have
a separate setting.

| Key | Action |
| --- | --- |
| `↑ ↓` | Select rows; up past the first table reaches the menu. |
| `enter` | Open the selection with an empty composer; submit an instruction with text. |
| `shift+enter` | Open the selection fullscreen once; insert a line break with text. Alt+Enter is also accepted. |
| `tab` | Complete a directory path in the composer; otherwise enter the pane. Native viewer behavior is below. |
| `→` | With an empty composer, enter the pane or open the session fullscreen if the pane is off. |
| `ctrl+z` | Return from a viewer or screen to the list. |
| `ctrl+\` | Toggle the pane; in a focused screen, switch split/fullscreen. Also `ctrl+4`. |
| `esc` | Cancel the current action or draft, then back out. Native viewers receive Escape. |
| `ctrl+c twice` | Quit within 1.5 seconds from the list or an agent viewer. Shell viewers keep Ctrl+C for interrupts. |
| `ctrl+o` | Open the selected harness's [launch settings](#launch-settings). |
| `ctrl+d` | Start the [coordinator](cli.md#coordinator) for the selected folder; refuse an existing live coordinator. |
| `ctrl+g` | Open Help. |

Click a row to select it; click its pane or press Enter/Tab to focus the viewer. The footer
shows actions for the current selection or draft, followed by navigation. Fullscreen viewers
have a bottom strip with the session title, fleet counts, input requests and return keys.

## Menu

The menu has `jobs`, `config` and `help`. Choose with Left/Right or a click. Screens use the
pane when enabled, otherwise the full frame. Menu and tab rows scroll to keep their selection
visible in narrow layouts. Column pickers open from Config.

### Jobs

Jobs appear in file order, followed by `new job`. Enter runs the selected job; `ctrl+e` edits
it with an empty composer. Removal stops a live run or deletes an idle job. The optional next
run column follows the configured schedule; it does not confirm that launchd loaded it.

Open `new job`, or submit a drafted instruction on the Jobs menu button. The wizard asks:

| Question | Answer |
| --- | --- |
| `what` | Task, seeded from the composer. |
| `where` | Directory, initially the selection's. Tab completes paths. |
| `when` | `once`, `hourly`, `daily`, `weekdays`, `weekly` or `cron`. |
| `at` | `09:00` for daily/weekdays, `mon 09:00` for weekly, or a five-field cron. Hourly uses `0 * * * *`. |
| `name` | Task slug until edited; follows the [job name rules](jobs.md#job-fields-and-defaults). |

Arrows select answers, Enter accepts, and Escape cancels. Backspace on an empty answer goes
back. Untouched answers keep their defaults. A `once` task uses the first job's policy or the
built-in policy and creates no job. Scheduled tasks expose overrides in `runs`; Backspace
restores inheritance and Enter on a setting saves.

Saving validates the file and updates only the chosen job block, preserving comments and
formatting. Save and delete [install schedules](jobs.md#schedules). Errors keep the form open.

### Columns

Config's column settings open a picker with Sessions, Runs, Jobs and History tabs. Each tab
remembers its cursor. Up past the first column reaches the tabs; Left/Right switches tabs.

| Key | Action |
| --- | --- |
| `space` | Show or hide the selected column. |
| `[` `]` | Move a shown column earlier or later. |
| `backspace` | Restore this table's defaults. |
| `←` or `esc` on a column | Return to Config. |
| `tab` or `ctrl+z` | Return to the dashboard list. |

Click a checkbox to toggle visibility or a row to select it. `[x]` shows visibility, the
number shows saved order, and `›` shows focus. Every change saves that table's setting and
updates the display. A failed save retains the previous selection. An explicit empty set
hides all optional columns; reset removes the override. See [column values](jobs.md#list-settings).

### Config

The editor has three tabs: `cones` for display settings, `harnesses` for availability and
shared AWS settings, and `runs` for run policy. Up past the fields reaches the tabs;
Left/Right or `[` `]` switches groups. Each group remembers its selected field.

A `*` marks configured values; inherited values are dim. Unset native choices say `harness
default`; unset AWS settings say `AWS default`. The fixed hint shows the default/reset value.
Press `?` or F1 for the full explanation; F1 also works while editing text.

| Control | Interaction |
| --- | --- |
| Choice | Left/Right or Space cycles and saves. Enter opens a list; confirm a choice to save, Escape to cancel. |
| Number | Arrows step values; validation enforces the allowed range. |
| Text | Enter edits, Enter saves, Escape restores. Empty values omit overrides. |
| Reset | Backspace restores the built-in value. |
| Columns | Enter, Right or Space opens the picker. |
| Folders | Open the pinned list; Enter edits/adds, `ctrl+x` removes a pin. |
| Connectivity | Enter, Right or Space checks installed CLIs and required launch flags without writing. |

Changes save immediately. Escape closes Config; Tab or Ctrl+Z returns to the list. Validation
errors identify the field; write failures retain the entered value for retry. An unreadable
configuration is reported and cannot be overwritten through this screen.

Saves preserve job blocks. They do not reinstall schedules, so changing captured environment
settings requires a subsequent job save. Model, effort and provider controls live in the
composer's [launch settings](#launch-settings).

### Help

Help opens a compact index for the list, history and viewers. Enter expands a heading;
Left/Right closes or opens it. It covers cones-specific actions; screen footers show navigation.

Type to search. `shift+tab` switches between words and meaning using the same local search
as [history](#history). Word search needs no model download. Matches open their headings;
Escape or Ctrl+U clears the query, and Escape on an empty query returns to the list. Ctrl+G
closes Help directly.

Bindings are embedded at build time. See [binding definitions](bindings.md) for the complete
key source and how to change it. There is no runtime binding editor or override file.

## Sessions and runs

Sessions group by directory or state, with input requests first in state grouping. Linked
Git worktrees share their repository heading and carry `⑂`; actions use the actual working
directory. Sessions sort oldest first by reported start, with unknown starts last. Forks
follow visible parents. Runs show the newest 200 visible records.

[Column settings](jobs.md#list-settings) control each table independently. Identity and row
icons stay visible. The default last reply column hides with the pane open; an explicit
selection keeps it. Columns grow to fit values during the session, and `whole_columns`
controls clipping in narrow lists. Times display in the local timezone.

| State | Display |
| --- | --- |
| `active` | Animated bar, working |
| `blocked` | Yellow bar, input |
| `idle` | Dim low bar, idle |
| `done` | Green check, done |
| `failed` | Red cross, failed |
| `stopped` | Dim low bar, stopped |
| Unknown | `-` |

Harness marks stay still. The coordinator has an orange `★` title, identified by its claim.
An external session without native attach support says `own terminal`.

| Key | Action |
| --- | --- |
| `ctrl+x twice` | Confirm stop or removal. |
| `ctrl+s` | Group by state or directory. |
| `ctrl+f` | Filter; Enter keeps it, Escape clears it. |
| `ctrl+h` | Show or hide history. |
| `ctrl+n` | Rename a Claude or Codex session. |
| `ctrl+p` | Toggle a temporary session highlight. |
| `ctrl+y` | [Fork a conversation](#fork-a-conversation). |
| `ctrl+t` | Pin a session above every group, or unpin it. |
| `ctrl+l` | Open [MCP servers](#mcp-servers). |
| `ctrl+r` | Refresh now. |

Rename accepts a manual name or an empty submission for native automatic naming. Codex's
suggestion is accepted once ready. A key, paste or mouse action cancels a pending handoff.
An external interactive Claude client supports manual transcript naming only and may later
overwrite it.

The first `ctrl+x` arms the action; another key cancels it. Expiry uses `confirm_secs`.
The row disappears while the action runs and returns if it fails.

| Selection | Confirmed action |
| --- | --- |
| Live session | Its [native stop or removal](harness.md#native-actions). |
| Settled Claude background | Remove the job record; retain the transcript. |
| Running run or job | Stop the run. |
| Finished run | Hide the run and its owned session; retain output and ledger. Restore through the [hidden file](jobs.md#stored-files). |
| Idle job | Delete it and reinstall schedules. |
| Empty pinned folder | Remove its pin; retain the directory. |

Run previews read the archived/native conversation, falling back to captured events and
stderr. They show messages and compact tool calls, excluding thinking and tool output. Live
output refreshes once per second and follows new text until scrolled back. Enter joins an
available native session, follows a running run's output if unjoinable, or revives a settled
run. A resumed run retains its row and original ledger outcome while displaying live values.

### Add a folder

`+ add folder` is the last session-list row. Type an existing path there; `~` expands and
relative paths use the dashboard's cwd. Tab completes directories; a second Tab lists matches.
Hidden names require a `.` prefix. Enter pins the folder and selects it; invalid paths retain
the input. Escape clears it.

The row offers pinned folders not already represented in the list, filtered by path fragment.
It does not build suggestions from past sessions. Down selects an offer, Enter accepts it,
and Tab copies it into the input. Aliases are deduplicated; deleted directories are rejected
when chosen. Pins share the `folders` configuration with Config. A pin becomes an empty
folder row whenever its sessions leave.

### Fork a conversation

`ctrl+y` forks a live or historical Claude, Codex, pi or OpenCode conversation through the
native harness. It creates a new identity in the same folder, sends no instruction and
preserves the composer draft. Claude forks use persistent interactive terminals. Forks do
not create worktrees or transfer conversations between harnesses.

The new row appears beneath its visible parent, with indentation confined to the title.
`STATE_DIR/forks.json` records confirmed links. Hidden parents leave a branch marker; absent
parents leave an ordinary row. Missing transcripts, archived sources and unsupported CLIs
are reported before launch.

### MCP servers

`ctrl+l` reads native configuration for the selected session, or the composer harness and
folder when no session is selected. Configured servers are shown without claiming a running
session loaded them.

| Harness/scope | Configuration source |
| --- | --- |
| Claude user | `mcpServers` in `.claude.json` under the native home. |
| Claude project | `.mcp.json` in the project folder. |
| Claude local | The folder's entry in the native home's `.claude.json`. |
| Codex | `mcp_servers` in `config.toml` under `CODEX_HOME`. |

Other harnesses report unverified support. Missing files and parse errors are shown explicitly.

| Key | Action |
| --- | --- |
| `x` | Stage or undo removal. |
| `c` | Copy to a compatible scope; arrows choose it and Enter stages it. |
| `s` | Save staged changes. |
| `u` | Discard staged changes. |
| `esc` | Close and discard unsaved changes. |

The prompt lists pending changes. Saves preserve unrelated settings, file modes and TOML
comments; copies precede removals. Failed writes retain staged changes for retry. Saving
never restarts a session; native clients pick up configuration when they next start.

### History

`ctrl+h` shows conversations below the live list, newest activity first. Live sessions and
identified Claude run conversations are excluded. Pages contain 50 rows; moving beyond a
page loads more. [Historical sources](harness.md#historical-sessions) define inclusion.

Typing or pasting with history selected searches titles, folders, harnesses, IDs and visible
conversation text beyond loaded pages. Matching conversations appear once, with excerpts
where needed. Selecting a result previews its passage; Enter resumes. In the explicit
Ctrl+F field, Enter keeps the filter. Escape clears the query, then hides history.

`shift+tab` switches search modes:

- **Words:** every query word must appear in one passage. Common English filler is dropped,
  except when the whole query is filler. Titles and other metadata are also searched.
- **Meaning:** local MiniLM matches passages and marks results `≈`. The first use downloads
  about 90 MB. Conversation text and queries stay on the machine. English works best;
  similarity below 0.5 is discarded. Model failures are reported; Ctrl+R retries.

Ctrl+R refreshes history; reopening it also refreshes. During meaning search it can fill the
embedding index for every conversation and reports progress; a second Ctrl+R cancels that
work. Filling stops while history is hidden; [`cones index`](cli.md#building-the-meaning-index)
finishes it from a terminal. The rebuildable cache is under `STATE_DIR/search/`. Browsing without a query loads no
model, and live session polling does not rescan history.

Previews show chronological messages, Markdown replies and compact tool calls, excluding
thinking and tool output. The initial view starts at the latest message or search match.
Scrolling loads more in bounded pages; oversized messages mark omitted text. Native themes,
extensions and interactive widgets are not reproduced. Live pi/OpenCode rows without owned
viewers, and settled sessions without clients to join, use the same read-only presentation.
Live previews refresh once per second.

In a focused preview, arrows, page keys and the wheel scroll. Home/End reaches the loaded
beginning/latest text. Tab, Escape or Ctrl+Z returns; Ctrl+R rereads at the bottom. `c` copies
the last response, one of its fenced code blocks, or session details. Incomplete blocks are
marked and copied text excludes terminal escapes.

Right from a Claude or Codex history preview opens the context inspector. Browse categories,
entries and text with Right; Left returns. It distinguishes recorded content, named sources
and files merely present on disk, including mismatches with recorded copies. Unreported
token counts stay unknown. It is unavailable for live rows, runs and other harnesses.

Enter resumes with the recorded native home and folder while preserving the composer draft.
History offers no delete action or recent-session switcher. Browsing previews and context
starts no native client.

## Viewer

A focused viewer stays in the pane; otherwise it follows the selection. Rows never display
another session's output. Folder and job rows may retain the last focused viewer. Resting on
a joinable session prepares its client; a session needing revival waits for explicit entry.
Saved Codex threads also wait for entry if their daemon is gone.

Quitting or crashing the dashboard leaves hosted shells, pi, OpenCode, experimental launchers
and interactive Claude forks running. Reopen with the same state directory and Enter
reconnects to the process and its draft. One dashboard may attach to a host at a time.
`ctrl+x` twice stops it. Host crashes and machine restarts end these processes; conversation
history remains resumable where the harness provides it. Claude background sessions and
Codex daemon threads keep native ownership, while their attach clients close and draft
behavior remains native.

| Native viewer input | Behavior |
| --- | --- |
| `ctrl+z` | Return to the list, including with a draft. |
| `tab` / `←` | Return from a recognized empty native editor; otherwise remain native. Exceptions below. |
| `ctrl+c` | Quit confirmation in agent viewers; interrupt foreground commands in shell viewers. Use Escape to interrupt an agent turn. |
| `ctrl+\` | Switch split/fullscreen. |
| Other keys | Pass through, including Escape, Shift+Tab and modified arrows. Some terminal encodings lose modified-key distinctions. |
| Shift+PageUp/PageDown | Scroll emulator history. |
| Left drag | Select and copy pane text while cones owns mouse input. |
| Wheel | Scroll the pane under the pointer; native mouse clients receive events, otherwise emulator history scrolls. Shift+wheel always scrolls the emulator. |
| Paste | Preserve native bracketed paste. Empty paste events become Ctrl+V for image paste. |

OpenCode returns with Left from its standard empty editor but keeps Tab native. Experimental
launchers keep both keys native. Zsh returns with Tab/Left from an empty command line;
continuations and foreground programs keep them. Other shells keep both native.

Clicks in scrolled emulator history stay with the emulator. Typing returns to the live
screen. A fullscreen client without mouse reporting gives mouse control to the terminal.
Use the terminal's selection modifier or `alt+m` to select across the pane boundary;
Alt+M toggles mouse capture. See [diagnostics](cli.md#diagnostics) for delays.

## Composer

Type an instruction and Enter to launch in the selected folder. With no folder selection,
the dashboard's cwd is used. Submitting on Jobs opens its wizard. Shift+Tab cycles through
[visible enabled harnesses](jobs.md#composer-harnesses), then the terminal. If every launcher
is hidden, the terminal remains available.

For `terminal`, Enter opens an interactive shell or runs the drafted command in a new one.
The shell comes from executable `$SHELL`, then the account shell, then `/bin/sh`. With an empty
command field, Enter reconnects to a selected owned terminal. Commands and agent instructions
keep separate drafts. Escape clears a command, then returns to the default harness.

Launch preparation selects a temporary row and keeps the list usable until the viewer is
ready. Discovery fills in native identity without reclaiming selection after you move away.
Escape or Ctrl+Z cancels pending preparation. Failure or cancellation restores the instruction
unless you have typed new text. [Identity limits](harness.md#composer-identity) vary by harness.

### Launch settings

Ctrl+O opens model, effort and provider controls available for the selected harness, with the
composer draft intact. It is unavailable where the composer names no harness. Amp and Droid
have no settings here.

Changes immediately write `defaults` in `jobs.yaml`; closing the picker keeps them. Scheduled
jobs use applicable new defaults on their next run, while running sessions retain their launch
settings. There is no separate set of interactive launch defaults. Controls match Config;
Escape, Tab or Ctrl+O closes the picker.

### Text and images

The composer wraps up to eight rows. Standard cursor, word movement and deletion keys edit
text; Shift+Enter inserts a newline. Ctrl+E edits a selected job only when the composer is
empty. See [the binding source](../assets/bindings.yaml) for all aliases.

Ctrl+V pastes clipboard images through macOS `osascript`. Temporary PNGs appear as
`[Image #n]` markers, delete as one unit and expand to their paths at launch. Text pastes
insert at the cursor.
