# The dashboard

Back to the [README](../README.md). See [harness.md](harness.md) for session sources and supported actions, [jobs.md](jobs.md) for job policy, and [cli.md](cli.md) for commands.

`cones tui` shows live sessions and recent runs. The menu opens folders, jobs, configuration and help. The composer starts native sessions in the selected row's directory. No jobs file is needed to view sessions.

## The screen

| Area | Contents |
| --- | --- |
| Summary | Counts of working, input, idle and done sessions, plus jobs and runs. |
| Menu | `folder`, `jobs`, `config`, `help`. |
| Sessions | Activity icon, harness mark, optional harness name and state, title, then the configured [columns](#columns). |
| Jobs | A separate screen with enabled marker, harness, last run status and schedule, name, model, last run age and directory. A `new job` row opens the wizard. |
| Runs | Newest 200 visible runs: icon, job, status, fired time, duration, dollars and reason. |
| Viewer | A live terminal screen for the focused or selected session. |
| Composer | Instruction for a new session, followed by context-sensitive key hints or the last action's result. |

Sessions group by directory, sorted without case, or by state with input requests first. Within each group, sessions sort by reported start time, oldest first; unknown starts sort last, then by id. Pinned empty folders keep a row with their git branch and tree state.

Column headers are dim and unselectable. Widths only grow during a dashboard session, so changing values do not shift adjacent columns. Narrow panes clip the right edge. Filtering hides the column headers.

| State | Icon | Label | Color |
| --- | --- | --- | --- |
| `active` | Animated `▁▂▃▄▅▆▇` and back | working | plain |
| `blocked` | `▇` | input | yellow |
| `idle` | `▁` | idle | dim |
| `done` | `✓` | done | green |
| `failed` | `✗` | failed | red |
| `stopped` | `▁` | stopped | dim |
| `-` | `–` | `-` | dim |

The activity bar advances every 160 ms, holding three frames at each endpoint. Harness marks and the mascot stay still. The coordinator's title is orange with a `★` before it; the table never spells the word out. Without a `state` column there is no slot before the title; a session that requires its own terminal says so in the footer.

The pane shows live viewers only. Use `cones logs` for recorded output; the dashboard has no transcript details pane.

## Columns

`columns:` in jobs.yaml selects session columns. `harness` and `state`, when present, sit before the title in that order; the rest follow it in the listed order. Unknown names fail validation. Missing data shows `-`; [harness.md](harness.md#observe) names each source.

| Column | Cell | Default |
| --- | --- | --- |
| `harness` | The harness name after its mark, `✻ claude`; without it the mark stands alone | yes |
| `state` | working, input, idle, done, failed or stopped | yes |
| `context` | Reported prompt/window tokens, such as `98k/200k`; prompt alone when the window is unavailable | yes |
| `activity` | Counts per time bucket as bars; the window is bars × bucket | yes |
| `model` | Reported model under the name its provider presents it by, `claude-fable-5-1` as Fable 5.1 and `openai.gpt-6-astra` as GPT-6 Astra; an id of no name cones knows is shown verbatim, see [harness.md](harness.md) | yes |
| `age` | Time since the reported session start | yes |
| `last` | Latest reply or status text; directory when grouped by state | yes |
| `tokens` | Session input/output totals, such as `49.2M/201k` | no |

`age` counts from the session start and never resets. Claude's start comes from its transcript; Codex process rows and pi rows use process start, while detached Codex thread rows use rollout metadata or their saved launch record. Cost appears on run rows, not as a configurable session column.

```yaml
whole_columns: true
```

A column the list's right edge would cut through is left out, so the table ends on a column that fits. `whole_columns: false` draws as much of it as there is room for. The mark, harness, state and title are drawn either way, so a row names itself however narrow the list is. Cells keep the width of the widest value on screen, so a wide value can drop the column after it.

`ctrl+t` arranges columns against the live table. The strip lists visible columns first, then a separator and hidden ones. `← →` select a column, `space` shows or hides it, and `[` `]` reorder visible columns. `enter` saves `columns:`; `esc` restores the original set. The jobs screen uses its own fixed columns.

### Pane

```yaml
pane:
  at: right
  ratio: 50
```

| `at` | Layout |
| --- | --- |
| `right` | List on the left, viewer on the right; a divider between them. |
| `bottom` | List above, viewer below; a divider between them. |

`ratio` is the percent of the frame the viewer takes, 30 to 70. The list keeps the rest, less the divider. The defaults are `right` and `50`. There is no minimum terminal size for a split. `ctrl+\` changes the layout during use; [start](#start) controls whether the pane opens initially.

### Start

```yaml
start:
  harness: claude
  pane: true
```

`harness` is `claude`, `codex` or `pi`, initially selected in the composer. `pane` opens the viewer pane when true. Omitted fields use the values above. These settings are applied at startup; `shift+tab` and `ctrl+\` change the current dashboard without saving them. The config editor can save startup settings for future dashboards.

A job's default harness is the separate `defaults.harness` field.

### Activity

```yaml
activity:
  bars: 16
  bucket: 1m
  metric: lines
  bound: fleet
```

| Field | Values and meaning |
| --- | --- |
| `bars` | 1 to 64 buckets, oldest left and newest right. |
| `bucket` | Positive duration in `s`, `m` or `h`, at most `24h`. |
| `metric` | `lines`, `messages`, `tools` or `tokens` (output tokens). See the [activity mappings](harness.md#observe) for each harness. |
| `bound` | `fleet`: busiest bucket across loaded sessions; `row`: each row's busiest bucket; `log`: fleet scale with logarithmic values; a positive number: fixed count for a full bar, set in the file rather than the editor. |

Omitted fields use the values above. Bucket edges align to the clock, so bars move left once per bucket and only the newest grows between boundaries. Empty buckets use the lowest bar; rows with no activity in the window are dim. Future timestamps count in the newest bucket.

Sparklines count reported activity. They do not determine session state: low bars beside `working` mean the harness has written little recently.

## Keys

These keys apply to the dashboard. A focused viewer receives its own input as described under [viewers](#viewers).

| Key | Action |
| --- | --- |
| `↑ ↓` | Move between rows; up past the first table reaches the menu. |
| `enter` | With an empty composer, act on the row: open a session or finished run, follow a running run, start a job, or press a menu button. With an instruction, start a session; on `jobs` or `new job`, open the wizard with that instruction. |
| `shift+enter` | With an empty composer, open the selected viewer or menu screen over the whole frame once; leaving restores the prior layout. With an instruction, break the line instead of starting. Terminals reporting alt+enter use the same action. |
| `tab` | Move focus between the list and pane. Forms keep tab for their own input; leave those with `ctrl+z` or `esc`. |
| `shift+tab` | Cycle the composer's harness: Claude, Codex, pi. Inside a viewer, this key belongs to the harness. |
| `ctrl+x twice` | Stop, delete, hide, forget or remove the selected row, as listed below. |
| `ctrl+p` | Pin the selected row's folder, or the dashboard's cwd from the menu. |
| `ctrl+e` | Edit a selected job when the composer is empty; otherwise move to the instruction's end. |
| `ctrl+s` | Group sessions by state or directory. |
| `ctrl+t` | Arrange session columns. |
| `ctrl+f` | Filter rows; `enter` keeps the filter, `esc` clears it. |
| `ctrl+n` | Rename a Claude session by appending its native `custom-title` transcript record. A live session may later overwrite it from memory. |
| `ctrl+r` | Reload now. |
| `ctrl+\` | Toggle the pane from the list; switch between split and full frame from a focused viewer or menu screen. Also recognized as ctrl+4. |
| `ctrl+v` | Paste a clipboard image into the composer. |
| `ctrl+g` | Open this guide; `↑ ↓` scroll, `esc`, `enter` or `ctrl+g` close it. |
| `ctrl+z` | Return from a viewer or menu screen. Viewers keep running. |
| `esc` | Back out one step: armed action, typed instruction, jobs screen, then dashboard. Forms cancel their current edit or close. |
| `ctrl+c twice` | Quit within a 1.5-second confirmation window, from the list or a focused viewer. Viewers never receive it. |

The first `ctrl+x` marks the row red. Another key cancels it, as does inactivity for `confirm_secs` seconds, default 2. Set `confirm_secs: 0` to wait until a key; valid values are 0 to 600.

| Selected row | Confirmed action |
| --- | --- |
| Claude background session | `claude rm`, removing its job record while preserving the conversation. |
| Interactive session | Signal the verified process. |
| Codex daemon thread | Forget its saved record, hide the id and stop a live client on the row, if any. The thread remains resumable. |
| Running run, or job with a run in flight | Stop the run. |
| Finished run | Hide its row; keep the ledger, output and transcript. |
| Job without a run in flight | Delete it from jobs.yaml and reinstall schedules. |
| Pinned empty folder | Remove the pin; leave the directory alone. |

A removed row leaves the dashboard, not the harness. A deleted Claude session is still in `claude --resume`, and a forgotten Codex thread is still in `codex resume`. The hint line says nothing on success; the row leaving the list is the confirmation.

### Text editing

Any key that is not an action types into the composer. Text prompts share these editing keys:

| Keys | Edit |
| --- | --- |
| `← →` | Move one character. |
| `alt+← alt+→`, `ctrl+← ctrl+→`, `alt+b alt+f` | Move one word. |
| `home end`, `ctrl+a ctrl+e` | Move to the line's ends. |
| `backspace delete` | Delete one character. |
| `alt+backspace`, `ctrl+w` | Delete the preceding word. |
| `alt+d` | Delete the following word. |
| `ctrl+u ctrl+k` | Delete before or after the cursor. |

Words are runs of non-space characters. macOS terminal bindings usually translate command and option shortcuts into these control and alt keys. Text pastes are inserted at the cursor.

## Viewers

Viewers are harness clients on private ptys, rendered by a vt100 emulator. Agents remain in their harness daemons. Claude background sessions and Codex daemon threads can be opened; sessions marked `own terminal` cannot. A finished run resumes through `cones attach`; a running run opens its log. The [kinds table](harness.md#kinds) covers each case.

The pane shows the focused viewer, otherwise the selected session's viewer. A session never displays another session's screen. Non-session rows can retain the last focused viewer, while menu rows preview their selected button. A pane stays blank until its live viewer paints; it never substitutes transcript text.

`enter`, `tab` or a pane click gives a viewer focus. `tab` or `ctrl+z` returns to the list while it continues parsing output. Clicking a list row selects it and takes focus back. `ctrl+\` changes split/full-frame layout persistently; `shift+enter` supplies a temporary full-frame view. A split viewer keeps the same dimensions across focus changes, and its hints go in the dashboard's own hint row so the harness keeps its bottom status row, permission mode and all. A full-frame viewer reserves its last row for a strip showing its title, fleet counts, input requests elsewhere and return keys.

Inside a viewer, plain `tab`, `ctrl+z` and `ctrl+\` are dashboard keys, and so is `←` while the client's composer is empty: the cones emulator sees a caret sitting behind nothing but box art and a prompt marker, so the key has nowhere to go in the client and returns to the list instead. Shift-page-up/down scroll the emulator. `ctrl+c` is also the dashboard's, and quits on the second press as it does from the list: Claude Code, Codex and pi all read two of them as quit, and Claude Code's first one drops to the agents list, so it never reaches the client; `esc` interrupts a turn. Other keys, including `esc` and `shift+tab`, go to the viewer. The wheel scrolls the viewer under it, focused or not: clients that request mouse events receive them; otherwise the emulator scrolls its history. Shift-wheel always uses emulator history. Typing returns to the live screen. Terminal text selection may require the terminal's modifier, such as option-drag in iTerm2.

Text pastes preserve bracketed-paste mode when requested. An empty paste, which VS Code sends for a clipboard image, becomes the harness's ctrl+v. The composer saves images as temporary PNGs and shows `[Image #n]` markers, each deleted as one character and expanded to its path at launch. Image clipboard access uses macOS `osascript`.

### Viewer lifetime

Resting on a joinable Claude row opens a speculative viewer after 50 ms in split view or 400 ms otherwise. Finished runs and Codex clients are opened only on request because opening them can change the session or fleet. A speculative viewer says `attach` until first entered; an entered live viewer says `return`.

The live pool targets three viewers. Making room closes the least recently focused Claude attach of a listed session. Codex clients and resumed runs cannot be reopened speculatively, so they are retained and may exceed the cap. Two speculative viewers have a separate pool, oldest evicted first. A viewer left in Claude's own agent list is closed to avoid displaying that list under a session's name.

All viewers close with the dashboard. Closing a viewer leaves its daemon-owned agent running; confirming a stop or removal is a separate action. Codex threads are recorded after their first turn for later resume. A launch left before its first turn has no resumable record. Codex startup prepares the daemon connection on a background thread; `esc` cancels the pending opening.

### Terminal behavior

The emulator answers cursor-position, device and color queries. Colors are probed once from the real terminal after raw mode starts and before input polling. Kitty keyboard queries stay unanswered because input uses classic xterm encoding; some modified keys, including shift+enter, cannot be distinguished inside a viewer.

Synchronized viewer output holds the previous screen and cursor until the update ends, with a 150 ms timeout. Dashboard redraws also use synchronized output, including clears and cursor placement, so terminals that support it display complete frames while scrolling. Each viewer pump waits up to 1 ms between reads and spends at most 50 ms collecting a burst, allowing large redraws without starving dashboard input. UTF-8 tails are retained across reads. Input the client has not read, whether typed keys or the emulator's replies to its terminal queries, is capped at 8 MiB; past that the viewer is closed rather than left to grow. On close, the pty is drained while the child is reaped to avoid a macOS wait deadlock.

Scrollback retains 1000 lines from the normal screen. The vendored vt100 0.16.2 patch lets a scroll region beginning at row zero contribute history even when a fixed prompt occupies the rows below it. Interior regions and alternate screens add no history. The patch also exposes unscrolled cells so layout can inspect the live input box without moving the history view. `viewer::tests::history_above_a_fixed_prompt_stays_in_scrollback` and `tui::tests::the_composers_rule_lands_on_the_harnesss_own` cover these changes.

Viewer bytes never reach the real terminal directly. Dashboard shutdown restores the shell's original tty settings and disables its own reporting modes.

## The menu

The menu contains `folder`, `jobs`, `config` and `help`. Use `↑` from the first table to reach it, then `← →` or a click to choose a button. `enter` opens the selected screen in the pane when enabled; `shift+enter` opens it over the whole frame. This applies at every terminal size. `tab`, `ctrl+z` or `esc` returns to the list unless a form uses tab for input.

`folder` accepts an existing directory, with `~` expansion and relative paths based on the dashboard's cwd. `tab` completes directory names; a second tab lists remaining matches. Hidden names need a `.` prefix. `↑ ↓` recalls up to 20 previously seen folders, newest first. A newly pinned empty folder gets a selected row; a folder already containing sessions keeps the current selection. `ctrl+p` pins without opening the prompt.

Pins live in `~/.cones/folders`, recall history in `~/.cones/recent`. These paths follow `--state-dir`. Sessions replace an empty folder's placeholder while they exist; the placeholder returns when they leave.

## The composer

Type an instruction and press `enter` to start a native session in the selected row's directory. From the menu, or with no selected directory, it uses the dashboard's cwd. The input wraps to at most eight text rows. In side-by-side layout its lower rule aligns with the harness input box on the live screen and stays in place while scrolling history.

Claude starts with `--bg`; a placeholder row appears immediately and becomes the registry row when available. A failed launch removes the placeholder and restores the instruction. New sessions take the selection unless a viewer is focused or an instruction is being typed. Codex opens a client of its app-server daemon, which keeps the thread when the client goes. pi has no daemon and no attach, so a pi is the viewer itself: `enter` on its row returns to that viewer, and closing the viewer or quitting the dashboard ends the session.

`shift+tab` cycles the harness, so the hint names the key's effect rather than the next harness. The composer takes model and provider settings from `defaults`: `model` for Claude, `codex_model` for Codex, and `bedrock`, `aws_profile` and `aws_region`, which reach Claude only. A pi takes none of them and starts on its own configured model. The config editor is where those fields change. The prefix shows the selected harness alone; the model each session came up on is the `model` column of its row.

Native sessions use their harness's permissions. Job budgets, timeouts and tool restrictions apply to supervised runs, started with `cones run --prompt` or `once` in the wizard.

## The jobs screen

The menu's `jobs` button opens jobs in file order, followed by `new job`. With the pane enabled it occupies the pane; otherwise it replaces the main list. `enter` starts a job, `ctrl+e` edits it, and `ctrl+x twice` stops its running run or deletes the job. `esc` returns to the main dashboard.

## The wizard

Open `new job`, or press `enter` on the menu's `jobs` button with an instruction typed. `enter` accepts each answer, `← →` pick a schedule, `↑` or backspace on an empty answer goes back, and `esc` cancels. Earlier answers remain visible.

| Question | Answer |
| --- | --- |
| `what` | Task, seeded from the composer. |
| `where` | Existing directory; `~` expands, relative paths use the jobs file's directory, and empty uses the selected row's directory. Tab completes paths. |
| `when` | `once`, `hourly`, `daily`, `weekdays`, `weekly` or `cron`. `once` immediately starts a supervised run under the first job's policy, or read-only defaults when no template is available. |
| `at` | `09:00` for daily/weekdays; `mon 09:00` for weekly; five-field cron otherwise. Hourly uses `0 * * * *` and skips this question. Empty uses the displayed default. |
| `name` | Suggested from the task; 1-80 ASCII letters, digits, `-` or `_`. |

New jobs inherit `defaults.harness`, falling back to Claude; only Claude jobs currently pass execution validation. Editing preserves fields the wizard does not expose. Saving validates the whole file and rewrites only the selected job block, retaining surrounding formatting and comments. Save and delete run `cones install` afterward; errors appear in the hint line.

## The defaults editor

Open `config` to edit jobs.yaml. Fields are grouped under `cones` (dashboard settings), `harnesses` (models and provider), and `runs`, which holds the value every run starts with, for each field a run has, unless the job's own line says otherwise. Subheadings name actual config blocks or harnesses. Job fields and defaults are listed in [jobs.md](jobs.md); dashboard fields are described above.

The `runs` section comes up shut, since a run inherits it and rarely changes it: `enter` or `→` on its head opens it, and `↑ ↓` stop on that head until they do.

Each row displays its control and current value. `↑ ↓` select a field; `← →` change a choice or step a number, validating and saving immediately. `backspace` restores the built-in. `enter` moves on to the next field; on a plain text or number field it opens editing first, and a second enter accepts the text and moves on. Escape restores the previous value. Leaving the form keeps already saved changes. Validation errors focus the relevant field and leave the file untouched.

| Control | Fields and behavior |
| --- | --- |
| Choices | Harness, provider, write, overlap, notify, pane settings, metric and chart scale. Arrows cycle; an initial letter selects a matching option. An unset field brackets the built-in's own word, such as `[claude]`, so the effective value is on the row; `default` appears only where the built-in is the harness's own. |
| Choices or text | Claude model, Codex model, AWS region and bucket. An empty slot follows the words; arrows step onto it or typing starts in it, and a value the words do not offer stays there. Stepping back onto a word drops it. The Claude words carry `opus[1m]` and `sonnet[1m]` beside the bare aliases: the suffix asks for the million-token window, which `opus` alone does not get. |
| Numbers | `confirm_secs`, bar count, turn cap and daily budget step by 1; timeout by 5 minutes; per-run budget by 0.25 USD. Steps stay on their grid and never go below zero; validation can reject zero. Non-numeric built-ins step from zero. |
| Text | AWS profile, and the names of the shell variables every run imports, separated by commas. |
| Columns | Arrows select, space shows/hides, brackets reorder, backspace restores defaults. Each change saves immediately; `ctrl+t` offers the same arrangement on the table. |

Empty values omit overrides and use built-ins. Harness-owned fields pass no override when empty. Bedrock requires both an explicit AWS profile and region, including in the temporary session form. Codex settings can be saved for native sessions, but supervised Codex jobs are unavailable.

Saving replaces only `defaults`, `columns`, `activity`, `pane`, `start` and `confirm_secs`, retaining job blocks. A missing file is created with `jobs: []`. The editor does not run `cones install`; reinstall when changing environment settings that scheduled jobs must receive, since launchd retains the environment captured at installation.

## Polling

A background read starts one second after the previous read completes. Only one read runs at a time. Stops, deletions, manual reloads and viewer returns invalidate older reads; the dashboard discards a stale result and immediately starts a fresh one. Confirmed deletions disappear before the harness command finishes and return if it fails.

The event loop waits up to 25 ms for input, or 8 ms with a focused viewer, and redraws when needed. Animation checks run on a 100 ms cadence. Reads include the ledger, job file, harness registries, process information, coordinator records and transcript metadata. No session hooks or filesystem watchers are installed.

Claude and pi transcript summaries are cached by file length and recounted when it changes. Claude title/reply scans use growing tail windows from 256 KiB to 16 MiB. Codex caches immutable headers and the first prompt, then folds only appended complete lines into its rollout state. State comes from reported events and statuses, never an inactivity threshold; polling cannot expose a change the harness has not written yet.

The config-button preview rereads jobs.yaml every frame. If profiling shows this cost, cache the form during the dashboard's rebuild.

For delays, use `cones tui --debug`. [cli.md](cli.md) lists its timing events; `scripts/bench-tui.py` summarizes logs or measures the TUI with fixture harnesses, without model calls. Limits in the code are implementation settings, not guarantees of harness startup time or memory use.
