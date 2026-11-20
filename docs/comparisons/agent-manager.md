# cones and agent-manager

[All comparisons](README.md)

**Choose agent-manager** to run many agents from one list on its own tmux server,
on macOS, Linux or Windows. One key starts an agent in a group or answers a
blocked one, `ctrl+t` gives a new session its own git worktree, and `ctrl+r`
opens a whole-file diff whose line comments are sent back to the agent. It ships
thirteen CLIs, SSH connections to other machines, and an MCP server through which
agents start and message each other. [Overview][am-docs], [sessions][am-sessions],
[review][am-home]

It lists only the sessions it started; an agent opened in another terminal is not
in its list. Claude Code status comes from hooks; the other CLIs are read from
their pane through per-tool rules, so a screen the rules do not know can be
misread. [Status][am-status], [Claude pickers][am-compare]

**Choose cones** to see every supported session on the Mac wherever it was
started, open it in its native interface, and search past conversations by words
or local meaning. State, context and cost are what each harness reports, and
scheduled jobs run in the same list. agent-manager lists cost tracking as not yet
supported. [cones support][c-harness], [history][c-history], [jobs][c-jobs]

cones leaves worktree creation and code review to your tools, runs only on macOS
and has no remote machines. Both keep sessions running after the dashboard quits:
agent-manager through tmux, cones through native background sessions and its own
terminal host. [Dashboard][c-dashboard], [viewer][c-viewer]

Reviewed 2026-11-20 from the agent-manager.dev documentation, which names v0.48.0.
Its source was not inspected and it was not run. cones: `0ae263f`.
[Review basis](README.md#review-basis).

[am-docs]: https://agent-manager.dev/docs/
[am-sessions]: https://agent-manager.dev/docs/sessions/
[am-home]: https://agent-manager.dev/
[am-status]: https://agent-manager.dev/docs/status/
[am-compare]: https://agent-manager.dev/compare/#claude-code-tmux
[c-harness]: ../harness.md
[c-history]: ../dashboard.md#history
[c-jobs]: ../jobs.md
[c-dashboard]: ../dashboard.md
[c-viewer]: ../dashboard.md#viewer
