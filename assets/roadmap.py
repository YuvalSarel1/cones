"""Generates assets/roadmap.svg. Run: python3 assets/roadmap.py"""
import html, textwrap, pathlib
W = 860  # GitHub README column width, so text renders 1:1
BG, CARD, LINE, FG, MUTED = "#0d1117", "#161b22", "#30363d", "#e6edf3", "#8b949e"
CAT = {"REL": ("Reliability", "#f0883e"), "OBS": ("Observability", "#d2a8ff"),
       "HAR": ("Harnesses", "#79c0ff"), "COORD": ("Coordination", "#56d4dd")}
COLS = [("NOW", "What the coordinator needs from cones", "#3fb950", [
    ("Lock holder in ls", "COORD", "Holder pid and session id land beside the .lock; ls, the dashboard and cones lock --status show who holds and who waits; --try exits 1, no waiting."),
    ("Touched files from hooks", "OBS", "PostToolUse Edit and Write paths accumulate in the session's fleet file; ls --json and the details pane show each footprint. Bash edits are not seen."),
    ("Status transitions", "OBS", "cones status 'text' writes the session's fleet file and fills the last column, so the orchestrator reads check-ins instead of asking; hook-derived blocked/idle/exited shipped."),
    ("Public release", "REL", "Version, install steps and verification record are in; tag v0.1.0 and publish under the personal account. Owner action, no code left."),
]), ("NEXT", "Claude only, no second writer needed", "#58a6ff", [
    ("Sleep/wake proof", "REL", "One slept-through tick fires one run on wake, none after a reboot past one; observed in cones ls --json and written into the README."),
    ("Next fire time in ls", "OBS", "Each job row shows its next tick, computed from the compiled StartCalendarInterval list and confirmed against the loaded plist."),
    ("Run diffs", "OBS", "A write run records git diff --stat of its cwd at exit; cones logs and the details pane show what the run changed."),
    ("Retries with backoff", "REL", "Bounded retry for transient failures, chain visible in the ledger."),
    ("File triggers", "HAR", "Run on path change via launchd WatchPaths; a few plist lines, no watcher process."),
    ("Other coordinators", "COORD", "cones coordinator start --skill NAME launches any coordinator skill, Claude Code or another harness; start-orchestrator stays the default."),
    ("Fleet sessions in the ledger", "OBS", "SessionEnd writes a session record (cwd, duration, tokens, dollars) for sessions cones did not launch; ls totals it, daily_budget_usd ignores it."),
]), ("LATER", "Needs a second writer, harness or Mac", "#bc8cff", [
    ("Worktree per run", "COORD", "Concurrent writers each get a worktree; unlocks overlap: allow for write jobs. Recipe from claude-squad, ported as git commands."),
    ("Cross-harness coordination", "COORD", "The coordinator drives Claude Code and Codex agents in one folder alike: Codex arrivals greeted, gated and relayed, not only read from their rollout files."),
    ("Codex budget probe", "HAR", "Measure Codex usage events to decide whether a token budget can be enforced; the result gates the Codex adapter."),
    ("Codex jobs", "HAR", "Real token budget, rejected when unenforceable. Same policy file, second harness."),
    ("Codex fleet hooks", "HAR", "cones hook codex, once Codex hook trust can be configured without a bypass flag."),
    ("Webhook triggers", "HAR", "Run on an HTTP call. Needs a listener process, so after single-machine fleet control."),
    ("More Macs", "HAR", "Multi-machine after single-machine fleet control is in regular use."),
    ("Jump to pane", "OBS", "Enter on a foreign session resolves pid to tty to tmux or iTerm pane and switches there, instead of resuming a copy."),
])]
SHIPPED = ["launchd schedule, no daemon between ticks", "dollar budget, timeout, turn cap", "rolling daily budget",
           "read-only or sandboxed-write policy", "overlap skip / allow / replace", "one-off runs: cones run --prompt",
           "shared-workspace writer lock", "cones lock around any command", "opt-in failure notification",
           "durable JSONL run ledger", "dollars per run in ls and the ledger", "live event stream",
           "global fleet hook, one file per session", "every Claude session in ls and the TUI", "stop and attach from CLI and TUI",
           "fleet status from claude agents --json", "dashboard: details pane, dispatch, grouping", "key hints follow claude agents",
           "per-harness marks, spinners, colors", "doctor: login, job env, version drift, flags", "cones coordinator start, skill in the binary"]
