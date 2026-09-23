# cones and Vibe Kanban

[All comparisons](README.md)

**Choose Vibe Kanban** for an integrated local review workflow: isolated
workspaces, diffs, inline feedback, app previews and Git actions. It presents a
common conversation interface while native harnesses execute the work.
[Review][vk-review], [preview][vk-preview], [execution lifecycle][vk-container]

Maintenance matters: bloop announced its closure on April 10, 2026, leaving
Vibe Kanban community maintained. Hosted planning data was scheduled for removal;
local workspaces would continue. Remaining cloud documentation does not establish
that former hosted services operate. [Shutdown announcement][vk-shutdown]

Vibe Kanban creates worktrees for workspace repositories and attempts to commit
outstanding changes after successful coding executions. This structures isolation
and review, but commit timing becomes part of its workflow. Multiple sessions
within one workspace still share files. Its bundled Claude profile skips
permissions and Codex uses unrestricted filesystem access; users wanting approval
checks must configure them. [Worktrees][vk-worktrees],
[automatic commits][vk-container], [sessions][vk-sessions], [defaults][vk-defaults]

**Choose cones** to find, preview and revisit supported native sessions, including
work started elsewhere. Vibe Kanban's inspected session API covers its own
workspaces; no general external-session adoption path was found. cones preserves
native terminal interaction but leaves worktree creation, code review and merging
to other tools. Its forks share files, and visible external terminals may remain
unjoinable. Hosted terminals survive dashboard closure; Vibe Kanban browser
sessions depend on its backend, whose shutdown kills running executions.
[Session API][vk-session-api], [cones support][cones-harness],
[cones dashboard][cones-dashboard], [backend shutdown][vk-server]

Reviewed 2026-09-22. Vibe Kanban: [d5cbb53][vk-snapshot], plus official
documentation. Launcher targets macOS, Linux and Windows; release parity and
combined use were not tested. [Platforms][vk-launcher].
[Review basis](README.md#review-basis).

[vk-review]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/docs/reviewing-code.mdx
[vk-preview]: https://www.vibekanban.com/docs/browser-testing
[vk-container]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/local-deployment/src/container.rs
[vk-shutdown]: https://www.vibekanban.com/blog/shutdown
[vk-worktrees]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/workspace-manager/src/workspace_manager.rs
[vk-sessions]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/docs/workspaces/sessions.mdx
[vk-defaults]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/executors/default_profiles.json
[vk-session-api]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/server/src/routes/sessions/mod.rs
[cones-harness]: ../harness.md
[cones-dashboard]: ../dashboard.md
[vk-server]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/server/src/main.rs
[vk-snapshot]: https://github.com/BloopAI/vibe-kanban/commit/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a
[vk-launcher]: https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/npx-cli/src/cli.ts
