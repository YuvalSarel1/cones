# cones and T3 Code

[All comparisons](README.md)

**Choose T3 Code** for a common conversation interface across harnesses, integrated
code review and access from another computer or phone. Its server owns projects,
thread history and provider-session bindings; native agent runtimes execute the
work. T3 also supplies runtime instructions and translates approvals and questions
into its interface. [Architecture][t3-architecture],
[runtime instructions][t3-instructions], [permissions][t3-permissions]

T3 can create worktrees, run the same prompt in parallel threads and capture Git
checkpoints. The default workspace is the current checkout, so isolation is
optional. Web and desktop include PR review; mobile lacks that diff view.
New threads initially use Full access, configurable by environment or project,
and permission modes have provider-specific semantics.
[Defaults][t3-defaults], [parallel threads][t3-threads],
[review][t3-review], [permissions][t3-permissions]

**Choose cones** for previews, history and navigation around native terminal
sessions across existing projects. T3 onboarding imports recent Claude and Codex
conversations, limited to 30 days and 200 visible messages, omitting tool activity
and attachments. cones continuously discovers supported native sessions and reads
their archives, including pi. Neither discovery nor import establishes live
control of an arbitrary external terminal. [T3 import][t3-import],
[cones support][cones-harness]

cones leaves isolation and review to your tools; conversation forks share files.
Its hosted terminals survive dashboard closure, but reporting has gaps, including
Codex approval waits remaining working. T3 offers remote interaction while its
host is available; mobile push requires T3 Connect and activity publishing.
cones has no corresponding web/mobile client.
[cones dashboard][cones-dashboard], [reporting][cones-harness],
[remote access][t3-remote], [notifications][t3-notifications]

Reviewed 2026-09-22. T3: [f193a68][t3-revision], plus official documentation.
Desktop supports macOS, Windows and Linux; shared control with cones was not
tested. [Platforms][t3-install]. [Review basis](README.md#review-basis).

[t3-architecture]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/internals/overview.md
[t3-instructions]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/apps/server/src/provider/RuntimeInstructions.ts
[t3-permissions]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/permission-modes.md
[t3-defaults]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/packages/contracts/src/t3ProjectFile.ts#L110-L121
[t3-threads]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/thread-sidebar.md
[t3-review]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/source-control.md
[t3-import]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/welcome-wizard.md
[cones-harness]: ../harness.md
[cones-dashboard]: ../dashboard.md
[t3-remote]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/remote-access.md
[t3-notifications]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/mobile-notifications.md
[t3-revision]: https://github.com/pingdotgg/t3code/commit/f193a6863c494fdfa36a1ef3132e42a7d7b42926
[t3-install]: https://github.com/pingdotgg/t3code/blob/f193a6863c494fdfa36a1ef3132e42a7d7b42926/docs/user/install.md
