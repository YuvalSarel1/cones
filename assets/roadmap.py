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
SHIPPED = ("Claude jobs on a launchd schedule · dollar budget, timeout, turn cap · read-only or sandboxed-write policy · "
           "rolling daily budget · live event stream · native dashboard: live refresh, animated cone, details pane, dispatch prompt, group by state or directory · one-off tasks with cones run --prompt · stop, list, resume in Claude's TUI · overlap skip / allow / replace · "
           "shared-workspace writer lock · cones lock around any command, git commit included · opt-in failure notification · global Claude fleet hook: one state file per session · "
           "fleet view: every Claude session in ls and the dashboard grouped by directory with title, state, age, tokens and last message · "
           "stop for fleet sessions from the CLI and the dashboard · key hints follow claude agents, ctrl-x twice stops · per-harness marks, spinners and colors · "
           "enter opens a live session in this terminal through claude attach, follows a running run, resumes a finished one; cones logs takes a session id · details pane shows the last prompt and full reply · durable run ledger · agent-console stub, config.toml and Pi jobs dropped with the three dependencies only they used · doctor fails on missing login or job env, warns on Claude version drift, probes the flags the compiler emits · run rows and cones ls show dollars spent from the ledger · claude agents --json and the job state file feed the fleet view, last column reads Claude's own status · cones coordinator start: one start-orchestrator session per folder, the skill ships inside the binary as a per-session plugin")
FOOT = ("cones owns the clock, supervision, budgets, locks and ledger. The harness owns execution and permissions. "
        "Unenforceable guarantees are validation errors, never a second permission engine.")

def t(x, y, s, size, fill, **kw):
    attrs = " ".join(f'{k.replace("_", "-")}="{v}"' for k, v in kw.items())
    return f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" {attrs}>{html.escape(s)}</text>'

# Matrix layout: horizons are columns, categories are rows.
M, RAIL, GAP, PAD = 28, 0, 12, 14
COL_W = (W - 2 * M - 16 - 2 * GAP) // 3
WRAP = 29
o = [t(M, 62, "cones roadmap", 32, FG, font_weight=700),
     t(M, 92, "Scheduled coding-agent jobs on your Mac, under explicit policy, with every run accounted for.", 16, MUTED)]
ship = textwrap.wrap(SHIPPED, 96)
o.append(f'<rect x="{M}" y="112" width="{W-2*M}" height="{46+len(ship)*22}" rx="8" fill="{CARD}" stroke="{LINE}"/>')
o.append(t(M + 18, 136, "SHIPPED  v0.1.0", 12, "#3fb950", font_weight=700, letter_spacing=1.5))
for i, l in enumerate(ship):
    o.append(t(M + 16, 160 + i * 22, l, 15, FG))
y = 112 + 46 + len(ship) * 22 + 26
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
