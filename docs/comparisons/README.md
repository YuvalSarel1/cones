# Choosing an agent workspace

Short comparisons of where each tool fits, what it asks you to adopt, and what
cones leaves to other tools.

**Workspaces:** [Agent Deck](agent-deck.md), [cmux](cmux.md),
[Conductor](conductor.md), [herdr](herdr.md), [Superset](superset.md),
[T3 Code](t3code.md), [Vibe Kanban](vibe-kanban.md).

**Harnesses:** [Hermes Agent](hermes.md), [OpenCode](opencode.md), [pi](pi.md).
These supply the agent itself. Their comparisons explain whether adding cones
is useful and supported.

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
