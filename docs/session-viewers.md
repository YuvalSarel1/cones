# Session viewer reuse

Checked on 2026-09-17 against cones' Rust/Ratatui dashboard. This survey covers 32 projects and components, including related forks and predecessors. The tables link to the sources used. Capability claims describe the checked source trees; released versions can differ.

Use **tui-markdown** inside the existing pane and keep cones' transcript readers. It provides the reusable part we need immediately: Markdown converted to Ratatui text. The pane can show chronological conversations, start at the bottom, and load older messages without another executable, session catalog or harness launch.

This library does **not** supply Claude, Codex or pi skins. That presentation remains cones code, guided by native prompt markers, spacing and themes. Codex's history cells and pi's message components are useful primary references. None of the checked terminal viewers is a drop-in reproduction of all three native interfaces.

For runs, keep reading cones' own captured events, stderr and terminal ledger record. A generic agent viewer does not know which output and outcome belong to a particular cones run, especially after its native session is resumed.

## Strongest candidates

| Project | Relevant capabilities | Reuse in cones | Finding |
| --- | --- | --- | --- |
| [tui-markdown](https://github.com/joshka/tui-markdown) | Rust library returning Ratatui `Text`; Markdown tables, inline styles and code blocks; customizable styles. MIT/Apache-2.0. | Direct dependency. Compatible with the current Ratatui generation. Code highlighting is optional. | Best immediate fit. The prototype preserves styles through wrapping and uses cones' existing input and scrolling. |
| [ccrider](https://github.com/neilberkman/ccrider) | Go/Bubble Tea TUI, search, SQLite catalog, Claude/Codex/pi and other providers. Public parser packages under `pkg/`. MIT. | Parser behavior and fixtures are useful references. A separate search tool is plausible. | Strongest standalone terminal browser in this shortlist. Its TUI opens a browser rather than accepting one transcript file; `view` exports Markdown. Current Codex fixture loses the user prompt. |
| [cclv](https://github.com/JeiKeiLim/claude-code-log-viewer-cli) | Direct file/stdin TUI, Markdown, tool/thinking folding, search, watch mode. Claude/Codex/OpenCode. | Easiest external viewer to put on a private PTY. | No pi adapter. Released parser misses current Codex UI prompts and shows injected instructions. README declares MIT, but the checked source archive has no license file. |
| [Agent Session Browser](https://github.com/gautamgpt1/agent-session-browser) | Claude/Codex/pi/Gemini/Antigravity; TUI and React web UI; event filters, bounded parsing, exports. MIT. | Useful normalized event model and web inspection reference. | Node runtime and SQLite catalog duplicate existing infrastructure. Current Codex UI prompt remains an unclassified record rather than a user message. |
| [Klovi](https://github.com/cookielab/klovi) | Modular TypeScript plugins, shared React components, desktop and browser distributions. Claude/Codex/Cursor/OpenCode. MIT. | Real server embedding API: `startKloviServer`; separate parser and UI packages. | Best modular web option. No pi plugin in the checked tree. Requires a web surface and a JavaScript runtime. |
| [openctx](https://github.com/12og3r/open-context-cli) | Ink terminal browser, rendered conversation, compact tools and in-pane search. Claude/Codex/Gemini. GPL-3.0-only. | Interaction reference; an external application integration is a separate option from copying code. | No direct-file pane interface documented and no pi support. Its resume feature can create a new Claude transcript. Do not transplant its implementation into cones under cones' existing license without resolving that choice. |

## Checked behavior

Three published tools were exercised against the same synthetic Codex JSONL, using temporary files and an isolated catalog. No agent was resumed and no model was called.

The fixture contains session metadata, an injected `response_item` user message beginning with `# AGENTS.md`, a real `event_msg.payload.item` with type `UserMessage`, and an assistant response. That distinction is already covered by cones' `codex_uses_the_ui_user_stream_and_one_assistant_stream` test.

| Tool | Version and entry point | Observed result |
| --- | --- | --- |
| cclv | 0.6.1, `--plain --agent=codex <file>` | Displays the injected instructions as the user message. The actual prompt is missing. |
| ccrider | 1.11.1, isolated `sync`, then `view` | Filters the injected instructions, but exports only the assistant response. The actual prompt is missing. |
| Agent Session Browser | 0.3.1, published `parseCodexJsonlFile` | Keeps the actual prompt's envelope as an unclassified item without user text. The injected instructions remain a user item. |

These are specific format gaps, not claims that the tools fail on every Codex transcript. All three have parsers for earlier formats. Their current parser sources were also inspected. Klovi's checked Codex decoder handles the older `user_message` envelope; it was inspected, not executed.

The rendering prototype was exercised with Markdown styling, Unicode wrapping, scrolling, older-page loading and run output updates. It retains cones' user-message source selection and adds compact native tool-call records rather than replacing those readers with one of these parsers.

## Other terminal and search options

| Project | What it provides | Fit here |
| --- | --- | --- |
| [Agent Session View](https://github.com/dotneet/agent-session-view) | TypeScript web/TUI browser for Claude and Codex, filters and HTML export. README declares MIT; no license file found in the checked root. | Another complete browser; no pi adapter or documented library exports. |
| [ccx](https://github.com/thevibeworks/ccx) | Go CLI/web viewer for Claude, Codex and Grok; conversation trees, tracing and exports. Apache-2.0. | Useful tool-event presentation reference. `view` produces terminal output rather than an embeddable interactive conversation component. No pi adapter. |
| [Claude Code Trace](https://github.com/delexw/claude-code-trace) | Claude viewer with Tauri/React desktop, web and Python/Textual TUI, expandable tools and tailing. MIT. | Its TUI starts a backend server. Too much runtime infrastructure for a pane, and Claude-specific. |
| [claude-code-tools](https://github.com/pchalasani/claude-code-tools) | Session continuity and search tools, including a Rust/Tantivy search TUI. MIT. | Relevant to a future cross-session search feature, not a reusable conversation renderer. |
| [cass](https://github.com/Dicklesworthstone/coding_agent_session_search) | Broad multi-agent search/archive application with a Rust TUI and many connectors. | Excluded from the reuse shortlist: the checked license is modified MIT with an additional OpenAI/Anthropic restriction, not ordinary MIT. It also introduces an archive/search stack far beyond peeking at one file. |

## Web and desktop alternatives

These can inform the visual design or provide an optional external inspection tool. None is a direct Ratatui conversation widget.

| Project | Coverage and implementation | Assessment |
| --- | --- | --- |
| [AgentsView](https://github.com/kenn-io/agentsview) | Go service and browser UI, broad provider coverage including Claude/Codex/pi, search and analytics. MIT. Originally linked through `wesm/agentsview`. | Strong standalone archive browser. Its service and database should not become prerequisites for selecting a row in cones. |
| [cderv/agentsview](https://github.com/cderv/agentsview) | Related AgentsView fork with additional archive, database and analytics capabilities. MIT. | Checked separately; no distinct terminal embedding advantage. |
| [claude-devtools](https://github.com/matt1398/claude-devtools) | Electron/React Claude transcript debugger, tool inspection and context analysis. MIT. | Useful reference for tool detail and grouping; a separate desktop application. |
| [Claude Code History Viewer](https://github.com/jhlee0409/claude-code-history-viewer) | Tauri/React desktop history application. MIT. | Rust in its backend does not make its React transcript view usable in Ratatui. |
| [d-kimuson/claude-code-viewer](https://github.com/d-kimuson/claude-code-viewer) | Full web Claude client with history and active interaction. MIT. | Owns more session interaction than a read-only peek needs; not a terminal component. |
| [Agent Sessions](https://github.com/jazzyalex/agent-sessions) | Native macOS multi-agent history, search, quota and resume application. MIT. | Good standalone Mac option and visual reference; platform/UI mismatch for embedding. |
| [Session Analyzer](https://github.com/Yijia-Zhou/session-analyzer) | Browser inspection of Claude, Codex and DeepSeek sessions, with tools and file changes. BSD-3-Clause. | Useful conversation-plus-tool-detail design; Node/web runtime and no pi adapter. |
| [JUrban/codex_viewer](https://github.com/JUrban/codex_viewer) | Read-only local Codex browser, Node application. MIT. | Narrow provider coverage and a separate web surface. |
| [Session Explorer](https://github.com/prime-radiant-inc/claude-session-viewer) | Local Claude web viewer. No explicit license found in the checked root or package manifest. | No advantage over the stronger documented and licensed web candidates. |
| [RustingSword viewer](https://github.com/RustingSword/claude_code_session_viewer) | Claude web browsing/search application. No explicit license found in the checked root. | Same integration gap, with less documented packaging. |
| [wesm/agent-session-viewer](https://github.com/wesm/agent-session-viewer) | Earlier Python web viewer for Claude and Codex. MIT. | Its README explicitly says it is superseded by AgentsView and no longer maintained. Use the successor for evaluation. |

## Export and replay tools

| Project | Useful capability | Assessment |
| --- | --- | --- |
| [claude-code-transcripts](https://github.com/simonw/claude-code-transcripts) | Python, paginated Claude HTML transcripts. Apache-2.0. | Good export/reference implementation, not an interactive terminal pane. |
| [codex-transcripts](https://github.com/prateek/codex-transcripts) | Codex adaptation producing self-contained HTML. Apache-2.0. | Good optional export path; does not solve terminal rendering. |
| [codex-transcript-viewer](https://github.com/masonc15/codex-transcript-viewer) | Python conversion to searchable single-file HTML. MIT. | Same export use case, Codex-specific. |
| [claude-code-log](https://github.com/daaain/claude-code-log) | Python Claude log conversion, browser output and supporting navigation. MIT. | Useful export formatting reference; another parser/runtime to maintain if adopted for peeks. |
| [claude-replay](https://github.com/es617/claude-replay) | Multi-provider HTML replays with playback, tool folding and redaction. MIT. | Best fit when the goal is sharing or replaying a session, rather than reading it inside cones. |
| [cclogviewer](https://github.com/brads3290/cclogviewer) | Go conversion from a Claude JSONL file to interactive HTML. MIT. | Simple exporter; does not provide a terminal UI. |

## Rendering and native code

| Component | Reuse boundary | Assessment |
| --- | --- | --- |
| [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) | Rust Markdown parser. MIT. Used by tui-markdown. | Reuse through the renderer. Using it directly would leave us implementing styles, blocks and tables. |
| [Termimad](https://github.com/Canop/termimad) | Rust terminal Markdown renderer using its own formatting/view types and Crossterm. MIT. | Viable in a new terminal app; adapting its output to Ratatui adds work that tui-markdown already handles. |
| [Codex TUI](https://github.com/openai/codex/tree/main/codex-rs/tui) | Apache-2.0 Rust code, including exported `render_markdown_text`. | Closest source for exact Codex styling. The checked renderer spans 2,810 lines plus helper modules and depends on the larger Codex workspace. A maintained extraction is possible, but importing the whole TUI is disproportionate. |
| [pi](https://github.com/earendil-works/pi) | MIT TypeScript monorepo with a published terminal library, Markdown component, and native session parsing/context helpers. Former `badlogic/pi-mono` redirects here. | Best reference for pi session trees and native behavior. Using its terminal library for cones requires a Node wrapper. Its session manager is not a cross-harness parser. |

## Integration choices

1. **In-process rendering:** cones reads the selected source and owns navigation; tui-markdown renders assistant text. This keeps the existing binary and testable message selection. It is the recommended default.
2. **External viewer behind a normalizer:** cones could translate its selected messages into Claude-shaped JSONL and feed cclv. That would bypass its Codex/pi parser gaps and reuse search/folding, but adds an executable, a format adapter and a viewer lifecycle. Directly launching cclv on native files is not sufficient.
3. **External web inspection:** expose a deliberate action for an existing browser application or HTML export. Klovi, AgentsView, Agent Session Browser and claude-replay are relevant here. This is a separate feature from the terminal peek.

No upstream code was copied into cones during this evaluation. The prototype adds a normal Cargo dependency on tui-markdown; the existing harness definitions remain responsible for selecting actual user and assistant messages.
