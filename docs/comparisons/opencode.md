# cones and OpenCode

[All comparisons](README.md)

**Choose OpenCode** when one coding harness with several model providers, native
subagents and terminal or graphical clients covers your work. OpenCode owns
execution, permissions and session storage. Changing providers retains OpenCode's
agent behavior. Its server API supports shared sessions across native clients.
[Providers][providers], [agents][agents], [server][server]

Its graphical app adds optional worktrees and diff review. Conversation forks
alone still share files. Permission rules support allow, ask and deny, with
mostly permissive defaults. These choices remain OpenCode's when you launch it
through cones. [Worktrees][workspaces], [review][review], [forks][fork],
[permissions][permissions], [cones boundary][cones-architecture]

**Add cones** on macOS when you also use other harnesses and want a shared session
list, history browser and native terminal viewers. OpenCode launch, history,
resume and forks are supported. cones loads a reporting plugin into terminals it
owns to identify the conversation and report activity and input requests. It
changes no tools or permissions, but an explicit TUI configuration requires a
writable directory for its temporary copy. [cones integration][cones-harness],
[reporter][reporter], [configuration][report-config]

The integration does not attach through OpenCode's server. External terminals
have limited identity and no equivalent live reporting; cones cannot join an
arbitrary terminal. Owned terminals survive dashboard closure, with one dashboard
attachment at a time. Native message delivery, context-window reporting and
scheduled OpenCode jobs are unsupported. Use OpenCode's own clients when shared
server access matters more than a view across harnesses.
[Integration limits][cones-harness], [native web clients][web]

Reviewed 2026-09-22. OpenCode: [2406400][opencode-snapshot], plus official docs.
cones documents checks against 1.18.31; this review did not test 1.18.32,
concurrent clients or custom configurations. [Review basis](README.md#review-basis).

[providers]: https://opencode.ai/docs/providers/
[agents]: https://opencode.ai/docs/agents/
[server]: https://opencode.ai/docs/server/
[workspaces]: https://github.com/anomalyco/opencode/blob/2406400f0aeb07b36d0495af4e05aaca49159832/packages/app/src/pages/layout.tsx#L1817-L1858
[review]: https://github.com/anomalyco/opencode/blob/2406400f0aeb07b36d0495af4e05aaca49159832/packages/app/src/pages/session/review-tab.tsx
[fork]: https://github.com/anomalyco/opencode/blob/2406400f0aeb07b36d0495af4e05aaca49159832/packages/opencode/src/session/session.ts#L691-L722
[permissions]: https://opencode.ai/docs/permissions/
[cones-architecture]: ../architecture.md
[cones-harness]: ../harness.md
[reporter]: ../../assets/harnesses/opencode-report.mjs
[report-config]: ../../src/opencode/reporting.rs
[web]: https://opencode.ai/docs/web/
[opencode-snapshot]: https://github.com/anomalyco/opencode/commit/2406400f0aeb07b36d0495af4e05aaca49159832
