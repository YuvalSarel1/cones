# cones and cmux

[All comparisons](README.md)

**Choose cmux** for a Mac desktop workspace containing native agent terminals,
development servers, browser automation and diff review. It owns panes and
workspace layouts without requiring a task board or worktree per change.
[Overview](https://github.com/manaflow-ai/cmux/blob/bc23759f031213fb3b056f330197b34dc5bd3840/README.md)

**Choose cones** for a terminal dashboard across supported native sessions and
projects, including work started elsewhere. cmux Vault also indexes local agent
archives: opening a conversation starts native resume, while focusing an existing
pane is a separate action. Neither tool's discovery establishes control of an
arbitrary external terminal. [cones support](../harness.md),
[Vault](https://cmux.com/docs/vault)

cmux's browser and review surfaces keep web verification beside the agent; cones
leaves those activities to other tools. Both can fork supported conversations in
the same directory, so separate conversations still share edits. Arrange worktrees
separately when isolation matters.
[Browser API](https://cmux.com/docs/browser-automation),
[Fork](https://cmux.com/blog/cmux-fork),
[cones dashboard](../dashboard.md)

cmux Feed can answer supported Claude permissions and questions through hooks;
plain Codex hook approvals remain native. cones leaves replies in native viewers
and lacks a Codex approval-wait signal. Its hosted terminals survive dashboard
closure, with one attachment per host. cmux normally restores layouts and resumes
captured conversations; its inspected opt-in local tmux mode preserves processes
across app exit, but release availability was not established.
[Feed](https://github.com/manaflow-ai/cmux/blob/bc23759f031213fb3b056f330197b34dc5bd3840/docs/feed.md#decision-semantics),
[cones limits](../harness.md),
[restore](https://cmux.com/docs/session-restore),
[local tmux](https://github.com/manaflow-ai/cmux/blob/bc23759f031213fb3b056f330197b34dc5bd3840/docs/local-tmux.md)

Reviewed 2026-09-22. cmux:
[bc23759](https://github.com/manaflow-ai/cmux/commit/bc23759f031213fb3b056f330197b34dc5bd3840),
plus official documentation. Running cones inside cmux was not tested.
[Review basis](README.md#review-basis).
