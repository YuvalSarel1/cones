# cones and Agent Deck

[All comparisons](README.md)

**Choose Agent Deck** for native agent terminals organized through tmux, integrated
worktrees, project skills, optional Docker sandboxes and SSH/browser access.
It supports macOS, Linux and WSL. Execution belongs to the underlying harness.
[Overview][deck-readme]

Its TUI quick fork normally creates a worktree and copies uncommitted changes.
Ordinary sessions need no worktree; CLI forks require explicit worktree options.
cones forks conversations in the same directory and groups existing worktrees,
leaving creation and cleanup to you. [Fork defaults][deck-fork-config],
[CLI forks][deck-cli-fork], [cones forks][cones-forks]

Recall now adds opt-in transcript indexing across Claude, Codex, pi, Gemini,
OpenCode and Hermes, searchable session notes and tags, context briefs, MCP
retrieval and remote search.
Unregistered conversations resume only for Claude; other harnesses need an
existing registered session. Its OpenCode reader handles legacy JSON storage,
not the SQLite history cones reads. [Recall][deck-recall],
[resume limits][deck-recall-open], [OpenCode reader][deck-recall-opencode]

**Choose cones** for machine-wide local session discovery, previews and history
without registering sessions. Its history already offers word and local semantic
search, plus native resume for Claude, Codex, pi and OpenCode. Discovery of an
external terminal still does not guarantee attachment. [History][cones-history],
[support][cones-harness]

Agent Deck offers MCP configuration; both retain hosted terminals after detaching.
Agent Deck's Claude default skips permission prompts, configurably; cones
preserves interactive harness permissions, while supervised Claude jobs also
skip prompts. cones permits one dashboard attachment per owned host, and Codex
approval waits remain reported as working.
[permissions][deck-permissions], [jobs][cones-jobs], [limits][cones-harness]

Reviewed 2026-09-23. Agent Deck: [3b41e36][deck-base]; latest release
[v1.16.16][deck-release] includes Recall. cones: `c76fa83`.
Source and documentation review; live flows untested.
[Review basis](README.md#review-basis).

[deck-readme]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/README.md
[deck-fork-config]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/internal/session/userconfig.go#L3532-L3627
[deck-cli-fork]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/cmd/agent-deck/session_cmd.go#L1040-L1156
[cones-forks]: ../dashboard.md#fork-a-conversation
[deck-recall]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/docs/recall.md
[deck-recall-open]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/cmd/agent-deck/recall_cmd.go#L927-L969
[deck-recall-opencode]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/internal/recall/reader/opencode.go#L15-L56
[cones-history]: ../dashboard.md#history
[cones-harness]: ../harness.md
[deck-permissions]: https://github.com/asheshgoplani/agent-deck/blob/3b41e36de82d89f84a951e5c0f490fc6e1973c41/internal/session/userconfig.go#L2262-L2269
[cones-jobs]: ../jobs.md#what-the-harness-is-told
[deck-base]: https://github.com/asheshgoplani/agent-deck/commit/3b41e36de82d89f84a951e5c0f490fc6e1973c41
[deck-release]: https://github.com/asheshgoplani/agent-deck/releases/tag/v1.16.16
