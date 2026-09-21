# The dashboard

[README](../README.md) · [Configuration and runs](jobs.md) · [Harness support](harness.md) · [Commands](cli.md)

`cones` shows sessions and recent runs, including sessions it did not start. No jobs file is needed. This guide follows the screen: navigation, menu, session list, viewer and composer. The [configuration reference](jobs.md) owns field names, defaults and accepted values.

## Navigation

The summary counts working, input, idle and done sessions, jobs and runs. `! stale` means a read failed: the previous counts and rows stay visible, with the reason in the result line, until a read succeeds. Background reads start one second after the previous read completes. Manual actions discard obsolete reads, so an older snapshot cannot undo a stop or removal.

A separate `●` marks a newly observed completion you have not reviewed. The summary counts unread completions without changing native state. Entering its painted live viewer or focusing its loaded output preview clears the marker; selecting or hovering a row does not. Input requests remain until the harness reports that they are resolved.

Press `ctrl+f` and type `:attention` for input requests and unread completions, or `:unread` for completions alone. Add a space and text to narrow either filter. Existing completed sessions form the first baseline instead of appearing as an unread backlog. Read markers are shared by dashboards using the same state directory and survive restart. Cones observes native state transitions and changed final replies; it cannot count turns that begin and end between reads with an identical final reply.

Desktop notifications are optional, off by default. Set `start.notify: true` in Config's `cones` / `start` subsection. Newly reported input requests and completions can notify; the focused session stays quiet. This setting is separate from scheduled jobs' failure notifications.

