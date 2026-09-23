# cones and Conductor

[All comparisons](README.md)

**Choose Conductor** when each coding task should have a prepared workspace, a
visible diff and a path to a pull request. Its Mac app organizes branches, chats
and terminals; paid cloud workspaces add shared conversations and work that
continues after your laptop closes. [Workflow][workflow], [cloud][cloud]

Conductor creates Git worktrees and supports setup scripts and copying ignored
files. Inline diff comments send feedback to the agent with line context. This
reduces preparation and review work, although projects still need their dependencies
and configuration. Agents within one workspace share files; separate workspaces
isolate independent changes. [Worktrees][worktrees], [review][diff],
[parallel agents][parallel]

**Choose cones** to browse existing local conversations and return to native
terminal interfaces across projects. It discovers supported sessions started
elsewhere, though external interactive terminals may remain unjoinable. Conductor's
automatic adoption of arbitrary external sessions was not established. cones
leaves worktree creation, code review and merging to your tools; its conversation
forks share the original directory. [cones support][cones-harness],
[Conductor setup][first-workspace], [cones dashboard][cones-dashboard]

Conductor documents that local sessions end when its app closes. Cloud processes
have inactivity and maximum-lifetime limits, while files and chats persist.
cones keeps owned terminals running after dashboard closure, but a host or machine
restart ends them. It has no managed cloud workspace or shared team handoff.
Both offer scheduling: Conductor has paid cloud routines; cones supervises local
Claude Code jobs with a timeout and recorded outcomes, skipping permission prompts.
[Cloud lifetime][cloud-lifecycle], [local lifetime][cloud-faq],
[cones architecture][cones-architecture], [routines][routines], [cones jobs][cones-jobs]

Reviewed 2026-09-22. Conductor: official documentation only; latest listed release
[0.87.3][release]. Combined use was not tested.
[Review basis](README.md#review-basis).

[workflow]: https://www.conductor.build/docs/concepts/workflow
[cloud]: https://www.conductor.build/docs/cloud
[worktrees]: https://www.conductor.build/docs/concepts/git-worktrees
[diff]: https://www.conductor.build/docs/reference/diff-viewer
[parallel]: https://www.conductor.build/docs/concepts/parallel-agents
[cones-harness]: ../harness.md
[first-workspace]: https://www.conductor.build/docs/first-workspace
[cones-dashboard]: ../dashboard.md
[cloud-lifecycle]: https://www.conductor.build/docs/cloud/working-with-cloud-workspaces
[cloud-faq]: https://www.conductor.build/docs/cloud/faq
[cones-architecture]: ../architecture.md
[routines]: https://www.conductor.build/changelog/0.85.0-sections-routines-and-a-new-model-picker
[cones-jobs]: ../jobs.md
[release]: https://www.conductor.build/changelog/0.87.3-gpt-6-sol-and-gpt-6-luna