FOOT = ("cones owns the clock, supervision, budgets, locks and ledger. The harness owns execution and permissions. "
        "Unenforceable guarantees are validation errors, never a second permission engine.")

def t(x, y, s, size, fill, **kw):
    attrs = " ".join(f'{k.replace("_", "-")}="{v}"' for k, v in kw.items())
    return f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" {attrs}>{html.escape(s)}</text>'

# Matrix layout: horizons are columns, categories are rows.
M, RAIL, GAP, PAD = 28, 0, 12, 14
COL_W = (W - 2 * M - 16 - 2 * GAP) // 3
WRAP = 29
o = []
# SHIPPED: three columns of bullets, filled column-wise
per = -(-len(SHIPPED) // 3)
cols = [[(j > 0, l) for it in SHIPPED[c*per:(c+1)*per] for j, l in enumerate(textwrap.wrap(it, 30))] for c in range(3)]
ship_h = max(map(len, cols)) * 20
o.append(f'<rect x="{M}" y="16" width="{W-2*M}" height="{50+ship_h}" rx="8" fill="{CARD}" stroke="{LINE}"/>')
o.append(t(M + 18, 40, "SHIPPED  v0.1.0", 12, "#3fb950", font_weight=700, letter_spacing=1.5))
for ci, lines in enumerate(cols):
    x = M + 8 + ci * (COL_W + GAP)
    for i, (cont, l) in enumerate(lines):
        o.append(t(x + PAD + 12 if cont else x + PAD, 64 + i * 20, l if cont else "\u2022 " + l, 13.5, FG))
y = 16 + 50 + ship_h + 26
# column headers
for ci, (name, sub, color, _) in enumerate(COLS):
    x = M + 8 + ci * (COL_W + GAP)
    o += [f'<rect x="{x}" y="{y}" width="{COL_W}" height="6" rx="3" fill="{color}"/>',
          t(x + PAD, y + 38, name, 22, color, font_weight=800, letter_spacing=1.5),
          t(x + PAD, y + 58, sub, 12.5, MUTED) if len(sub) <= 40 else t(x + PAD, y + 58, sub, 11.5, MUTED)]
y += 74
def cell(items):
    laid, h = [], 0
    for title, desc in items:
        lines = textwrap.wrap(desc, WRAP)
        laid.append((h, title, lines)); h += 28 + len(lines) * 22 + 16
    return laid, h
for key, (cname, cc) in CAT.items():
    cells = [cell([(t_, d) for t_, c, d in items if c == key]) for _, _, _, items in COLS]
    row_h = max(h for _, h in cells) + 2 * PAD + 24
    o.append(f'<rect x="{M}" y="{y}" width="{W-2*M}" height="{row_h}" rx="10" fill="{CARD}" stroke="{LINE}"/>')
    o.append(f'<rect x="{M}" y="{y+12}" width="5" height="{row_h-24}" rx="2.5" fill="{cc}"/>')
    o.append(t(M + 18, y + PAD + 16, cname.upper(), 12.5, cc, font_weight=700, letter_spacing=1.5))
    y0 = y; y += 30
    for ci, (laid, h) in enumerate(cells):
        x = M + 8 + ci * (COL_W + GAP)
        if not laid:
            o.append(t(x + PAD, y + PAD + 18, "—", 16, LINE)); continue
        for dy, title, lines in laid:
            yy = y + PAD + 18 + dy
            o.append(t(x + PAD, yy, title, 17, FG, font_weight=700))
            for j, l in enumerate(lines):
                o.append(t(x + PAD, yy + 24 + j * 22, l, 16, MUTED))
    y = y0 + row_h + 12
foot = textwrap.wrap(FOOT, 100)
for i, l in enumerate(foot):
    o.append(t(M, y + 14 + i * 20, l, 14, MUTED))
H = y + 14 + len(foot) * 20 + 24
o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
     'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
     f'<rect width="{W}" height="{H}" fill="{BG}"/>'] + o + ["</svg>"]
pathlib.Path(__file__).with_name("roadmap.svg").write_text("\n".join(o))
