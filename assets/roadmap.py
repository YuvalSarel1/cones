"""Generates assets/roadmap.svg. Run: python3 assets/roadmap.py"""
import html, textwrap, pathlib
W = 1000
BG, CARD, LINE, FG, MUTED = "#0d1117", "#161b22", "#30363d", "#e6edf3", "#8b949e"
CAT = {"REL": ("Reliability", "#f0883e"), "OBS": ("Observability", "#d2a8ff"),
       "HAR": ("Harnesses", "#79c0ff"), "COORD": ("Coordination", "#56d4dd")}
COLS = [("NOW", "Make the first release trustworthy", "#3fb950", [
    ("Sleep/wake proof", "REL", "launchd catch-up after lid-close and reboot verified; what replays and what is lost, documented."),
    ("Public release", "REL", "Install steps and verification record, published under the personal account."),
    ("Failure notification", "OBS", "Opt-in macOS notification on failed, crashed or budget-skipped runs."),
    ("Run summary in ls", "OBS", "One line per run: what it did, cost, why it stopped."),
    ("Doctor covers auth", "REL", "cones doctor verifies Claude login and every env var a job imports, so a scheduled run does not fail on missing credentials."),
]), ("NEXT", "More harnesses, less manual follow-up", "#58a6ff", [
    ("Codex jobs", "HAR", "Real token budget, rejected when unenforceable. Same policy file, second harness."),
    ("Next fire time in ls", "OBS", "See what runs next, not only what already ran."),
    ("Retries with backoff", "REL", "Bounded retry for transient failures, chain visible in the ledger."),
    ("Run diffs", "OBS", "The working-tree change a run produced, from the ledger."),
    ("Worktree per run", "COORD", "Concurrent writers each get a worktree; unlocks overlap: allow for write jobs."),
    ("File and webhook triggers", "HAR", "Run on path change or HTTP call, not only on a calendar."),
]), ("LATER", "From scheduler to fleet control", "#bc8cff", [
    ("Fleet roster", "COORD", "cones launches agents with a name, cwd, task and declared file scope; the roster is the source of truth."),
    ("Fleet view", "OBS", "Everything running on the machine: who, where, on what, busy/idle, cost. Side pane or TUI."),
    ("Commit lock and queue", "COORD", "Agents acquire a per-repo lock before committing; cones grants in order. Workers never push or stash."),
    ("Trusted coordinator messages", "COORD", "Coordinator messages carry a per-run token; workers ignore messages without it."),
    ("Status transitions", "OBS", "Started, milestone, blocked, done from the agent itself."),
    ("Standing orders", "COORD", "Fleet rules as data, checked when the last job ends."),
    ("More harnesses, more Macs", "HAR", "Pi when a real use case appears; multi-machine after single-machine fleet control is in regular use."),
])]
SHIPPED = ("Claude jobs on a launchd schedule · dollar budget, timeout, turn cap · read-only or sandboxed-write policy · "
           "rolling daily budget · live event stream · stop, list, resume in Claude's TUI · overlap skip / allow / replace · "
           "shared-workspace writer lock · durable run ledger")
FOOT = ("cones owns the clock, supervision, budgets, locks and ledger. The harness owns execution and permissions. "
        "Unenforceable guarantees are validation errors, never a second permission engine.")

def t(x, y, s, size, fill, **kw):
    attrs = " ".join(f'{k.replace("_", "-")}="{v}"' for k, v in kw.items())
    return f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" {attrs}>{html.escape(s)}</text>'

# Stacked layout: one band per horizon, items in two columns, sized to read at README width.
M, RAIL, GAP = 40, 190, 20
ITEM_W = (W - 2 * M - RAIL - GAP - 24) // 2
WRAP = 50
o = [t(M, 62, "cones roadmap", 34, FG, font_weight=700),
     t(M, 92, "Scheduled coding-agent jobs on your Mac, under explicit policy, with every run accounted for.", 16, MUTED)]
ship = textwrap.wrap(SHIPPED, 118)
o.append(f'<rect x="{M}" y="112" width="{W-2*M}" height="{44+len(ship)*19}" rx="8" fill="{CARD}" stroke="{LINE}"/>')
o.append(t(M + 18, 136, "SHIPPED  v0.1.0-headless", 12, "#3fb950", font_weight=700, letter_spacing=1.5))
for i, l in enumerate(ship):
    o.append(t(M + 18, 158 + i * 19, l, 14, FG))
y = 112 + 44 + len(ship) * 19 + 22
lx = W - M
for k in reversed(list(CAT)):
    name, c = CAT[k]; lx -= len(name) * 7 + 22
    o.append(f'<circle cx="{lx+4}" cy="{y-4}" r="4" fill="{c}"/>' + t(lx + 14, y, name, 12, MUTED)); lx -= 14
o.append(t(M, y, "CATEGORIES", 12, MUTED, letter_spacing=1))
y += 16
for name, sub, color, items in COLS:
    rows = [items[i:i + 2] for i in range(0, len(items), 2)]
    laid = []  # (col, dy, title, cat, lines)
    band_h = 34
    for row in rows:
        row_h = 0
        for ci, (title, cat, desc) in enumerate(row):
            lines = textwrap.wrap(desc, WRAP)
            laid.append((ci, band_h, title, cat, lines))
            row_h = max(row_h, 24 + len(lines) * 19 + 18)
        band_h += row_h
    band_h += 4
    o += [f'<rect x="{M}" y="{y}" width="{W-2*M}" height="{band_h}" rx="10" fill="{CARD}" stroke="{LINE}"/>',
          f'<rect x="{M}" y="{y}" width="6" height="{band_h}" rx="3" fill="{color}"/>',
          t(M + 26, y + 44, name, 24, color, font_weight=800, letter_spacing=2)]
    for i, l in enumerate(textwrap.wrap(sub, 20)):
        o.append(t(M + 26, y + 70 + i * 17, l, 13, MUTED))
    for ci, dy, title, cat, lines in laid:
        cname, cc = CAT[cat]
        x = M + RAIL + GAP + ci * (ITEM_W + 24)
        yy = y + dy
        o += [f'<circle cx="{x}" cy="{yy-6}" r="4" fill="{cc}"/>',
              t(x + 14, yy, title, 17, FG, font_weight=700),
              t(x + 14 + len(title) * 9.1 + 10, yy - 1, cname.upper(), 10.5, cc, letter_spacing=1)]
        for j, l in enumerate(lines):
            o.append(t(x + 14, yy + 22 + j * 19, l, 14.5, MUTED))
    y += band_h + 16
for i, l in enumerate(textwrap.wrap(FOOT, 118)):
    o.append(t(M, y + 12 + i * 17, l, 12.5, MUTED))
H = y + 12 + len(textwrap.wrap(FOOT, 118)) * 17 + 24
o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
     'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
     f'<rect width="{W}" height="{H}" fill="{BG}"/>'] + o + ["</svg>"]
pathlib.Path(__file__).with_name("roadmap.svg").write_text("\n".join(o))
