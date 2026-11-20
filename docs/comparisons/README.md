# Choosing an agent workspace

Short comparisons of where each tool fits, what it asks you to adopt, and what
cones leaves to other tools.

## Where each one fits

Workspaces:

- [agent-manager](agent-manager.md): a tmux list that starts, answers and reviews its own agents.
- [Agent Deck](agent-deck.md): tmux terminals with worktrees, sandboxes and SSH.
- [cmux](cmux.md): a Mac app of terminals, browser and diff review.
- [Conductor](conductor.md): a workspace, diff and pull request per task.
- [herdr](herdr.md): persistent terminal layouts, across SSH machines.
- [Superset](superset.md): agent terminals beside diffs, editor and previews.
- [T3 Code](t3code.md): one conversation interface, reachable from a phone.
- [Vibe Kanban](vibe-kanban.md): isolated workspaces with inline review.
- cones: every supported session on the Mac, wherever it started, with history.

Harnesses supply the agent itself; their comparisons explain whether adding
cones is useful and supported:

- [Hermes Agent](hermes.md): a continuing assistant with memory and messaging.
- [OpenCode](opencode.md): one harness across model providers.
- [pi](pi.md): a harness you customize with extensions.

Not in cones yet: worktree creation, code review, Linux and remote machines.
The comparisons above name who has each.

## Review basis

Initial reviews on **2026-09-22** through documentation and source inspection, with
Conductor assessed from documentation only. No products were run for these
reviews. Performance, recovery and cross-product interoperability were not tested.
Source snapshots and website documentation may differ from released versions.
Each article records its sources. Later article dates and source snapshots
supersede this shared baseline.

The cones base was `bcdd076b538c54f8a7e4a5cf2b6ede337b94bc63`, unpublished at
review time. Relative cones links follow the reader's checkout. That base requires
macOS; discovery, state reporting and control vary by harness. See the
[harness support guide](../harness.md) for those boundaries.

## Writing comparisons

Aim for **200 to 300 words per article**, excluding link definitions. Lead with
who should choose each tool. Keep only the differences that could change that
choice, with primary sources beside claims and a dated source snapshot.

Give alternatives their strongest case and name cones' material gaps. Distinguish
defaults from optional features, documented behavior from tested behavior, and
missing support from unknowns. Explain combined use for harnesses. Avoid repeating
the verdict, inventorying every feature, or adding a fixed set of sections.
