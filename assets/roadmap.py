"""Generates assets/roadmap.svg. Run: python3 assets/roadmap.py"""
import html, textwrap, pathlib
W, H = 1600, 860
BG, CARD, LINE, FG, MUTED = "#0d1117", "#161b22", "#30363d", "#e6edf3", "#8b949e"
CAT = {"REL": ("Reliability", "#f0883e"), "OBS": ("Observability", "#d2a8ff"),
       "HAR": ("Harnesses & triggers", "#79c0ff"), "COORD": ("Coordination", "#56d4dd")}
COLS = [("NOW", "Make the first release trustworthy", "#3fb950", [
    ("Sleep/wake proof", "REL", "launchd catch-up after lid-close and reboot verified; what replays and what is lost, documented."),
    ("Public release", "REL", "Install steps and verification record, published under the personal account."),
    ("Failure notification", "OBS", "Opt-in macOS notification on failed, crashed or budget-skipped runs."),
    ("Run summary in ls", "OBS", "One line per run: what it did, cost, why it stopped."),
    ("Doctor covers auth", "REL", "Claude login and named env vars checked before the 2 AM run fails silently."),
]), ("NEXT", "More harnesses, less babysitting", "#58a6ff", [
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
    ("Trusted coordinator messages", "COORD", "A per-run token lets workers tell the coordinator from a stray peer."),
    ("Status transitions", "OBS", "Started, milestone, blocked, done from the agent itself."),
    ("Standing orders", "COORD", "Fleet rules as data, checked when the last job ends."),
    ("More harnesses, more Macs", "HAR", "Pi when a real use case appears; multi-machine after single-machine fleet control is daily-driven."),
])]
SHIPPED = ("Claude jobs on a launchd schedule · dollar budget, timeout, turn cap · read-only or sandboxed-write policy · "
           "rolling daily budget · live event stream · stop, list, resume in Claude's TUI · overlap skip / allow / replace · "
           "shared-workspace writer lock · durable run ledger")
FOOT = ("cones owns the clock, supervision, budgets, locks and ledger. The harness owns execution and permissions. "
        "Unenforceable guarantees are validation errors, never a second permission engine.")

def t(x, y, s, size, fill, **kw):
    attrs = " ".join(f'{k.replace("_", "-")}="{v}"' for k, v in kw.items())
    return f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" {attrs}>{html.escape(s)}</text>'

o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
     'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">',
     f'<rect width="{W}" height="{H}" fill="{BG}"/>',
     t(60, 74, "cones roadmap", 40, FG, font_weight=700),
     t(60, 108, "Scheduled coding-agent jobs on your Mac, under explicit policy, with every run accounted for.", 19, MUTED),
     f'<rect x="60" y="132" width="{W-120}" height="84" rx="8" fill="{CARD}" stroke="{LINE}"/>',
     t(80, 158, "SHIPPED  v0.1.0-headless", 12, "#3fb950", font_weight=700, letter_spacing=1.5)]
for i, l in enumerate(textwrap.wrap(SHIPPED, 160)[:2]):
    o.append(t(80, 182 + i * 20, l, 14, FG))
lx = W - 60
for k in reversed(list(CAT)):
    name, c = CAT[k]; lx -= len(name) * 7.2 + 22
    o.append(f'<circle cx="{lx+4}" cy="240" r="4" fill="{c}"/>' + t(lx + 14, 244, name, 12, MUTED)); lx -= 14
o.append(t(60, 244, "CATEGORIES", 12, MUTED, letter_spacing=1))
cw, top = (W - 120 - 48) // 3, 262
for ci, (name, sub, color, items) in enumerate(COLS):
    x = 60 + ci * (cw + 24)
    o += [f'<rect x="{x}" y="{top}" width="{cw}" height="{H-top-48}" rx="10" fill="{CARD}" stroke="{LINE}"/>',
          f'<rect x="{x}" y="{top}" width="{cw}" height="6" rx="3" fill="{color}"/>',
          t(x + 22, top + 42, name, 22, color, font_weight=800, letter_spacing=2),
          t(x + 22, top + 66, sub, 14, MUTED)]
    yy = top + 102
    for title, cat, desc in items:
        cname, cc = CAT[cat]
        o += [f'<circle cx="{x+28}" cy="{yy-5}" r="4" fill="{cc}"/>',
              t(x + 44, yy, title, 16, FG, font_weight=700),
              t(x + 44 + len(title) * 8.6 + 12, yy - 1, cname.upper(), 10.5, cc, letter_spacing=1)]
        yy += 21
        for l in textwrap.wrap(desc, 58):
            o.append(t(x + 44, yy, l, 13.5, MUTED)); yy += 18
        yy += 13
o += [t(60, H - 18, FOOT, 13, MUTED), "</svg>"]
pathlib.Path(__file__).with_name("roadmap.svg").write_text("\n".join(o))
