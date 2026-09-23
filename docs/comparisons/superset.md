# cones and Superset

[All comparisons](README.md)

**Choose Superset** to keep parallel agent terminals beside diffs, an editor,
browser previews and pull requests. It supplies worktree creation and project
lifecycle scripts, while allowing existing worktrees and local checkouts.
Worktree per task is optional. Remote access and mobile clients extend that
workspace through paid features. [Workspace model][s-model],
[imports][s-workspaces], [remote access][s-remote], [mobile][s-mobile]

**Choose cones** for previews, history and navigation across supported native
sessions, including work started elsewhere. Superset imports directories and
worktrees, but its status hooks monitor terminals within its own lifecycle.
cones discovers external sessions without registering them; control remains
partial, and some require their original terminal. [Superset hooks][s-hooks],
[cones support][c-harness]

Superset connects staging, PR feedback and browser checks to the workspace.
Its setup scripts help prepare isolated changes, though agents start alongside
setup by default unless an experimental setting changes that. cones leaves
worktree preparation and review to other tools; its conversation forks share
files. Both preserve hosted terminals after closing their interfaces, but cones
allows one dashboard attachment per owned host.
[Review][s-diff], [browser][s-browser], [setup][s-lifecycle],
[daemon][s-daemon], [cones dashboard][c-dashboard]

Scheduling has a consequential difference: Superset's paid automations record
successful dispatch, which does not establish that the agent finished. Offline
hosts fail that occurrence. cones supervises only local Claude Code jobs, with
overlap handling, a timeout and recorded native outcomes. Those jobs skip
permission prompts; their reported success still does not establish code
correctness. [Automations][s-automations], [cones jobs][c-jobs]

Reviewed 2026-09-22. Superset: [0b0d79f][s-snapshot], plus official documentation.
macOS and experimental Linux support; combined use was not tested.
[Installation][s-install]. [Review basis](README.md#review-basis).

[s-model]: https://github.com/superset-sh/superset/blob/0b0d79ff41ec456349e891ecfe56e63880ed26ca/apps/docs/content/docs/superset-model.mdx
[s-workspaces]: https://docs.superset.sh/workspaces
[s-remote]: https://docs.superset.sh/remote-access
[s-mobile]: https://superset.sh/mobile
[s-hooks]: https://github.com/superset-sh/superset/blob/0b0d79ff41ec456349e891ecfe56e63880ed26ca/packages/agent-setup/templates/notify-hook.template.sh
[c-harness]: ../harness.md
[s-diff]: https://docs.superset.sh/diff-viewer
[s-browser]: https://docs.superset.sh/browser
[s-lifecycle]: https://docs.superset.sh/setup-teardown-scripts
[s-daemon]: https://github.com/superset-sh/superset/blob/0b0d79ff41ec456349e891ecfe56e63880ed26ca/packages/host-service/DAEMON_SUPERVISION.md
[c-dashboard]: ../dashboard.md
[s-automations]: https://docs.superset.sh/automations
[c-jobs]: ../jobs.md
[s-snapshot]: https://github.com/superset-sh/superset/tree/0b0d79ff41ec456349e891ecfe56e63880ed26ca
[s-install]: https://docs.superset.sh/install
