"""Generates assets/roadmap.svg. Run: python3 assets/roadmap.py"""
import html, textwrap, pathlib
W = 1200
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
    ("Coordinator token", "COORD", "Coordinator messages carry a per-run token; workers ignore messages without it."),
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

# Matrix layout: horizons are columns, categories are rows.
M, RAIL, GAP, PAD = 40, 150, 14, 16
COL_W = (W - 2 * M - RAIL - 2 * GAP) // 3
WRAP = 36
o = [t(M, 62, "cones roadmap", 38, FG, font_weight=700),
     t(M, 92, "Scheduled coding-agent jobs on your Mac, under explicit policy, with every run accounted for.", 18, MUTED)]
ship = textwrap.wrap(SHIPPED, 130)
o.append(f'<rect x="{M}" y="112" width="{W-2*M}" height="{46+len(ship)*22}" rx="8" fill="{CARD}" stroke="{LINE}"/>')
o.append(t(M + 18, 136, "SHIPPED  v0.1.0-headless", 12, "#3fb950", font_weight=700, letter_spacing=1.5))
for i, l in enumerate(ship):
    o.append(t(M + 18, 160 + i * 22, l, 16, FG))
y = 112 + 46 + len(ship) * 22 + 28
# column headers
for ci, (name, sub, color, _) in enumerate(COLS):
    x = M + RAIL + ci * (COL_W + GAP)
    o += [f'<rect x="{x}" y="{y}" width="{COL_W}" height="6" rx="3" fill="{color}"/>',
          t(x + PAD, y + 38, name, 24, color, font_weight=800, letter_spacing=2),
          t(x + PAD, y + 60, sub, 13.5, MUTED)]
y += 76
def cell(items):
    laid, h = [], 0
    for title, desc in items:
        lines = textwrap.wrap(desc, WRAP)
        laid.append((h, title, lines)); h += 26 + len(lines) * 20 + 14
    return laid, h
for key, (cname, cc) in CAT.items():
    cells = [cell([(t_, d) for t_, c, d in items if c == key]) for _, _, _, items in COLS]
    row_h = max(h for _, h in cells) + 2 * PAD - 6
    o.append(f'<rect x="{M}" y="{y}" width="{W-2*M}" height="{row_h}" rx="10" fill="{CARD}" stroke="{LINE}"/>')
    o.append(f'<rect x="{M}" y="{y+12}" width="5" height="{row_h-24}" rx="2.5" fill="{cc}"/>')
    for i, l in enumerate(textwrap.wrap(cname, 16)):
        o.append(t(M + 20, y + PAD + 18 + i * 20, l, 16, cc, font_weight=700))
    for ci, (laid, h) in enumerate(cells):
        x = M + RAIL + ci * (COL_W + GAP)
        if not laid:
            o.append(t(x + PAD, y + PAD + 18, "—", 16, LINE)); continue
        for dy, title, lines in laid:
            yy = y + PAD + 18 + dy
            o.append(t(x + PAD, yy, title, 17, FG, font_weight=700))
            for j, l in enumerate(lines):
                o.append(t(x + PAD, yy + 22 + j * 20, l, 14.5, MUTED))
    y += row_h + 12
foot = textwrap.wrap(FOOT, 130)
for i, l in enumerate(foot):
    o.append(t(M, y + 14 + i * 19, l, 14, MUTED))
H = y + 14 + len(foot) * 19 + 24
o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
     'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
     f'<rect width="{W}" height="{H}" fill="{BG}"/>'] + o + ["</svg>"]
pathlib.Path(__file__).with_name("roadmap.svg").write_text("\n".join(o))
