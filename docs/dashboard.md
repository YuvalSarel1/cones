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
| `tab` | Enter the pane from the list. Return when an agent's empty prompt is recognized or zsh reports an empty command line; otherwise pass through for completion. Other shells and forms keep Tab for their own input. |
| `→` | With an empty composer, go to the agent: into the pane when it is open, over the whole frame when it is closed. On the menu row it picks a button. |
| `ctrl+z` | Return from a viewer or menu screen; the viewer keeps running. |
| `ctrl+\` | Toggle the pane from the list; switch split/full frame from a focused viewer or menu screen. Also recognized as ctrl+4. |
| `esc` | Back out one step: armed action, typed instruction, jobs screen, dashboard. Forms cancel an edit or close. Inside a native viewer it goes to the client. |
| `ctrl+c twice` | Quit within a 1.5-second confirmation window, from the list or an agent viewer. In a terminal, Ctrl+C interrupts commands. |
| `ctrl+g` | Open help. |

Clicking a list row selects it and takes focus. `enter`, `tab` or a pane click gives the selected viewer focus. Split viewers retain their dimensions across focus changes. A full-frame viewer has a bottom strip with its title, fleet counts, other input requests and return keys.

## Menu

The menu has `folder`, `jobs`, `config`, `columns` and `help`. Once there, use `← →` or click to choose a button. It opens in the pane when enabled, otherwise over the full frame, at any terminal size.

Menu and section buttons shift horizontally to keep the selected button visible in narrow panes. Clicks follow the visible buttons; gaps do not select anything. An oversized selected label is clipped to the available space.

### Folder

Enter an existing directory. `~` expands and relative paths use the dashboard's cwd. `tab` completes directory names; a second tab lists remaining matches. Hidden names need a `.` prefix. `↑ ↓` recall previously seen folders. The list sits above the prompt, newest first, and the keys move through its rows: either key starts at the newest, `↓` goes down toward older and `↑` back up, both wrapping.

A newly pinned empty folder becomes the selection. Choosing a folder already containing sessions keeps the current selection. `ctrl+p` also pins the selected row's folder, or the dashboard cwd from the menu.

| File under the state directory | Contents |
| --- | --- |
| `folders` | Pinned paths, one per line. Sessions replace the empty-folder placeholder while present; it returns when they leave. |
| `recent` | The last 20 seen folders, newest first, used by folder recall. |

### Jobs

The jobs screen lists jobs in file order, then `new job`. Its `job_columns` picker controls status, schedule, next run, model, last run and folder, with harness names optional. Enabled and harness icons and the job name always show. Next run follows the enabled job's configured calendar intervals. `enter` starts a job; `ctrl+e` edits it with an empty composer; confirmed removal stops its running run or deletes the idle job.

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

### Columns

`columns` opens a picker for Sessions, Runs, Jobs and History, starting on the table you came from. Its table tabs use the same button row as Config: `↑` past the first column reaches the buttons, `← →` switch tables and wrap around, and `↓`, `enter` or space returns to the columns. Each table remembers its selected column. Config's `columns` link opens the same picker; `esc` returns to that link. The picker uses the existing pane or the full frame, like the other menu screens.

| Key | Action |
| --- | --- |
| `↑ ↓` | Select a column; `↑` past the first reaches the table buttons. Movement stops at the last column. |
| `← →` on the table buttons | Switch tables and wrap around. |
| `←` on a column | Return to the dashboard list, including when opened from Config. `→` does nothing on a column. |
| `space` | Show or hide the selected column without moving its row. |
| `[` `]` | Move a shown column earlier or later; the cursor follows it. |
| `backspace` | Restore the current table's defaults. |
| `home`, `end`, `page up`, `page down` | Navigate longer lists and short panes. |
| `esc` | Return to Config when opened there, otherwise close the picker. |
| `tab`, `ctrl+z` | Return to the dashboard list. |

The `›` marker and shaded row show keyboard focus; `[x]` and `[ ]` show visibility. The order number records a shown column's position in its saved set. State/status and the harness name retain their places beside the row's identity, as their descriptions explain. `defaults` or `custom` identifies where the selection comes from, independently of visibility.

Click a row to select it, its checkbox to toggle it, or a table name to focus its button. The wheel moves through columns. The selected row stays visible when the pane is short. A focused tab row stays visible even in a one-line pane; returning to the columns restores their space.

Every change validates and saves only that table's column setting in `jobs.yaml`, then updates the dashboard. Other settings and job blocks are preserved. A failed save keeps the previous selection and explains the error. An explicit empty selection hides every optional column; reset removes that override so defaults apply again.

### Config

The editor has three tabs: `cones` for dashboard settings, `harnesses` for model/provider settings, and `runs` for shared run policy. The tabs are a button row like the [menu](#menu): `↑` past the first field lands on them, `← →` pick a tab and wrap around, and `↓` or `enter` returns to the fields. The visible `[` `]` shortcut switches groups while browsing settings, and clicking a tab also lands on the row. Each tab remembers its selected field. Bold subheadings, indentation and blank rows separate config blocks and harnesses. Bedrock and AWS settings are under `claude`.

Each setting occupies one row with its current value. A `*` marks values set in config; inherited values are dim. Boolean controls show `on` and `off`, and `full access` describes the existing `codex_full_access` setting. These labels do not change the stored keys or boolean values. An unset harness-owned setting says `harness default`; `aws_profile` and `aws_region` say `AWS default`, since an unset one is resolved by AWS from the shell and `~/.aws/config`, not by the harness.

Only the focused field has the `›` marker. Moving to the tabs removes the field's focus styling without losing its selection. Focused tabs remain visible in short panes, and returning to the fields restores their space.

A fixed two-line hint stays below the list and names the default or reset value. `?` or `F1` opens the full explanation for the selected setting; arrows, the wheel and page keys scroll it, and `esc` returns to the same setting. `F1` also works during text editing. Results and errors can use more hint space.

`↑ ↓` select a field; `← →` change a choice or step a number, validating and saving immediately. `backspace` restores the built-in value. `home`, `end`, `page up` and `page down` navigate within the current tab. The wheel moves through fields, and the selected row stays visible in short panes.

| Control | Interaction |
| --- | --- |
| Choices | Arrows or space cycle directly. `enter` or clicking the value opens a vertical list. `↑ ↓` browse without changing the setting; `enter`, space or clicking a choice saves it. `(*)` marks the current value. `esc` or `←` returns without changing it. |
| Choices or text | Model, AWS region and bucket pickers include a custom value option. Typing on the setting also opens text editing. Arrows cycle through the choices and the custom slot. |
| Numbers | Confirmation time and bar count step by 1, timeout by 5 minutes. Steps stay on their grid and never go below zero; field validation may reject zero. Unset nonnumeric values step from zero. |
| Text | `enter` or clicking the value starts editing; `enter` accepts and `esc` restores the previous value. Long values scroll within the row to keep the cursor visible. Environment names use commas. Empty values omit overrides. |
| Columns | `enter`, `→` or space opens the [column picker](#columns). |
| Connectivity | `enter`, `→` or space runs the launch probe for every harness and answers on the explanation line: the binary on cones's own launch PATH, and whether the installed version takes the flags a dashboard session needs. It writes nothing, and the next key clears the answer. |

Clicking a field's label only selects it. Saving keeps focus on that field. `esc` closes Config; `tab` or `ctrl+z` returns to the dashboard list. Leaving keeps saved changes. Validation errors focus the relevant field and leave the file untouched; a failed file write restores the preceding value and shows the error.

Config saves replace only `defaults`, `columns`, `run_columns`, `job_columns`, `history_columns`, `whole_columns`, `activity`, `pane`, `start` and `confirm_secs`, preserving job blocks. A missing file is created with `jobs: []`. Config saves do not reinstall schedules; [captured environment changes](jobs.md#environment) require a job save afterward.

### Help

`help` and `ctrl+g` open the built-in guide. The search prompt accepts typing and pasted text immediately; `/` and `ctrl+f` also start search. Each search word must match the shortcut, description or section name, regardless of case. For example, `config reset` finds reset in the Config section. The guide shows the match count and explains how to recover from an empty result.

`↑ ↓` and the wheel scroll by rendered lines. `page up` and `page down` scroll a page; `home` and `end` reach the first and last results. Shortcuts stack above their descriptions in narrow panes. Left and right edit a nonempty search, and `enter` keeps its results visible. `esc` or `ctrl+u` clears the search; with an empty search, `esc` or `←` returns to the list. `ctrl+g` closes Help directly.

## Sessions and runs

Live sessions group by directory, sorted without case, or by state with input requests first. Within a group they sort by reported start, oldest first; unknown starts sort last, then by id. Pinned empty folders show their git branch and tree state. Runs show the newest 200 visible records. Their independent `run_columns` setting controls harness name, status, start and end times, duration, context, model, tokens, cost, reason, directory, trigger and last reply. Start and end times use your local timezone.

Session rows have an activity icon, harness mark, title and [configured columns](jobs.md#list-settings). Run rows always retain their status icon, harness mark and job name. The [column picker](#columns) edits `columns`, `run_columns`, `job_columns` and `history_columns` independently. Harness names are hidden by default while their icons remain. Agent folders appear as a column when grouping by state and as headings in the normal view. The default last reply column hides while the preview pane is open; an explicitly selected last reply stays visible. Headers are dim and unselectable; filtering hides them. Column widths grow during the dashboard session so changing values do not move adjacent cells. Narrow lists follow `whole_columns`.

Selecting a run previews its captured output, including tool activity and stderr, beneath its recorded status and reason. The preview refreshes once per second while visible and follows new output until you scroll back. It never resumes the run. `tab` or a pane click focuses the read-only preview; `enter` follows a running run's log or opens a finished run through the harness. Older runs with only an archived transcript use that file instead. Runs without captured files say so.

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
| `ctrl+f` | Filter; `enter` keeps the filter, `esc` clears it, `←` leaves it while empty. |
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

History appears below the live list, ordered by latest recorded activity regardless of grouping. It excludes live sessions and identified Claude ledger sessions. Its independent `history_columns` default to last active, folder, model, context and last reply. Live state and activity chart columns are not offered. [Historical sources](harness.md#historical-sessions) define which conversations appear.

Pages contain 50 rows. Arrows, page-up/down and the wheel over the list load older entries without wrapping. With the cursor in history, typing or pasting edits the filter directly, including while results load or no rows match. Backspace and the usual text editing keys edit the search; `esc` clears it, or hides history when empty. Filtering searches beyond loaded pages. `ctrl+r`, or hiding and reopening history, refreshes the snapshot; live polling does not rescan it.

Typing in history searches session titles, folders, harnesses, IDs and conversation text. `ctrl+f` also opens the search field. Keyword and semantic matches share one list, ordered by relevance. Each conversation appears once, with a matching excerpt; yellow words match the query and `≈` marks a result found by meaning. Selecting a result opens its matching passage; `enter` resumes it. In the explicit `ctrl+f` field, `enter` keeps the search and returns to the list.

Semantic search uses MiniLM locally. The first search downloads the model, about 90 MB, and builds passage embeddings in the background. Keyword results remain usable while semantic results arrive. Conversation text and queries stay on the machine. If the model cannot load, the list says semantic search is unavailable; `ctrl+r` retries. The model works best with English; keyword search also handles other languages.

The rebuildable search cache lives under `STATE_DIR/search/`. Text comes from visible user and assistant messages, excluding thinking, tool output and harness control records. Changed transcripts are reindexed on refresh; unchanged passage embeddings are reused. SQLite stores the text index and vectors, and model files are cached alongside it. Opening history without a query does not load or download the model.

Selecting a row loads its conversation in the pane after a short cursor rest. Messages appear in chronological order, with the latest message at the bottom. Claude and Codex use their prompt and response markers; pi uses shaded prompt blocks and unmarked replies. Replies render Markdown with highlighted code. Recorded tool calls appear as compact names and inputs; tool outputs and thinking stay out of the conversation. The pane says `history · read only`. Scrolling up loads earlier messages in bounded pages and keeps the text you were reading in place. Large individual messages remain bounded; omitted text is marked.

These are read-only presentations based on each harness's default appearance. Native extensions, custom themes and interactive tool widgets are not reproduced.

The wheel scrolls the preview without changing the selection. When focused, arrows and page-up/down scroll; home/end reach the loaded beginning or latest text. Reaching the beginning requests an older page when available. `tab`, `esc` or `ctrl+z` returns to the list. Typing and pasting are ignored. Focused `ctrl+r` rereads the preview at the bottom; `ctrl+\` changes its layout.

A search preview starts at the matching passage. Scrolling past either end loads more conversation in that direction. Large individual messages show a bounded excerpt around the match, with omitted text marked.

`enter` resumes the selected conversation, including from its preview. An existing composer draft is preserved while searching and resuming history. Its native viewer replaces the preview and is reused when the session appears live. History has no delete action. Browsing it starts no harness client.

## Viewer

The pane shows the focused viewer, otherwise the selected session's viewer. Sessions, runs and historical rows never show another row's output. A live session stays blank while its client starts. Runs and historical rows use their read-only previews until explicitly opened; menu rows preview their button. Folder and job rows can retain the last focused viewer.

Resting on a joinable live row can prepare its viewer before entry. This never starts a new agent: finished runs require an explicit open, and a saved Codex thread waits for entry if its daemon has exited. A speculative viewer says `attach` until entered, then `return`. A preview may be closed to make room for another; entered Codex clients, resumed runs and historical viewers are retained. A Claude client left in its own agents list closes so that list cannot appear under a session's name.

All viewers close with the dashboard. Whether that also ends the session depends on [native ownership](harness.md#native-actions); composer pi and OpenCode sessions end with their viewers. Stopping or removing a row is a separate action.

| Input in a native viewer | Behavior |
| --- | --- |
| `tab` | Return when the harness's empty prompt is recognized or zsh reports an empty command line; otherwise pass through for completion. Other shells always keep Tab. |
| `ctrl+z`, `ctrl+\` | Dashboard navigation, as above. Ctrl+Z returns even with an unfinished draft. |
| `←` | Return when the harness's standard empty editor is recognized or zsh reports an empty command line; populated or multiline input keeps the arrow. Modified arrows stay native. |
| `ctrl+c` | Dashboard quit confirmation in an agent viewer. These clients interpret two presses as quit, and Claude's first press leaves the conversation for its agents list. Use `esc` to interrupt a turn. In a terminal, Ctrl+C interrupts the shell's foreground command. |
| Other keys, including `esc` and `shift+tab` | Pass to the native client. Classic terminal encoding means some modified keys, including shift+enter, cannot be distinguished there. |
| Shift-page-up/down | Scroll emulator history. |
| Wheel | Scroll the viewer under the pointer, even without focus. Clients requesting mouse events receive them; otherwise emulator history scrolls. Shift-wheel always uses emulator history. |
| Text paste | Preserve bracketed paste when the client requests it. An empty paste, used for images by VS Code, becomes the client's ctrl+v. |

OpenCode returns with Left when its standard session editor is empty. Drafts, multiline input and native menus keep Left; Tab stays native. Ctrl+Z always returns to the list.

Typing returns to the live screen. Terminal text selection may need the terminal's modifier, such as option-drag in iTerm2. Split viewers use the dashboard's hint line so the harness retains its own bottom status row. For delays, see [diagnostics](cli.md#diagnostics).

## Composer

Type an instruction and press `enter` to start a native session in the selected row's directory. From the menu or without a selected directory, it uses the dashboard's cwd. On `jobs` or `new job`, submitting an instruction opens the job wizard instead. `shift+tab` cycles Claude Code, Codex, pi, OpenCode and terminal; the prefix names the selection. A harness turned off by [`<harness>_enabled`](jobs.md#job-fields-and-defaults) is skipped, including as the harness the dashboard comes up on, and its [discovery](harness.md#discovery) stops too; the terminal stays reachable with every harness off. Identified agent rows show their reported model.

On `terminal`, type or paste a command and press `enter` to run it in a new interactive shell in that directory and focus its pane. An empty command opens the shell at its prompt. The command field supports the composer's editing keys and `shift+enter` for a new line. The prefix shows the detected shell, such as `terminal (zsh)`. cones uses an executable `$SHELL`, then the account's configured shell, then `/bin/sh`. In zsh, Left or Tab returns to the list when the command line is empty. With a command typed, Left edits and Tab completes using your existing bindings. Continuation lines and foreground programs keep both keys. Other shells keep their native Left and Tab bindings. Ctrl+C interrupts commands, and Ctrl+Z always returns to the list.

From the list, Tab returns to the selected terminal in either split or full-screen layout; Enter with `terminal` selected opens another one. Shell rows survive dashboard refreshes and end when the shell exits, you close their viewer with Ctrl+X twice, or the dashboard closes. Terminal commands and agent instructions keep separate drafts when switching with `shift+tab`. Escape clears a drafted command; with the command empty, it returns to the default harness.

The instruction wraps to at most eight text rows. In a side-by-side pane it aligns with Claude or pi's input box when that box is near the bottom; Codex and log viewers keep the composer at its normal position. Scrolling history does not move it.

A launch immediately selects a row containing the harness, directory and first instruction line, while preparation runs in the background. List focus stays available until the viewer is ready. Native discovery fills in reported details and replaces the launch identity without changing the viewer or taking selection back after you move away. Unrelated arrivals do not take selection while typing or viewing a session. `esc` cancels pending preparation; a cancellation or failure removes the launch row and restores its instruction unless you have typed new text.

Model and provider choices come from [policy defaults](jobs.md#job-fields-and-defaults). Native sessions retain their harness permissions; timeouts and tool restrictions belong to supervised runs. [Launch identity](harness.md#composer-identity) explains attribution limits.

### Text and images

Keys without another action type into the composer. Text prompts share these editing controls:

| Keys | Edit |
| --- | --- |
| `← →` | Move one character. With nothing typed, `←` leaves the jobs screen, the folder prompt and the filter, since there is nothing to its left. |
| `alt+← alt+→`, `ctrl+← ctrl+→`, `alt+b alt+f` | Move one word. |
| `home end`, `ctrl+a ctrl+e` | Move to the line's ends. With an empty composer, `ctrl+e` edits a selected job. |
| `backspace delete` | Delete one character. |
| `alt+backspace`, `ctrl+w` | Delete the preceding word. |
| `alt+d` | Delete the following word. |
| `ctrl+u ctrl+k` | Delete before or after the cursor. |
| `ctrl+v` | Paste a clipboard image. |

Words are runs of non-space characters. macOS terminals commonly translate command/option shortcuts into these control/alt keys. Text pastes insert at the cursor. Images use macOS `osascript`, are saved as temporary PNGs and appear as `[Image #n]` markers; each marker deletes as one character and expands to its path at launch.
