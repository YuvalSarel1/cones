# cones and herdr

[All comparisons](README.md)

**Choose herdr** for agents, shells, development servers and logs in persistent
terminal layouts, including across SSH machines. Its server owns workspaces,
tabs and panes, while agents retain their native interfaces. It supports Linux,
macOS and Windows. [Concepts][h-concepts], [remote machines][h-machines],
[platforms][h-install]

**Choose cones** to collect supported native sessions and conversation history
across existing projects. herdr recognizes agents launched inside its panes;
its documented discovery does not adopt arbitrary outside terminals. cones can
find work started elsewhere, but an external interactive terminal may remain
visible without being joinable. [herdr discovery][h-agents],
[cones support][c-harness]

herdr offers optional worktree creation and terminal automation through prompts,
keys and waits. cones groups existing worktrees but leaves creation to you; its
conversation forks share files. Its cross-session controls require native harness
operations. Both leave code review and merging to your chosen tools.
[Worktrees][h-worktrees], [automation][h-automation],
[cones dashboard][c-dashboard], [native operations][c-harness]

Reporting has different gaps. herdr combines process recognition, screen rules
and lifecycle integrations; unfamiliar prompts can fall back to idle. cones reads
native reports, but Codex approval waits remain working and external OpenCode
clients lack live state. Both preserve hosted processes after client closure.
herdr supports multiple clients; cones allows one dashboard attachment per owned
terminal. After server restart, herdr can restore layouts and resume eligible
conversations; cones offers native history after its hosted processes end.
[Detection][h-agents], [fallback][h-detect], [clients][h-concepts],
[restore][h-restore], [cones limits][c-harness]

Reviewed 2026-09-22. herdr: [c00a62d][herdr-base], plus official documentation for
0.9.1. Combined use was not tested. [Review basis](README.md#review-basis).

[h-concepts]: https://github.com/herdrdev/herdr/blob/c00a62dda169beb472fc2f386d0f673ca100dfa4/docs/versions/0.9.1/website/src/content/docs/concepts.mdx
[h-machines]: https://herdr.dev/docs/connecting-machines/
[h-install]: https://herdr.dev/docs/install/
[h-agents]: https://herdr.dev/docs/agents/
[c-harness]: ../harness.md
[h-worktrees]: https://github.com/herdrdev/herdr/blob/c00a62dda169beb472fc2f386d0f673ca100dfa4/docs/versions/0.9.1/website/src/content/docs/configuration.mdx#L96-L111
[h-automation]: https://github.com/herdrdev/herdr/blob/c00a62dda169beb472fc2f386d0f673ca100dfa4/docs/versions/0.9.1/website/src/content/docs/agent-automation.mdx
[c-dashboard]: ../dashboard.md
[h-detect]: https://github.com/herdrdev/herdr/blob/c00a62dda169beb472fc2f386d0f673ca100dfa4/src/detect/manifest.rs#L536-L583
[h-restore]: https://herdr.dev/docs/session-state/
[herdr-base]: https://github.com/herdrdev/herdr/tree/c00a62dda169beb472fc2f386d0f673ca100dfa4