| Key | Action |
| --- | --- |
| `↑ ↓` | Select rows; up past the first table reaches the menu. |
| `enter` | With an empty composer, act on the selection: open a session or finished run, follow a running run, start a job, or open a menu screen. With text, submit the instruction. |
| `shift+enter` | Open the selection over the whole frame once; returning restores the prior layout. With text, insert a line break. Terminals reporting alt+enter use the same action. |
| `tab` | Complete the directory path being typed in the composer, the one under the cursor when the instruction holds several. With nothing to complete, enter the pane from the list. Return when an agent's empty prompt is recognized or zsh reports an empty command line; otherwise pass through for completion. Other shells and forms keep Tab for their own input. |
| `→` | With an empty composer, go to the agent: into the pane when it is open, over the whole frame when it is closed. On the menu row it picks a button. |
| `ctrl+z` | Return from a viewer or menu screen; the viewer keeps running. |
| `ctrl+\` | Toggle the pane from the list; switch split/full frame from a focused viewer or menu screen. Also recognized as ctrl+4. |
| `esc` | Back out one step: armed action, typed instruction, jobs screen, dashboard. Forms cancel an edit or close. Inside a native viewer it goes to the client. |
| `ctrl+c twice` | Quit within a 1.5-second confirmation window, from the list or an agent viewer. In a terminal, Ctrl+C interrupts commands. |
| `ctrl+o` | Open the [launch settings](#launch-settings) of the harness the composer names. |
| `ctrl+g` | Open help. |

`ctrl+b` lists the sessions and runs entered from this dashboard, most recent first and without the one you are on, so the first row is the session to go back to. Pressing `ctrl+b` again enters it; the arrows choose another row and `enter` takes it. Moving through the list only highlights rows: nothing is entered until you choose it. Only entering a row records it, so resting on a row, a read-only preview and a speculative viewer leave the order alone. A session that has closed is no longer listed and is never resumed to satisfy the list. Switching sessions keeps the composer draft and every open viewer's own state.

Clicking a list row selects it and takes focus. `enter`, `tab` or a pane click gives the selected viewer focus. Split viewers retain their dimensions across focus changes. A full-frame viewer has a bottom strip with its title, fleet counts, other input requests and return keys.

Quitting cones detaches from its hosted terminals: shells, pi, OpenCode, interactive Claude forks and experimental launchers started here. Reopen cones with the same state directory and press Enter on the row to reconnect to the same process, including its native editor draft. `ctrl+x` twice explicitly stops that terminal. Claude background sessions and Codex daemon threads retain their native ownership; their attach clients are not retained by the new host, and their draft behavior remains native.

Each owned terminal has a detached host and permits one attached dashboard at a time. A second dashboard reports that it is already open; it does not steal input. Closing the first dashboard releases the connection. Processes survive dashboard crashes, but not a host crash or machine restart. Native conversation history can still be resumed afterward; arbitrary shell processes are not automatically restarted. Cones never adopts or stops unrelated external terminals on exit.

The footer has two labeled lines: actions for the current selection or draft,
then navigation and `ctrl+g help`. Session actions appear while browsing;
launch settings and harness selection appear while drafting new work. Narrow
panes drop secondary hints. The composer starts a new agent in the selected folder.

## Menu

The menu has `jobs`, `config` and `help`. Once there, use `← →` or click to choose a button. It opens in the pane when enabled, otherwise over the full frame, at any terminal size. Column pickers open from Config.

Menu and section buttons shift horizontally to keep the selected button visible in narrow panes. Clicks follow the visible buttons; gaps do not select anything. An oversized selected label is clipped to the available space.

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

Config's column settings open a picker for Sessions, Runs, Jobs and History, starting on the selected setting's table. Its table tabs use the same button row as Config: `↑` past the first column reaches the buttons, `← →` switch tables and wrap around, and `↓`, `enter` or space returns to the columns. Each table remembers its selected column. `esc` returns to the Config setting. The picker uses the existing pane or the full frame, like the other menu screens.

| Key | Action |
| --- | --- |
| `↑ ↓` | Select a column; `↑` past the first reaches the table buttons. Movement stops at the last column. |
| `← →` on the table buttons | Switch tables and wrap around. |
| `←` on a column | Return to the column setting in Config. `→` does nothing on a column. |
| `space` | Show or hide the selected column without moving its row. |
| `[` `]` | Move a shown column earlier or later; the cursor follows it. |
| `backspace` | Restore the current table's defaults. |
| `home`, `end`, `page up`, `page down` | Navigate longer lists and short panes. |
| `esc` | Return to the column setting in Config. |
| `tab`, `ctrl+z` | Return to the dashboard list. |

The `›` marker and shaded row show keyboard focus; `[x]` and `[ ]` show visibility. The order number records a shown column's position in its saved set. State/status and the harness name retain their places beside the row's identity, as their descriptions explain. `defaults` or `custom` identifies where the selection comes from, independently of visibility.

Click a row to select it, its checkbox to toggle it, or a table name to focus its button. The wheel moves through columns. The selected row stays visible when the pane is short. A focused tab row stays visible even in a one-line pane; returning to the columns restores their space.

Every change validates and saves only that table's column setting in `jobs.yaml`, then updates the dashboard. Other settings and job blocks are preserved. A failed save keeps the previous selection and explains the error. An explicit empty selection hides every optional column; reset removes that override so defaults apply again.

### Config

The editor has three tabs: `cones` for dashboard settings, `harnesses` for which harnesses the composer offers and the AWS settings every harness shares, and `runs` for shared run policy. The tabs are a button row like the [menu](#menu): `↑` past the first field lands on them, `← →` pick a tab and wrap around, and `↓` or `enter` returns to the fields. The visible `[` `]` shortcut switches groups while browsing settings, and clicking a tab also lands on the row. Each tab remembers its selected field. Bold subheadings, indentation and blank rows separate config blocks. The `harnesses` tab holds the connectivity check, the Bedrock switch with its AWS profile and region, then one on/off row per harness. A harness's own model, effort and provider are not here: [`ctrl+o`](#launch-settings) sets them beside the composer, where the harness is already selected.

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

Clicking a field's label only selects it. Saving keeps focus on that field. `esc` closes Config; `tab` or `ctrl+z` returns to the dashboard list. Leaving keeps saved changes. Validation errors focus the relevant field and leave the file untouched; a failed file write keeps the value that was entered and shows the error, so the same change can be made again once the cause is fixed. A `jobs.yaml` this build cannot read, such as one holding a column or a version a newer cones wrote, is reported on the hint row instead of being shown as the built-in defaults, and a write onto it is refused rather than replacing the settings it holds.

Config saves replace only `defaults`, `columns`, `run_columns`, `job_columns`, `history_columns`, `whole_columns`, `activity`, `pane`, `start` and `confirm_secs`, preserving job blocks. A missing file is created with `jobs: []`. Config saves do not reinstall schedules; [captured environment changes](jobs.md#environment) require a job save afterward.

### Help

`help` and `ctrl+g` open the built-in guide. The search prompt accepts typing and pasted text immediately; `/` and `ctrl+f` also start search. Each search word must match the shortcut, description or section name, regardless of case. For example, `config reset` finds reset in the Config section. The guide shows the match count and explains how to recover from an empty result.

The guide separates shared navigation, session rows, terminal rows, folder
rows, job rows, run rows, history and the composer. Each section says when its
controls apply, including what Enter and stop do for that selection.

Shortcuts come from the same [YAML definitions](bindings.md) used for input
handling. Help explains how to edit `assets/bindings.yaml` and rebuild and
restart cones. There is no user override file or in-app binding editor yet.

`↑ ↓` and the wheel scroll by rendered lines. `page up` and `page down` scroll a page; `home` and `end` reach the first and last results. Shortcuts stack above their descriptions in narrow panes. Left and right edit a nonempty search, and `enter` keeps its results visible. `esc` or `ctrl+u` clears the search; with an empty search, `esc` or `←` returns to the list. `ctrl+g` closes Help directly.

## Sessions and runs

Live sessions group by directory, sorted without case, or by state with input requests first. Within a group they sort by reported start, oldest first; unknown starts sort last, then by id. Forks follow their visible parents, with indentation confined to the title column. The list's last row is always [`+ add folder`](#add-a-folder). Runs show the newest 200 visible records. Their independent `run_columns` setting controls harness name, status, start and end times, duration, context, model, tokens, cost, reason, directory, trigger and last reply. Start and end times use your local timezone.

Session rows have an activity icon, harness mark, title and [configured columns](jobs.md#list-settings). Run rows always retain their status icon, harness mark and job name. The [column picker](#columns) edits `columns`, `run_columns`, `job_columns` and `history_columns` independently. Harness names are hidden by default while their icons remain. Agent folders appear as a column when grouping by state and as headings in the normal view. A folder that is a linked Git worktree, its own checkout of a repository held elsewhere, carries `⑂`. The mark is read per folder, so a history row shows it only for a folder the live list already resolved. The default last reply column hides while the preview pane is open; an explicitly selected last reply stays visible. Headers are dim and unselectable; filtering hides them. Column widths grow during the dashboard session so changing values do not move adjacent cells. Narrow lists follow `whole_columns`.

Selecting a run previews its captured output, including tool activity and stderr, beneath its recorded status and reason. The preview refreshes once per second while visible and follows new output until you scroll back. It never resumes the run. `tab` or a pane click focuses the read-only preview; `enter` joins the run's own session whenever that session is still up, falls back to a running run's log when it cannot be joined, and opens a settled run through the harness. Older runs with only an archived transcript use that file instead. Runs without captured files say so. A resumed run keeps its place in the run list: the session the harness reports for it belongs to that row, not to a new agent. Reviving a history row is the opposite and does put a real agent in the live list.

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
| `ctrl+s` | Group by state or directory. |
| `ctrl+f` | Filter; `enter` keeps the filter, `esc` clears it, `←` leaves it while empty. |
| `ctrl+h` | Show or hide history. With an empty composer, opening selects the first history row when loaded. |
| `shift+tab` | With the cursor in history, switch its search between words and meaning. |
| `ctrl+n` | Rename a Claude session. The [native title record](harness.md#reports) is updated; a live client may overwrite it from memory. |
| `ctrl+y` | [Fork a supported conversation](#fork-a-conversation). |
| `ctrl+t` | Inspect and change the [MCP servers](#mcp-servers) of the selected session's harness. |
| `ctrl+b` | List the sessions entered from here, most recent first; press it again to enter the highlighted one. |
| `ctrl+r` | Reload now. |

The first `ctrl+x` marks the row red; another key cancels it. Expiry follows [`confirm_secs`](jobs.md#list-settings). Confirmed deletions disappear before the native command finishes and return if it fails. A successful action clears the hint; the disappearing row is its confirmation.

| Selected row | Confirmed action |
| --- | --- |
| Live session | The [native stop or removal](harness.md#native-actions) for its kind. |
| Settled Claude background session | `claude rm`; the row goes with the job record, the transcript stays. |
| Run in flight, or job with a live run | Stop the run. |
| Finished run | Hide the row, and the session it owns with it, so neither returns as an agent row; retain its ledger, output and transcript. Restore through the [`hidden` file](jobs.md#stored-files). |
| Job with no live run | Delete the job and reinstall schedules. |
| Pinned empty folder | Remove its pin; leave the directory intact. |

### Add a folder

`+ add folder` is the last row of the session list, always there. Select it and type: the row takes a path instead of an instruction, so the composer stays empty. Enter an existing directory. `~` expands and relative paths use the dashboard's cwd. `tab` completes directory names; a second tab lists remaining matches. An empty input has nothing to complete, so `tab` enters the pane there as it does elsewhere in the list. Hidden names need a `.` prefix. `enter` pins the folder, `esc` clears what is typed, and a missing directory is reported without losing the text.

While the row or one of its offers holds the cursor, folders are offered under it: the pinned ones and the directories this read already saw sessions in, most recently used first, with a linked worktree followed by the repository it belongs to. Each offer says why it is there: `pinned`, `worktree`, `repository` or `recent`. Typing filters them by any fragment of the path, `↓` moves onto one, `enter` pins it, `tab` puts it in the input to edit, and `esc` returns to the input. Aliases of one folder, such as a symlinked path, are offered once. The offers come from the rows this read already holds, so opening them reads no transcripts; a folder that has since been deleted is still offered and reported as missing when it is picked.

The pinned folder is dropped in among the other folders in sorted order and takes the cursor, so the next instruction starts there. Adding a folder that already has sessions keeps the folder's existing rows.

| File under the state directory | Contents |
| --- | --- |
| `folders` | Pinned paths, one per line. Sessions replace the empty-folder placeholder while present; it returns when they leave. |

### Fork a conversation

Select a live session or a history entry and press `ctrl+y`. Claude Code, Codex, pi and OpenCode use their native fork operation to create a separate conversation. The source stays unchanged and no instruction is sent automatically. Claude forks open a persistent interactive terminal; regular Claude launches remain background sessions. The composer draft stays intact. A fork uses the same project directory; it does not create a Git worktree or isolate file edits. A missing transcript, unsupported CLI or archived source is reported before launching.

In the folder view a fork appears beneath its visible parent. Only its title is indented, using `↳`; the state, harness and configurable columns share the same alignment as every other row. Nested forks receive another indent. If the parent is hidden or in another state group, the fork remains visible with a branch mark; once the parent is gone the mark goes with it and the fork reads as an ordinary row. Relationships created through cones are saved in `STATE_DIR/forks.json` after the new native identity is known.

### MCP servers

`ctrl+t` opens the MCP panel for the selected session's harness and folder. With no session selected it uses the harness the composer names and the folder a launch would use. The panel reads native configuration files and nothing else: it starts no harness client, sends no prompt and writes nothing until you save.

cones offers only the scopes it has verified for a harness. Claude Code has three: `user`, the `mcpServers` table of `.claude.json` under the native home, which reaches every project; `project`, `.mcp.json` in the folder, which is checked in with the repository; and `local`, that folder's own entry inside `.claude.json`, private to the home. Codex has one, `mcp_servers` in `config.toml` under its `CODEX_HOME`. Every other harness reports that cones has not verified where it keeps MCP configuration instead of guessing. No harness cones reads reports whether a running session actually loaded a server, so the panel states what is configured and says so.

Each row is a server with the transport and the command or address its own entry states. A scope with no file says so; an unreadable or malformed file shows its parse error rather than reading as empty.

| Key | Action |
| --- | --- |
| `x` | Stage removal of the selected server from its scope, or take that change back. |
| `c` | Copy the selected server into another scope that stores servers the same way. `← →` choose the scope and `enter` stages it. |
| `s` | Save the staged changes. |
| `u` | Discard every staged change and keep the panel open. |
| `esc` | Close the panel. Staged changes are discarded and no file is written. |

The prompt lists what a save will do before it happens. Saving rewrites only the servers table of the scopes named in that list, through a temporary file in the same directory, and keeps the file's mode, because `.claude.json` holds credentials. Comments and unrelated settings in `config.toml` survive. Copies are written before removals, so moving a server between scopes in one save works. A failed write leaves the file as it was and keeps the changes staged, so the same save can be retried once the cause is fixed.

Saving changes files, not processes. A session that already loaded these servers keeps them until it next starts; cones never restarts a live session.
### History

History appears below the live list, ordered by latest recorded activity regardless of grouping. It excludes live sessions and identified Claude ledger sessions. Its independent `history_columns` default to last active, folder, model, context and last reply. Live state and activity chart columns are not offered. [Historical sources](harness.md#historical-sessions) define which conversations appear.

Pages contain 50 rows. Arrows, page-up/down and the wheel over the list load older entries without wrapping. With the cursor in history, typing or pasting edits the filter directly, including while results load or no rows match. Backspace and the usual text editing keys edit the search; `esc` clears it, or hides history when empty. Filtering searches beyond loaded pages. `ctrl+r`, or hiding and reopening history, refreshes the snapshot; live polling does not rescan it.

Typing in history searches session titles, folders, harnesses, IDs and conversation text. `ctrl+f` also opens the search field. Each conversation appears once. A row whose title already holds the words stands alone; the rest carry one excerpt line, a short run of whole words around the match with markup removed, and yellow marks the words that matched. Selecting a result opens its matching passage; `enter` resumes it. In the explicit `ctrl+f` field, `enter` keeps the search and returns to the list.

`shift+tab` chooses what a query means, and the prompt says which is active. By words, the default, every word typed must appear in the same passage; common English filler such as `something about` is dropped, so it neither hides a match nor stands in for one, and a query of nothing but filler searches for the filler itself. By meaning, passages close to the query are returned instead and `≈` marks them. Titles, folders, harnesses and IDs match in both. Switching reruns the current query.

Search by meaning uses MiniLM locally. The first such search downloads the model, about 90 MB, and builds passage embeddings in the background, reporting how many passages remain. Embedding only runs while such a search is on screen, so `ctrl+r` in history also fills the index for every conversation, with no query attached, and reports its progress in the same place; it stops when nothing remains, and a second `ctrl+r` calls it off. Conversation text and queries stay on the machine. If the model cannot load, the list says so and `ctrl+r` retries; `shift+tab` returns to words, which never loads the model at all. Resemblance below 0.5 cosine is discarded as noise. The model works best with English; search by words also handles other languages.

The rebuildable search cache lives under `STATE_DIR/search/`. Text comes from visible user and assistant messages, excluding thinking, tool output and harness control records. Changed transcripts are reindexed on refresh; unchanged passage embeddings are reused. SQLite stores the text index and vectors, and model files are cached alongside it. Opening history without a query does not load or download the model.

Selecting a row loads its conversation in the pane after a short cursor rest. Messages appear in chronological order, with the latest message at the bottom. Claude and Codex use their prompt and response markers; pi uses shaded prompt blocks and unmarked replies. Replies render Markdown with highlighted code. Recorded tool calls appear as compact names and inputs; tool outputs and thinking stay out of the conversation. The pane says `history · read only`. Scrolling up loads earlier messages in bounded pages and keeps the text you were reading in place. Large individual messages remain bounded; omitted text is marked.

These are read-only presentations based on each harness's default appearance. Native extensions, custom themes and interactive tool widgets are not reproduced.

The wheel scrolls the preview without changing the selection. When focused, arrows and page-up/down scroll; home/end reach the loaded beginning or latest text. Reaching the beginning requests an older page when available. `tab`, `esc` or `ctrl+z` returns to the list. Typing and pasting are ignored apart from `c`, which opens the copy menu. Focused `ctrl+r` rereads the preview at the bottom; `ctrl+\` changes its layout.

`c` in a focused preview offers its last response, each fenced code block in that response and the row's own details. Blocks are named by language and first content line, and a block whose closing fence has not arrived is marked as still streaming. The chosen text goes to the system clipboard with terminal escapes stripped, since a transcript is data rather than commands.

From a focused preview, `→` opens the context inspector: what instructions, skills and MCP servers the selected session's own records hold, and where each came from. It reads only those records and the folder beside them, so it starts no harness client, sends no prompt and writes nothing. Categories list their entries, `→` opens an entry and then its text, and `←` steps back and finally returns to the conversation with the same entry still selected. Every entry says whether its text is in the session records, only named by them, or merely a file on disk; installed is not loaded. A recorded file that no longer matches the copy on disk says so, and the recorded copy is what the entry shows. Token totals are the harness's own reported numbers; an unreported count stays unknown rather than estimated. Claude and Codex record different things, and a category a harness never records says that instead of appearing empty.

A search preview starts at the matching passage. Scrolling past either end loads more conversation in that direction. Large individual messages show a bounded excerpt around the match, with omitted text marked.

`enter` resumes the selected conversation, including from its preview. An existing composer draft is preserved while searching and resuming history. Its native viewer replaces the preview and is reused when the session appears live. History has no delete action. Browsing it starts no harness client.

## Viewer

The pane shows the focused viewer, otherwise the selected session's viewer. Sessions, runs and historical rows never show another row's output. A live session stays blank while its client starts. Runs and historical rows use their read-only previews until their viewer opens; menu rows preview their button. Folder and job rows can retain the last focused viewer.

Resting on a joinable live row can prepare its viewer before entry. A run whose session is still up is such a row, finished or not: its session is collapsed into the run's row, and resting there joins that session the way resting on an agent does, replacing the read-only preview with the live pane. This never starts a new agent: a run whose session has settled requires an explicit open, since only a resume could show it, and a saved Codex thread waits for entry if its daemon has exited. A speculative viewer says `attach` until entered, then `return`. A preview may be closed to make room for another; entered Codex clients, resumed runs and historical viewers are retained, while a run's join makes room like any agent's. A Claude client left in its own agents list closes so that list cannot appear under a session's name.

All viewers close with the dashboard. Whether that also ends the session depends on [native ownership](harness.md#native-actions); this closes the owned terminal client for pi, OpenCode, the experimental launchers and interactive Claude forks. Stopping or removing a row is a separate action.

| Input in a native viewer | Behavior |
| --- | --- |
| `tab` | Return when the harness's empty prompt is recognized or zsh reports an empty command line; otherwise pass through for completion. Other shells always keep Tab. |
| `ctrl+z`, `ctrl+\` | Dashboard navigation, as above. Ctrl+Z returns even with an unfinished draft. |
| `←` | Return when the harness's standard empty editor is recognized or zsh reports an empty command line; populated or multiline input keeps the arrow. Modified arrows stay native. |
| `ctrl+c` | Dashboard quit confirmation in an agent viewer. These clients interpret two presses as quit, and Claude's first press leaves the conversation for its agents list. Use `esc` to interrupt a turn. In a terminal, Ctrl+C interrupts the shell's foreground command. |
| Other keys, including `esc` and `shift+tab` | Pass to the native client. Classic terminal encoding means some modified keys, including shift+enter, cannot be distinguished there. |
| Shift-page-up/down | Scroll emulator history. |
| Wheel | Scroll the viewer under the pointer, even without focus. Clients requesting mouse events receive them; otherwise emulator history scrolls. Shift-wheel always uses emulator history. A focused full-frame client that requests no mouse events, such as Codex, gives the mouse back to the terminal, so dragging selects text and the wheel belongs to the terminal until the list returns. |
| Text paste | Preserve bracketed paste when the client requests it. An empty paste, used for images by VS Code, becomes the client's ctrl+v. |

The [experimental terminal launchers](harness.md#additional-terminal-harnesses) return with Ctrl+Z; Tab and Left stay native even with empty input. OpenCode returns with Left when its standard session editor is empty. Drafts, multiline input and native menus keep Left; Tab stays native. Ctrl+Z always returns to the list.

Typing returns to the live screen. Beside the list, and for clients that read mouse events, terminal text selection needs the terminal's modifier, such as option-drag in iTerm2. Split viewers use the dashboard's hint line so the harness retains its own bottom status row. For delays, see [diagnostics](cli.md#diagnostics).

## Composer

Type an instruction and press `enter` to start a native session in the selected row's directory. From the menu or without a selected directory, it uses the dashboard's cwd. On `jobs` or `new job`, submitting an instruction opens the job wizard instead. `shift+tab` cycles the [configured harnesses](jobs.md#composer-harnesses), then terminal; the prefix names the selection. A harness turned off by its [enabled switch](jobs.md#composer-harnesses) is skipped, including as the harness the dashboard comes up on, and its [discovery](harness.md#discovery) stops too; the terminal stays reachable with every harness off. Identified agent rows show their reported model.

On `terminal`, type or paste a command and press `enter` to run it in a new interactive shell in that directory and focus its pane. An empty command opens the shell at its prompt. The command field supports the composer's editing keys and `shift+enter` for a new line. The prefix shows the detected shell, such as `terminal (zsh)`. cones uses an executable `$SHELL`, then the account's configured shell, then `/bin/sh`. In zsh, Left or Tab returns to the list when the command line is empty. With a command typed, Left edits and Tab completes using your existing bindings. Continuation lines and foreground programs keep both keys. Other shells keep their native Left and Tab bindings. Ctrl+C interrupts commands, and Ctrl+Z always returns to the list.

From the list, Enter reconnects to a selected owned terminal when the command field is empty. Tab focuses a viewer already open in this dashboard. Select a folder to open another shell, or type a command to start one. Shell rows survive dashboard refreshes and closure; they end when the shell exits or you stop it with Ctrl+X twice. Terminal commands and agent instructions keep separate drafts when switching with `shift+tab`. Escape clears a drafted command; with the command empty, it returns to the default harness.

The instruction wraps to at most eight text rows. In a side-by-side pane it aligns with Claude or pi's input box when that box is near the bottom; Codex and log viewers keep the composer at its normal position. Scrolling history does not move it.

A launch immediately selects a row containing the harness, directory and first instruction line, while preparation runs in the background. List focus stays available until the viewer is ready. Native discovery fills in reported details and replaces the launch identity without changing the viewer or taking selection back after you move away. Unrelated arrivals do not take selection while typing or viewing a session. `esc` or `ctrl+z` cancels pending preparation; other keys do not cancel it; a cancellation or failure removes the launch row and restores its instruction unless you have typed new text.

The prefix names the harness the launch starts, and nothing else: its [launch settings](#launch-settings) stay in the `ctrl+o` picker rather than beside every instruction. Native sessions retain their harness permissions. Supervised runs add a timeout and use the [unattended run contract](jobs.md#what-the-harness-is-told). [Launch identity](harness.md#composer-identity) explains attribution limits.

### Launch settings

`ctrl+o` opens the selected harness's own settings under the list, with the composer and its draft still in place: Claude's model and effort, Codex's model and full access, pi's model, provider and thinking, and one model row for every other harness that takes one. Amp and droid define none, so `ctrl+o` says so and opens nothing. It is unavailable on the terminal, a menu button, the folder row, a history search and the jobs screen, where the composer names no harness.

The rows are the [config editor's](#config) own controls, and a change is written to `defaults` in `jobs.yaml` as it is made, so there is no save key and nothing to lose by closing the picker; the status line names the file. Scheduled jobs pick the new value up on their next run, and a running session keeps what it started with. The saved choice survives a restart, and reopening `ctrl+o` shows it. `↑ ↓` select a row, `enter` opens the choices or text editing, `backspace` restores the harness default, and `?` explains the selected setting. `esc`, `tab` or another `ctrl+o` closes the picker and keeps the draft and the selection. A failed write keeps the picker open with the value that was typed, leaves the previous default intact and shows the error in the hint row.

### Text and images

Keys without another action type into the composer. Text prompts share these editing controls:

| Keys | Edit |
| --- | --- |
| `← →` | Move one character. With nothing typed, `←` leaves the jobs screen and the filter, since there is nothing to its left. |
| `alt+← alt+→`, `ctrl+← ctrl+→`, `alt+b alt+f` | Move one word. |
| `home end`, `ctrl+a ctrl+e` | Move to the line's ends. With an empty composer, `ctrl+e` edits a selected job. |
| `backspace delete` | Delete one character. |
| `alt+backspace`, `ctrl+w` | Delete the preceding word. |
| `alt+d` | Delete the following word. |
| `ctrl+u ctrl+k` | Delete before or after the cursor. |
| `ctrl+v` | Paste a clipboard image. |

Words are runs of non-space characters. macOS terminals commonly translate command/option shortcuts into these control/alt keys. Text pastes insert at the cursor. Images use macOS `osascript`, are saved as temporary PNGs and appear as `[Image #n]` markers; each marker deletes as one character and expands to its path at launch.
