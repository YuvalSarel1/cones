"""Generates assets/roadmap.svg. Run: python3 assets/roadmap.py"""
# Owner rulings on this roadmap, given while questioning "Lock holder in ls":
# cones does not meddle in what jobs do. Two jobs that write one directory both run; a collision
# is the coordinator skill's business or nobody's. Per-job `overlap` (skip, replace, allow) is the
# only overlap policy cones holds, plus a wanted fourth mode, continue: stop run 1, start run 2
# with `claude --resume` on run 1's session id.
# Applied to this roadmap on 2026-09-13 by the owner's direction: "Lock holder in ls", "Other
# coordinators" and "Cross-harness coordination" are gone; COORD is out of CAT; "Worktree per run"
# is REL; "Touched files" and "Status transitions" are OBS; "overlap: continue" is in NEXT. The
# writer lock is out of the code: no runner flock, no `workspace` skip reason, no `cones lock`,
# no validate rule against overlap: allow with write: true; the internal ledger and admission
# locks stay. Undecided: moving the embedded skill back to its own repo, installed by coordinator start.
# 2026-09-14, owner: "Status transitions" is gone, an agent reporting into cones is coordinator
# messaging; "Touched files" is LATER, its details pane is gone and run diffs cover cones' own runs;
# "Jump to pane" is NEXT, since own terminal rows now refuse to open and this is the missing half of
# moving between agents.
# Roster, participants, gating, knowledge transfer and messaging belong to the coordinator skill;
# hooks mean hooks on jobs cones launches, never instrumentation of sessions it did not start.
import html, textwrap, pathlib
W = 860  # GitHub README column width, so text renders 1:1
BG, CARD, LINE, FG, MUTED = "#0d1117", "#161b22", "#30363d", "#e6edf3", "#8b949e"
CAT = {"OBS": ("See", "#d2a8ff"), "CTL": ("Move and control", "#56d4dd"),
       "HAR": ("Harnesses", "#79c0ff"), "REL": ("Reliability", "#f0883e")}
COLS = [("NOW", "One dashboard, every harness", "#3fb950", [
    ("Public release", "REL", "Version, install steps and verification record are in; tag v0.1.0 and publish under the personal account. Owner action, no code left."),
]), ("NEXT", "Everyday control of jobs", "#58a6ff", [
    ("overlap: continue", "REL", "A tick that finds the previous run still going stops it and starts the new run with claude --resume on run 1's session id, so run 2 keeps what run 1 learned."),
    ("Job lifecycle hooks", "CTL", "Opt-in commands on start, exit and failure for jobs cones launches, set in jobs.yaml. Nothing hooks sessions cones did not start."),
    ("Next fire time in ls", "OBS", "Each job row shows its next tick, computed from the compiled StartCalendarInterval list and confirmed against the loaded plist."),
    ("Jump to pane", "CTL", "Enter on an own terminal row resolves pid to tty to tmux or iTerm pane and switches the user there; the session is never joined or stopped."),
    ("Run diffs", "OBS", "A write run records git diff --stat of its cwd at exit; cones logs and the details pane show what the run changed."),
    ("Sleep/wake proof", "REL", "One slept-through tick fires one run on wake, none after a reboot past one; observed in cones ls --json and written into the README."),
]), ("LATER", "Needs a second harness or Mac", "#bc8cff", [
    ("Codex budget probe", "HAR", "Measure Codex usage events to decide whether a token budget can be enforced; the result gates Codex jobs, not Codex in the fleet."),
    ("Codex jobs", "HAR", "Real token budget, rejected when unenforceable. Same policy file, second harness."),
    ("Fleet session records", "OBS", "A session cones did not launch gets a ledger record when it leaves Claude's registry: cwd, duration, tokens, dollars as last reported. ls totals it, daily_budget_usd ignores it."),
    ("Touched files", "OBS", "Edit and Write paths read from the session's transcript; ls --json shows each footprint. Bash edits are not seen."),
    ("Retries with backoff", "REL", "Bounded retry for transient failures, chain visible in the ledger."),
    ("Worktree per run", "REL", "Concurrent writers each get a worktree. Recipe from claude-squad, ported as git commands. Only if shared directories prove insufficient."),
    ("File triggers", "HAR", "Run on path change via launchd WatchPaths; a few plist lines, no watcher process."),
    ("Webhook triggers", "HAR", "Run on an HTTP call. Needs a listener process, so after single-machine fleet control."),
    ("More Macs", "HAR", "Multi-machine after single-machine fleet control is in regular use."),
])]
SHIPPED = ["launchd schedule, no daemon between ticks", "dollar budget, timeout, turn cap", "rolling daily budget",
           "read-only or sandboxed-write policy", "overlap skip / allow / replace", "one-off runs: cones run --prompt",
           "opt-in failure notification", "durable JSONL run ledger", "dollars per run in ls and the ledger", "live event stream",
           "fleet from Claude's own session registry, no hook", "every Claude session in ls and the TUI",
           "Codex sessions: process table, rollout file, app-server threads joined and left like Claude",
           "model, start, activity and context read from the transcript, never estimated", "context window as the harness states it",
           "stop and attach from CLI and TUI", "viewers on a private pty; the shell never shows between transitions",
           "own terminal rows: a session cones cannot join says so, no stolen tty",
           "dashboard: grouping, filter, a harness's own agents view", "menu: folder, jobs, config, help",
           "composer: an instruction starts a session in the selected folder, any harness", "image paste into the composer",
           "a jobs screen behind the menu: start, add, edit and delete jobs there", "attach returns to the row; ctrl+x stops",
           "ctrl+x removes a session with claude rm, hides a finished run", "states, keys and hints follow claude agents",
           "per-harness marks, spinners, colors", "doctor: login, job env, version drift, flags", "cones coordinator start, skill in the binary",
           "coordinator: two-way channel to Codex agents",
           "viewers live inside the dashboard; leaving and returning is a focus change",
           "a fleet strip under a viewer; ctrl+] cycles live viewers",
           "a session's viewer opens while the cursor rests on its row; enter is instant",
           "the selected session shows live beside the list on a wide terminal"]
FOOT = ("cones owns the clock, supervision, budgets and the ledger. The harness owns execution and permissions. "
        "Unenforceable guarantees are validation errors, never a second permission engine. "
        "Seeing a harness's sessions never waits on enforcing its budgets.")

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
