//! `cones tui` is the native dashboard: jobs, every live harness session grouped by directory
//! or by state, and runs, with a composer at the bottom like `claude agents`: type an
//! instruction, `enter` starts a session in the selected row's directory under the harness
//! `tab` picked. Jobs are added, edited and deleted here too (`ctrl+n`, `ctrl+e`, `ctrl+x`);
//! `ctrl+x` marks the row red and a second press acts; any other key keeps it. On a finished
//! run it hides the row here for good; the ledger keeps it.
//! ratatui draws; cones supplies rows. `cones __list` prints the same rows as tab-separated text.
//! Run statuses and session states go through the same match arms (`active`, `idle`, `blocked`,
//! `exited` are session states); a run status must not reuse those words or its rows sort and
//! draw as sessions.
use crate::{
    codex,
    config::{self, HarnessKind, ResolvedJob},
    fleet::{self, Session},
    harness::{self, Start},
    launchd,
    ledger::{Ledger, Run},
    output, runner,
};
use anyhow::{Context, Result};
use ratatui::{
    DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{OnceLock, mpsc},
    time::{Duration, Instant},
};

/// The tty as the shell handed it over, read once at start. Crossterm snapshots the tty at
/// every raw-mode entry and hands that snapshot back on exit; after a child, the snapshot is
/// whatever the child left, so a child that died raw would reach the shell through it.
static SHELL_TTY: OnceLock<Option<libc::termios>> = OnceLock::new();

/// Put the tty back the way the shell had it, after ratatui has left raw mode and the
/// alternate screen: the line discipline from `SHELL_TTY`, and off with every mode a child
/// turns on and may not have turned off: mouse reports, focus events, bracketed paste,
/// colour-scheme reports, kitty keys and modifyOtherKeys. Claude Code enables all of these
/// and disables them only on its own way out; a viewer killed on ctrl-z, or a child that
/// crashed, leaves them on, and a shell with them on echoes garbage on every click, focus
/// change and paste. Invisible on a terminal where nothing was left on.
fn hand_back_tty() {
    use std::io::Write;
    if let Some(Some(t)) = SHELL_TTY.get() {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[<u\x1b[>4m\x1b(B\x1b[0m\x1b[?25h",
    );
    let _ = out.flush();
}

const ORANGE: Color = Color::Indexed(208);
/// The header cone's lit and shadow sides, one hue either side of ORANGE.
const LIT: Color = Color::Indexed(214);
const SHADE: Color = Color::Indexed(202);
const SPINNER: [&str; 4] = ["▲", "◭", "▲", "◮"];
/// Claude Code's own working animation: its star grows then shrinks.
const CLAUDE_SPINNER: [&str; 12] = ["·", "✢", "✳", "✶", "✻", "✽", "✽", "✻", "✶", "✳", "✢", "·"];
/// Codex and pi both spin braille dots.
const DOTS_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, PartialEq, Eq)]
pub enum Kind {
    Header,
    /// Column names under a section title, dim and unselectable.
    Columns,
    Blank,
    Job(String),
    /// Session id and state.
    Session(String, String),
    /// Run id and status.
    Run(String, String),
}

impl Kind {
    fn selectable(&self) -> bool {
        !matches!(self, Kind::Header | Kind::Columns | Kind::Blank)
    }

    /// What names a row across reloads: the job name, the session id or the run id. The state
    /// is left out on purpose, so a session that went from idle to working while it was open
    /// is still the same row when the dashboard comes back.
    pub fn key(&self) -> Option<&str> {
        match self {
            Kind::Job(name) => Some(name),
            Kind::Session(id, _) | Kind::Run(id, _) => Some(id),
            _ => None,
        }
    }
}

pub struct Row {
    pub kind: Kind,
    pub cells: Vec<(String, Style)>,
}

impl Row {
    fn text(&self) -> String {
        self.cells.iter().map(|(t, _)| t.as_str()).collect()
    }
    fn working(&self) -> bool {
        matches!(&self.kind, Kind::Session(_, s) | Kind::Run(_, s) if s == "active" || s == "started")
    }
}

/// Everything the dashboard shows, loaded in one pass.
pub struct Data {
    pub jobs: Vec<ResolvedJob>,
    pub runs: Vec<Run>,
    pub sessions: Vec<Session>,
    /// Session column names after the harness and title, from jobs.yaml.
    pub columns: Vec<String>,
}

impl Data {
    pub fn load(jobs_path: &Path, state: &Path, claude: &Path) -> Result<Self> {
        let ledger = Ledger::new(state)?;
        let hidden = ledger.hidden()?;
        let mut runs = ledger.runs()?;
        runs.retain(|r| !hidden.contains(&r.started.run_id));
        let sessions = fleet_rows(claude, state, &runs)?;
        Ok(Self {
            jobs: config::read_jobs(jobs_path).unwrap_or_default(),
            runs,
            sessions,
            columns: config::columns(jobs_path),
        })
    }

    fn count(&self, state: &str) -> usize {
        self.sessions.iter().filter(|s| s.state == state).count()
    }

    /// The fleet in one line: a cone glyph and count per state, each in the state's color, then
    /// the jobs and runs. A count of zero goes dim so the live numbers stand out.
    pub fn summary(&self) -> Line<'static> {
        let sep = || Span::styled("  ", plain());
        let mut spans = Vec::new();
        for (state, title) in [
            ("active", "working"),
            ("blocked", "need input"),
            ("idle", "idle"),
            ("done", "done"),
        ] {
            let n = self.count(state);
            let style = if n == 0 { dim() } else { color(state) };
            spans.push(Span::styled(format!("{} ", icon(state)), style));
            spans.push(Span::styled(
                n.to_string(),
                style.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" {title}"), style));
            spans.push(sep());
        }
        spans.push(Span::styled("·  ", dim()));
        for (n, title) in [(self.jobs.len(), "jobs"), (self.runs.len(), "runs")] {
            let style = if n == 0 { dim() } else { plain() };
            spans.push(Span::styled(
                n.to_string(),
                style.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" {title}"), style));
            spans.push(sep());
        }
        spans.pop();
        Line::from(spans)
    }

    /// Sessions grouped by directory like Claude's own agents view, or by state so the row that
    /// needs a human is on top.
    pub fn rows(&self, by_state: bool) -> Vec<Row> {
        let mut out = Vec::new();
        let header = |out: &mut Vec<Row>, title: &str| {
            out.push(Row {
                kind: Kind::Blank,
                cells: vec![],
            });
            out.push(Row {
                kind: Kind::Header,
                cells: vec![(title.to_owned(), bold())],
            });
        };
        if !self.jobs.is_empty() {
            header(&mut out, "jobs");
            let cells = self
                .jobs
                .iter()
                .map(|j| {
                    let last = self
                        .runs
                        .iter()
                        .rev()
                        .find(|r| r.started.job.as_deref() == Some(&j.name))
                        .map_or("-".to_owned(), |r| r.status());
                    vec![
                        (if j.enabled { "◆" } else { "◇" }.into(), color(&last)),
                        (j.name.clone(), plain()),
                        (j.schedule.clone(), dim()),
                        (logo(&j.harness.to_string()), brand(&j.harness.to_string())),
                        (if j.enabled { "on" } else { "off" }.into(), plain()),
                        (last.clone(), color(&last)),
                    ]
                })
                .collect();
            let (names, cells) =
                columns(&["", "job", "schedule", "", "enabled", "last run"], cells);
            out.push(names);
            for (j, cells) in self.jobs.iter().zip(cells) {
                out.push(Row {
                    kind: Kind::Job(j.name.clone()),
                    cells,
                });
            }
        }
        let mut groups: BTreeMap<String, Vec<&Session>> = BTreeMap::new();
        for s in &self.sessions {
            let key = if by_state {
                // A rank digit orders the groups (needs input first); it is stripped for display.
                let rank = match s.state.as_str() {
                    "blocked" => 1,
                    "active" => 2,
                    "idle" => 3,
                    _ => 4,
                };
                format!("{rank}{}", label(&s.state))
            } else {
                fleet::tilde(&s.cwd)
            };
            groups.entry(key).or_default().push(s);
        }
        // One table across all groups, so columns line up between directories.
        let flat: Vec<(&String, &&Session)> = groups
            .iter()
            .flat_map(|(key, group)| group.iter().map(move |s| (key, s)))
            .collect();
        let cells = flat
            .iter()
            .map(|(_, s)| {
                let mut row = vec![
                    (icon(&s.state).into(), color(&s.state)),
                    (logo(&s.harness), brand(&s.harness)),
                    // The same words as the footer, on the row, so a session that cannot be
                    // joined from here is known before it is selected.
                    (
                        if s.own_terminal() { "own terminal" } else { "" }.into(),
                        dim(),
                    ),
                    (
                        // A long title would push every metric column off a 120-column screen.
                        clip(
                            &s.title
                                .clone()
                                .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
                            40,
                        ),
                        plain(),
                    ),
                ];
                row.extend(self.columns.iter().map(|c| cell(c, s, by_state)));
                row
            })
            .collect();
        let mut names = vec!["", "", "", "title"];
        names.extend(self.columns.iter().map(|c| match c.as_str() {
            "tokens" => "tokens in/out",
            "last" if by_state => "dir",
            c => c,
        }));
        let (names, cells) = columns(&names, cells);
        if !flat.is_empty() {
            out.push(Row {
                kind: Kind::Blank,
                cells: vec![],
            });
            out.push(names);
        }
        let mut current: Option<&String> = None;
        for ((key, s), cells) in flat.iter().zip(cells) {
            if current != Some(key) {
                if current.is_none() {
                    out.push(Row {
                        kind: Kind::Header,
                        cells: vec![((if by_state { &key[1..] } else { key }).to_owned(), bold())],
                    });
                } else {
                    header(&mut out, if by_state { &key[1..] } else { key });
                }
                current = Some(key);
            }
            out.push(Row {
                kind: Kind::Session(s.session_id.clone(), s.state.clone()),
                cells,
            });
        }
        if !self.runs.is_empty() {
            header(&mut out, "runs");
            // ponytail: the newest 200 runs; paging when the ledger outgrows a screenful of scrolling.
            let runs: Vec<&Run> = self.runs.iter().rev().take(200).collect();
            let cells = runs
                .iter()
                .map(|r| {
                    let last = r.terminal.as_ref().unwrap_or(&r.started);
                    let status = r.status();
                    vec![
                        (icon(&status).into(), color(&status)),
                        (r.started.job.clone().unwrap_or_else(|| "-".into()), plain()),
                        (status.clone(), color(&status)),
                        (
                            r.started
                                .fired_at
                                .map(|t| t.format("%m-%d %H:%M:%S").to_string())
                                .unwrap_or_default(),
                            dim(),
                        ),
                        (
                            last.duration_s
                                .map(|d| format!("{d:.0}s"))
                                .unwrap_or_default(),
                            dim(),
                        ),
                        (last.cost_usd.map(fleet::cost).unwrap_or_default(), dim()),
                        (last.reason.clone().unwrap_or_default(), dim()),
                    ]
                })
                .collect();
            let (names, cells) = columns(
                &["", "job", "status", "started", "took", "cost", "reason"],
                cells,
            );
            out.push(names);
            for (r, cells) in runs.iter().zip(cells) {
                out.push(Row {
                    kind: Kind::Run(r.started.run_id.clone(), r.status()),
                    cells,
                });
            }
        }
        out
    }

    /// The details pane for one row: a job's policy and prompt, a session's last `exchanges`
    /// prompts and replies from its transcript, or a run's captured output.
    pub fn details(&self, kind: &Kind, exchanges: usize) -> Vec<String> {
        match kind {
            Kind::Job(name) => {
                let Some(j) = self.jobs.iter().find(|j| &j.name == name) else {
                    return vec![];
                };
                let mut out = vec![
                    format!(
                        "{} · {} · {}",
                        j.schedule,
                        logo(&j.harness.to_string()),
                        fleet::tilde(&j.cwd)
                    ),
                    format!(
                        "timeout {:.0}m · budget ${:.2} · write {} · overlap {:?} · tools {}",
                        j.timeout_min,
                        j.budget_usd,
                        if j.write { "yes" } else { "no" },
                        j.overlap,
                        j.tools.join(",")
                    ),
                    String::new(),
                ];
                out.extend(j.prompt.lines().map(str::to_owned));
                out
            }
            Kind::Session(id, _) => {
                let Some(s) = self.sessions.iter().find(|s| &s.session_id == id) else {
                    return vec![];
                };
                // Model, start, last activity and context are the transcript's words, `-` when
                // it has none; the harness reports no window, so the context has no denominator.
                let stamp = |t: Option<chrono::DateTime<chrono::Utc>>| {
                    t.map_or_else(|| "-".into(), |t| t.format("%m-%d %H:%M:%S").to_string())
                };
                let mut out = vec![
                    fleet::tilde(&s.cwd),
                    format!(
                        "{} · {} {} · {} · started {} · last activity {} · {} context · {} tokens · pid {} · {}",
                        logo(&s.harness),
                        label(&s.state),
                        s.kind.as_deref().unwrap_or(""),
                        s.model.as_deref().unwrap_or("-"),
                        stamp(s.started),
                        stamp(s.last_activity),
                        fleet::context(s),
                        fleet::tokens(s),
                        s.pid.map(|p| p.to_string()).unwrap_or_default(),
                        s.session_id
                    ),
                    String::new(),
                ];
                if let Some(t) = &s.transcript_path {
                    out.extend(fleet::exchanges(t, exchanges));
                }
                out
            }
            Kind::Run(id, _) => {
                let Some(r) = self.runs.iter().find(|r| &r.started.run_id == id) else {
                    return vec![];
                };
                let last = r.terminal.as_ref().unwrap_or(&r.started);
                let mut out = vec![
                    format!(
                        "{} · {} · {} · {}",
                        r.started.job.as_deref().unwrap_or("-"),
                        r.status(),
                        last.reason.as_deref().unwrap_or(""),
                        r.started.run_id
                    ),
                    String::new(),
                ];
                out.extend(output::snapshot(
                    r.started.output.as_deref(),
                    r.started.stderr.as_deref(),
                ));
                out
            }
            _ => vec![],
        }
    }
}

/// Tab-separated rows for scripts and tests: hidden key (`job`, `hdr`, session or run UUID),
/// hidden aux (job name, session state or run status), then the display text with ANSI color.
/// Three header lines carry the pixel cone, the summary and the keys.
pub fn list(jobs_path: &Path, state: &Path, claude: &Path) -> Result<String> {
    let data = Data::load(jobs_path, state, claude)?;
    let mut out = String::new();
    for line in header_lines(data.summary()) {
        out += "hdr\t-\t";
        for span in line.spans {
            out += &ansi(&span.content, span.style);
        }
        out.push('\n');
    }
    for row in data.rows(false) {
        let (key, aux) = match &row.kind {
            Kind::Header | Kind::Columns | Kind::Blank => ("hdr".to_owned(), "-".to_owned()),
            Kind::Job(n) => ("job".to_owned(), n.clone()),
            Kind::Session(id, s) | Kind::Run(id, s) => (id.clone(), s.clone()),
        };
        out += &format!("{key}\t{aux}\t");
        if row.kind.selectable() {
            out += "  ";
        }
        for (text, style) in &row.cells {
            out += &ansi(text, *style);
        }
        out.push('\n');
    }
    Ok(out)
}

fn ansi(text: &str, style: Style) -> String {
    if text.trim().is_empty() {
        return text.to_owned();
    }
    let mut codes = Vec::new();
    if style.add_modifier.contains(Modifier::BOLD) {
        codes.push("1".to_owned());
    }
    if style.add_modifier.contains(Modifier::DIM) {
        codes.push("2".to_owned());
    }
    match style.fg {
        Some(Color::Green) => codes.push("32".into()),
        Some(Color::Yellow) => codes.push("33".into()),
        Some(Color::Red) => codes.push("31".into()),
        Some(Color::White) => codes.push("97".into()),
        Some(Color::Indexed(n)) => codes.push(format!("38;5;{n}")),
        _ => {}
    }
    if codes.is_empty() {
        text.to_owned()
    } else {
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

/// What `enter` does to the selected row: start a job, follow a headless run, attach a session.
fn enter_verb(kind: Option<&Kind>) -> &'static str {
    match kind {
        Some(Kind::Job(_)) => "start job",
        Some(Kind::Run(_, s)) if s == "started" => "follow log",
        Some(Kind::Session(..) | Kind::Run(..)) => "attach",
        _ => "open",
    }
}

/// The header cone: one orange hue in three tones, lit on the left, shadowed on the right, so
/// it reads as a solid rather than a flat triangle. Static and foreground-only: a blinking beacon
/// and background-filled bands were tried and rejected as too busy for a dashboard header.
fn cone() -> [Vec<Span<'static>>; 3] {
    let tone = |s: &'static str, c: Color| Span::styled(s, Style::default().fg(c));
    [
        vec![tone("  ▲  ", ORANGE)],
        vec![tone(" ▟", LIT), tone("█", ORANGE), tone("▙ ", SHADE)],
        vec![tone("▟█", LIT), tone("█", ORANGE), tone("█▙", SHADE)],
    ]
}

/// The three header lines: the cone, with the fleet summary beside its bands. The keys are on
/// the bottom line, under the composer, as in `claude agents`. Two callers: `draw` and the
/// `--tsv` path in `list`.
fn header_lines(summary: Line<'static>) -> Vec<Line<'static>> {
    let [top, mut middle, base] = cone();
    middle.push(Span::raw("  "));
    middle.extend(summary.spans);
    vec![Line::from(top), Line::from(middle), Line::from(base)]
}

/// Key hints: each key lit and its verb dim, so the eye finds the key first.
fn hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (i, (key, verb)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::styled((*key).to_owned(), bold()));
        spans.push(Span::styled(format!(" {verb}"), dim()));
    }
    Line::from(spans)
}

/// `text` without its ANSI color sequences, for a status line.
fn uncolored(text: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in text.chars() {
        match (esc, c) {
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, '\x1b') => esc = true,
            (false, c) => out.push(c),
        }
    }
    out
}

/// Pad each column to its widest cell, two spaces apart.
fn table(rows: Vec<Vec<(String, Style)>>) -> Vec<Vec<(String, Style)>> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|(t, _)| t.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.into_iter()
        .map(|r| {
            let n = r.len();
            r.into_iter()
                .enumerate()
                .map(|(c, (text, style))| {
                    let pad = if c + 1 == n {
                        0
                    } else {
                        widths[c] - text.chars().count() + 2
                    };
                    (format!("{text}{:pad$}", ""), style)
                })
                .collect()
        })
        .collect()
}

/// One configurable session cell; `last` shows the directory when rows are grouped by state,
/// since the group title no longer names it. `model`, `age`, `activity` and `context` are the
/// transcript's own words and read `-` until it has them.
fn cell(column: &str, s: &Session, by_state: bool) -> (String, Style) {
    let since = |t: Option<chrono::DateTime<chrono::Utc>>| t.map_or_else(|| "-".into(), fleet::age);
    match column {
        "state" => (label(&s.state).into(), color(&s.state)),
        "model" => (s.model.clone().unwrap_or_else(|| "-".into()), dim()),
        "age" => (since(s.started), dim()),
        "activity" => (since(s.last_activity), dim()),
        "context" => (fleet::context(s), dim()),
        "tokens" => (fleet::tokens(s), dim()),
        "last" if by_state => (fleet::tilde(&s.cwd), dim()),
        "last" => (
            s.last.as_deref().map(|l| clip(l, 100)).unwrap_or_default(),
            dim(),
        ),
        _ => ("?".into(), dim()),
    }
}

/// Column names as a dim row padded together with the table beneath it, indented past the cursor
/// gutter so each name sits over its column.
fn columns(names: &[&str], rows: Vec<Vec<(String, Style)>>) -> (Row, Vec<Vec<(String, Style)>>) {
    let mut all = vec![names.iter().map(|n| ((*n).to_owned(), dim())).collect()];
    all.extend(rows);
    let mut all = table(all);
    let mut cells = all.remove(0);
    cells[0].0.insert_str(0, "  ");
    (
        Row {
            kind: Kind::Columns,
            cells,
        },
        all,
    )
}

fn plain() -> Style {
    Style::default()
}
fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// What is typed with a block cursor after it, or the placeholder with the cursor on its first
/// letter: how Claude Code draws its own input.
fn typed(value: &str, placeholder: &str) -> Vec<Span<'static>> {
    let cursor = Modifier::REVERSED;
    if !value.is_empty() {
        return vec![
            Span::raw(value.to_owned()),
            Span::styled(" ", Style::default().add_modifier(cursor)),
        ];
    }
    let mut rest = placeholder.chars();
    let first = rest.next().map_or(" ".to_owned(), |c| c.to_string());
    vec![
        Span::styled(first, dim().add_modifier(cursor)),
        Span::styled(rest.as_str().to_owned(), dim()),
    ]
}

/// One glyph per state, cone-shaped where it can be: a solid cone is busy, a hollow one is
/// resting, a warning cone wants a human. `-` is a session whose harness reported no state, a
/// Codex before its first turn; it is not a failure.
fn icon(state: &str) -> &str {
    match state {
        "active" | "started" => "▲",
        "blocked" => "⚠",
        "idle" => "△",
        "exited" | "stopped" => "▵",
        "ok" | "done" => "✓",
        "skipped" | "-" => "–",
        _ => "✗",
    }
}

/// Which harness a session or job runs under, written the way each app writes itself: Codex's
/// `>_` startup box title, Claude's and pi's plain word (Claude's ✻ already spins in the state
/// column while it works; π is only pi's window title).
fn logo(harness: &str) -> String {
    match harness {
        "codex" => ">_ codex".into(),
        other => other.to_owned(),
    }
}

/// Each harness in the color it paints itself: Claude's orange, pi's teal accent; Codex has none.
fn brand(harness: &str) -> Style {
    match harness {
        "claude" => Style::default().fg(Color::Rgb(215, 119, 87)),
        "pi" => Style::default().fg(Color::Rgb(138, 190, 183)),
        _ => dim(),
    }
}

/// The working animation and color a row's harness would draw for itself. A cones run keeps the
/// flipping cone.
// ponytail: the harness is read back from the logo cell rather than carried on Row.
fn spinner(row: &Row) -> (&'static [&'static str], Option<Style>) {
    let mark = row.cells.get(1).map(|c| c.0.trim()).unwrap_or("");
    for h in ["claude", "codex", "pi"] {
        if mark == logo(h) {
            let frames: &'static [&'static str] = if h == "claude" {
                &CLAUDE_SPINNER
            } else {
                &DOTS_SPINNER
            };
            return (frames, Some(brand(h)));
        }
    }
    (&SPINNER, None)
}

fn label(state: &str) -> &str {
    match state {
        "active" => "working",
        "blocked" => "needs input",
        s => s,
    }
}

fn color(status: &str) -> Style {
    match status {
        // Claude's own agents view: working is plain, green is for finished work.
        "active" | "started" => plain(),
        "ok" | "done" => Style::default().fg(Color::Green),
        "blocked" | "skipped" => Style::default().fg(Color::Yellow),
        "idle" | "exited" | "stopped" | "-" => dim(),
        _ => Style::default().fg(Color::Red),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// Sessions from Claude's registry and Codex's process table, oldest first. Sessions belonging
/// to a ledger run collapse into that run's row.
pub fn fleet_rows(claude: &Path, state: &Path, runs: &[Run]) -> Result<Vec<Session>> {
    let owned: HashSet<&str> = runs
        .iter()
        .filter_map(|r| r.started.session_id.as_deref())
        .collect();
    let mut out: Vec<Session> = fleet::all(claude)?
        .into_iter()
        .filter(|s| !owned.contains(s.session_id.as_str()))
        .collect();
    // Codex threads the dashboard launched behind the daemon show nothing in the process
    // table while no client is attached; cones lists them from its own record.
    out.extend(codex::thread_rows(&codex::home(claude), state, &out));
    Ok(out)
}

/// The directory a launch runs in: `text` with `~` expanded and a relative path taken from
/// `base`, canonical, and an existing directory. Empty text means `fallback`, the row's cwd or
/// the dashboard's own. The error is the one line the prompt shows inline.
pub fn launch_dir(text: &str, base: &Path, fallback: &Path) -> Result<PathBuf, String> {
    let text = text.trim();
    let path = if text.is_empty() {
        fallback.to_path_buf()
    } else {
        crate::expand_path(Path::new(text), base).map_err(|e| e.to_string())?
    };
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    path.canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Where the job wizard is: each step is one question on the prompt line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Name,
    Dir,
    Schedule,
    Prompt,
}

/// What a key in the job wizard asks the dashboard to do.
#[derive(Debug, PartialEq)]
pub enum FormAction {
    Stay,
    Cancel,
    /// Write the job, replacing the one with this name when editing.
    Save(Option<String>, Box<config::Job>),
}

/// A row of options with the picked one lit and bracketed, then the keys that move and the
/// verb `enter` performs.
fn choices(spans: &mut Vec<Span<'static>>, options: &[&str], picked: usize, enter: &str) {
    let lit = Style::default().fg(ORANGE).add_modifier(Modifier::BOLD);
    for (i, o) in options.iter().enumerate() {
        spans.push(Span::styled(
            format!(
                "{}{o}{}",
                if i == picked { "[" } else { " " },
                if i == picked { "]" } else { " " }
            ),
            if i == picked { lit } else { dim() },
        ));
    }
    spans.push(Span::styled(
        format!("  ←→ pick · enter {enter} · esc cancel"),
        dim(),
    ));
}

/// The job wizard (`ctrl+n` adds, `ctrl+e` on a job row edits): a name, a directory, a
/// five-field cron schedule and the prompt. `enter` answers a question, `esc` cancels,
/// backspace on an empty answer steps back. Editing keeps every field the wizard does not ask
/// about (model, budget, tools). Pure: filesystem facts come in through `base`, `fallback` and
/// `launch_dir`; the file is written by the dashboard on `Save`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobForm {
    pub step: Step,
    pub name: String,
    pub dir: String,
    pub schedule: String,
    pub prompt: String,
    pub error: Option<String>,
    /// The job being edited, as written in the file; `None` adds one.
    original: Option<config::Job>,
    base: PathBuf,
    fallback: PathBuf,
}

impl JobForm {
    /// `base` is where a relative directory is taken from, the jobs file's; `fallback` is what
    /// an empty directory means and is shown as the placeholder.
    pub fn new(base: &Path, fallback: &Path, original: Option<config::Job>) -> Self {
        let (name, dir, schedule, prompt) = match &original {
            Some(j) => (
                j.name.clone(),
                j.cwd.display().to_string(),
                j.schedule.clone(),
                j.prompt.clone(),
            ),
            None => Default::default(),
        };
        Self {
            step: Step::Name,
            name,
            dir,
            schedule,
            prompt,
            error: None,
            original,
            base: base.to_owned(),
            fallback: fallback.to_owned(),
        }
    }

    fn field(&mut self) -> &mut String {
        match self.step {
            Step::Name => &mut self.name,
            Step::Dir => &mut self.dir,
            Step::Schedule => &mut self.schedule,
            Step::Prompt => &mut self.prompt,
        }
    }

    pub fn key(&mut self, code: KeyCode, ctrl: bool) -> FormAction {
        if code == KeyCode::Esc {
            return FormAction::Cancel;
        }
        self.error = None;
        match code {
            KeyCode::Enter => return self.next(),
            KeyCode::Backspace if self.field().is_empty() => {
                self.step = match self.step {
                    Step::Name | Step::Dir => Step::Name,
                    Step::Schedule => Step::Dir,
                    Step::Prompt => Step::Schedule,
                }
            }
            KeyCode::Backspace => {
                self.field().pop();
            }
            KeyCode::Char(c) if !ctrl => self.field().push(c),
            _ => {}
        }
        FormAction::Stay
    }

    /// Check the answer; move on, or at the last question hand the job over. The checks are the
    /// file's own, so what passes here passes `cones install`.
    fn next(&mut self) -> FormAction {
        match self.step {
            Step::Name => {
                let ok = !self.name.is_empty()
                    && self.name.len() <= 80
                    && self
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                if ok {
                    self.step = Step::Dir;
                } else {
                    self.error = Some("1-80 letters, digits, - or _".into());
                }
            }
            Step::Dir => match launch_dir(&self.dir, &self.base, &self.fallback) {
                Ok(dir) => {
                    self.dir = fleet::tilde(&dir);
                    self.step = Step::Schedule;
                }
                Err(e) => self.error = Some(e),
            },
            Step::Schedule => match launchd::calendar_intervals(&self.schedule) {
                Ok(_) => self.step = Step::Prompt,
                Err(e) => self.error = Some(format!("{e:#}")),
            },
            Step::Prompt => {
                if self.prompt.trim().is_empty() {
                    self.error = Some("the prompt is the task; it cannot be empty".into());
                    return FormAction::Stay;
                }
                let mut job = self.original.clone().unwrap_or_else(|| {
                    config::Job::new(&self.name, &self.schedule, Path::new(&self.dir), "")
                });
                job.name = self.name.clone();
                job.cwd = PathBuf::from(&self.dir);
                job.schedule = self.schedule.clone();
                job.prompt = self.prompt.trim().to_owned();
                return FormAction::Save(
                    self.original.as_ref().map(|j| j.name.clone()),
                    Box::new(job),
                );
            }
        }
        FormAction::Stay
    }

    /// The prompt line: what is asked, the answer so far or a placeholder, the inline error.
    fn line(&self) -> Line<'static> {
        let ask = Style::default().fg(ORANGE);
        let title = match &self.original {
            Some(j) => format!("edit {}", j.name),
            None => "new job".to_owned(),
        };
        let fallback = fleet::tilde(&self.fallback);
        let (what, value, hint) = match self.step {
            Step::Name => ("name", &self.name, "letters, digits, - or _"),
            Step::Dir => ("dir", &self.dir, fallback.as_str()),
            Step::Schedule => (
                "schedule",
                &self.schedule,
                "minute hour day month weekday, as in 0 9 * * 1-5",
            ),
            Step::Prompt => ("prompt", &self.prompt, "the task"),
        };
        let mut spans = vec![Span::styled(format!("{title} · {what} › "), ask)];
        spans.extend(typed(value, hint));
        if let Some(e) = &self.error {
            spans.push(Span::styled(
                format!("  {e}"),
                Style::default().fg(Color::Red),
            ));
        }
        Line::from(spans)
    }
}

enum Mode {
    Normal,
    Filter,
    Job(Box<JobForm>),
    /// The `ctrl+o` prompt: which harness's own agents view to open; an index into `harness::KNOWN`.
    Harness(usize),
}

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
    claude: PathBuf,
    /// The dashboard's own working directory: where a launch goes with nothing selected.
    cwd: PathBuf,
    data: Data,
    rows: Vec<Row>,
    /// Indexes into `rows` that pass the filter; the cursor indexes this list.
    visible: Vec<usize>,
    cursor: usize,
    scroll: usize,
    by_state: bool,
    filter: String,
    mode: Mode,
    status: String,
    /// The composer: the instruction a session in the selected row's directory starts with.
    text: String,
    /// The harness the next session starts under; `tab` cycles it. An index into `harness::KNOWN`.
    harness: usize,
    /// A `claude --bg` in flight on its own thread; its one line lands in the status.
    started: Option<mpsc::Receiver<String>>,
    tick: usize,
    refreshed: Instant,
    /// A reload in flight on its own thread; the loop applies it when it lands, so a slow read
    /// never holds the spinner or a keypress.
    loading: Option<mpsc::Receiver<Result<Data>>>,
    loading_started: Option<Instant>,
    /// A transition happened after the current read started. Discard that read and run again.
    reload_pending: bool,
    /// Harness commands can take seconds. Keep input and drawing alive while they finish.
    stopping: Vec<PendingStop>,
    /// Successful delete/forget commands take effect here before the registry catches up.
    removed_sessions: HashSet<String>,
    /// Only transitions and slow frames are timed, so idle drawing does not fill the log.
    feedback: Option<(&'static str, Instant)>,
    /// The row key ctrl+x armed; stays until ctrl+x confirms or any other key clears it.
    armed: Option<String>,
    /// `cones tui --debug`: every terminal hand-off and input event is appended here.
    log: Option<PathBuf>,
}

struct PendingStop {
    id: String,
    verb: &'static str,
    result: mpsc::Receiver<Result<bool>>,
}

impl PendingStop {
    fn message(&self) -> String {
        let action = match self.verb {
            "delete" => "deleting",
            "forget" => "forgetting",
            _ => "stopping",
        };
        format!("{action} {}", self.id.chars().take(8).collect::<String>())
    }
}

enum Waited {
    Exited(std::process::ExitStatus),
    Stopped,
}

impl App {
    fn new(exe: &Path, jobs_path: &Path, state: &Path, claude: &Path) -> Result<Self> {
        Ok(Self {
            exe: exe.to_owned(),
            jobs_path: jobs_path.to_owned(),
            state: state.to_owned(),
            claude: claude.to_owned(),
            cwd: std::env::current_dir().context("dashboard working directory")?,
            data: Data::load(jobs_path, state, claude)?,
            rows: vec![],
            visible: vec![],
            cursor: 0,
            scroll: 0,
            by_state: false,
            filter: String::new(),
            mode: Mode::Normal,
            status: String::new(),
            text: String::new(),
            harness: 0,
            started: None,
            tick: 0,
            refreshed: Instant::now(),
            loading: None,
            loading_started: None,
            reload_pending: false,
            stopping: Vec::new(),
            removed_sessions: HashSet::new(),
            feedback: None,
            armed: None,
            log: None,
        })
    }

    /// Append one timestamped line to the debug log, if `--debug` named one.
    fn debug(&self, msg: impl FnOnce() -> String) {
        if let Some(path) = &self.log {
            debug_line(path, msg());
        }
    }

    fn timing(&self, phase: &str, started: Instant) {
        self.debug(|| {
            format!(
                "timing {phase} ms={:.3}",
                started.elapsed().as_secs_f64() * 1000.0
            )
        });
    }

    fn selected(&self) -> Option<&Row> {
        self.visible.get(self.cursor).map(|&i| &self.rows[i])
    }

    /// Reload and stay on the selected row, found again by its key: a session whose state
    /// changed is still the same row, a row that is gone leaves the cursor at its position, on
    /// the neighbor. The filter and the grouping are fields, so a reload never touches them.
    #[cfg(test)]
    fn refresh(&mut self) -> Result<()> {
        let data = Data::load(&self.jobs_path, &self.state, &self.claude)?;
        self.apply(data);
        Ok(())
    }

    /// Start a reload on a thread unless one is already running; `poll` lands it.
    fn reload(&mut self) {
        if self.loading.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (jobs, state, claude) = (
            self.jobs_path.clone(),
            self.state.clone(),
            self.claude.clone(),
        );
        let started = Instant::now();
        let log = self.log.clone();
        self.debug(|| "refresh started".into());
        std::thread::spawn(move || {
            let data = Data::load(&jobs, &state, &claude);
            if let Some(log) = log {
                debug_line(
                    &log,
                    format!(
                        "timing refresh_read ms={:.3}",
                        started.elapsed().as_secs_f64() * 1000.0
                    ),
                );
            }
            let _ = tx.send(data);
        });
        self.loading = Some(rx);
        self.loading_started = Some(started);
    }

    /// A command or terminal hand-off invalidates any read already in flight. Keep one reader,
    /// but do not let its old snapshot delay a fresh read by another polling interval.
    fn invalidate(&mut self) {
        if self.loading.is_some() {
            self.reload_pending = true;
        } else {
            self.reload();
        }
    }

    /// Apply a finished reload, if one has landed. A failed read shows in the status line and
    /// the last good data stays on screen.
    fn poll(&mut self) {
        self.poll_stops();
        if let Some(rx) = &self.started
            && let Ok(msg) = rx.try_recv()
        {
            self.status = msg;
            self.started = None;
            self.invalidate();
        }
        let Some(rx) = &self.loading else {
            return;
        };
        let result = rx.try_recv();
        if matches!(result, Err(mpsc::TryRecvError::Empty)) {
            return;
        }
        self.loading = None;
        if std::mem::take(&mut self.reload_pending) {
            if let Some(started) = self.loading_started.take() {
                self.timing("refresh_discard", started);
            }
            self.reload();
            return;
        }
        if let Some(started) = self.loading_started.take() {
            self.timing("refresh_received", started);
        }
        match result {
            Err(mpsc::TryRecvError::Empty) => {}
            Ok(Ok(data)) => self.apply(data),
            Ok(Err(e)) => {
                self.status = format!("reload failed: {e:#}");
                self.refreshed = Instant::now();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status = "reload failed: worker disconnected".into();
                self.refreshed = Instant::now();
            }
        }
    }

    fn apply(&mut self, mut data: Data) {
        if self.status.starts_with("reload failed:") {
            self.status.clear();
        }
        self.removed_sessions
            .retain(|id| data.sessions.iter().any(|s| &s.session_id == id));
        data.sessions
            .retain(|s| !self.removed_sessions.contains(&s.session_id));
        self.data = data;
        self.rebuild();
        self.refreshed = Instant::now();
    }

    /// Grouping and acknowledged removals only need the data already on screen.
    fn rebuild(&mut self) {
        let started = Instant::now();
        let keep = self
            .selected()
            .and_then(|r| r.kind.key().map(str::to_owned));
        self.rows = self.data.rows(self.by_state);
        self.apply_filter();
        if let Some(k) = keep
            && let Some(i) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].kind.key() == Some(k.as_str()))
        {
            self.cursor = i;
        }
        self.settle();
        self.timing("rebuild", started);
    }

    /// Rows that match the filter, plus the headers that still have something under them.
    /// The keep-a-header-if-followed rule cannot tell a group title from any other unselectable
    /// row, so every unselectable kind except Header is dropped from the match set while a needle
    /// is set; a new unselectable kind needs the same treatment or it hides the title above it.
    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        let rows = &self.rows;
        let matched: Vec<usize> = (0..rows.len())
            .filter(|&i| {
                needle.is_empty()
                    || (!rows[i].kind.selectable() && rows[i].kind != Kind::Columns)
                    || rows[i].text().to_lowercase().contains(&needle)
            })
            .collect();
        if needle.is_empty() {
            self.visible = matched;
            return;
        }
        self.visible = matched
            .iter()
            .enumerate()
            .filter(|&(n, &i)| {
                rows[i].kind.selectable()
                    || (rows[i].kind == Kind::Header
                        && matched
                            .get(n + 1)
                            .is_some_and(|&j| rows[j].kind.selectable()))
            })
            .map(|(_, &i)| i)
            .collect();
    }

    /// Move the cursor onto a selectable row and rebuild the pane.
    fn settle(&mut self) {
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor = self.cursor.min(self.visible.len() - 1);
        let selectable = |i: usize| self.rows[self.visible[i]].kind.selectable();
        if !selectable(self.cursor)
            && let Some(i) = (self.cursor..self.visible.len())
                .chain((0..self.cursor).rev())
                .find(|&i| selectable(i))
        {
            self.cursor = i;
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.visible.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.cursor as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if self.rows[self.visible[i as usize]].kind.selectable() {
                break;
            }
        }
        self.cursor = i as usize;
        self.settle();
    }

    fn me(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("--jobs")
            .arg(&self.jobs_path)
            .arg("--state-dir")
            .arg(&self.state);
        c
    }

    /// The working directory of the selected row: a job's cwd, a session's, a run's.
    fn selected_cwd(&self) -> Option<PathBuf> {
        match &self.selected()?.kind {
            Kind::Job(name) => self
                .data
                .jobs
                .iter()
                .find(|j| &j.name == name)
                .map(|j| j.cwd.clone()),
            Kind::Session(id, _) => self
                .data
                .sessions
                .iter()
                .find(|s| &s.session_id == id)
                .map(|s| s.cwd.clone()),
            Kind::Run(id, _) => self
                .data
                .runs
                .iter()
                .find(|r| &r.started.run_id == id)
                .and_then(|r| r.started.cwd.clone()),
            _ => None,
        }
    }

    /// Start something in the background and forget it; the ledger and fleet files report back.
    /// `dir` is the subprocess's working directory, which is where `cones run --prompt` runs.
    fn spawn(&mut self, args: &[&str], dir: Option<&Path>, what: &str) {
        let mut c = self.me();
        if let Some(dir) = dir {
            c.current_dir(dir);
        }
        let r = c
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        self.status = match r {
            Ok(_) => what.to_owned(),
            Err(e) => format!("{what} failed: {e}"),
        };
    }

    /// Hand the terminal to a child, take it back when it exits, and surface its last stderr line.
    /// Hand the terminal to a child, take it back when it exits, and surface its last stderr
    /// line. The child runs in its own process group and owns the tty, like a shell job, so
    /// ctrl-c and ctrl-z reach it and everything it forked, and nothing else; `on_stop` says
    /// what a stop means.
    fn foreground(&mut self, terminal: &mut DefaultTerminal, mut c: Command, what: &str) {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        let height = terminal.size().map(|s| s.height).unwrap_or(0);
        self.debug(|| {
            format!(
                "foreground {what}: {c:?} in {:?}; size={:?}; {}",
                c.get_current_dir(),
                terminal.size().ok(),
                term_state()
            )
        });
        ratatui::restore();
        self.debug(|| format!("restored; {}", term_state()));
        // The normal screen is blank while the child has the terminal: the child starts over
        // it, and on the way out leaves its own screen to it, so neither gap shows the shell.
        // Not the last frame: `claude attach` execs into `claude agents` on the left key and
        // is seconds on the normal screen while it starts, and a dead dashboard there reads
        // as a live one that ignores keys.
        let mut still = Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .ok();
        if let Some(t) = still.as_mut() {
            let _ = t.clear();
        }
        c.stderr(Stdio::piped());
        // ponytail: ctrl-c must reach only the child; the dashboard ignores it while waiting.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            c.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::tcsetpgrp(0, libc::getpid());
                for sig in [libc::SIGINT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let r = c.spawn().and_then(|mut c| {
            self.debug(|| format!("child pid {}; waiting", c.id()));
            let pid = c.id() as libc::pid_t;
            let stderr = c.stderr.take().map(|mut e| {
                std::thread::spawn(move || {
                    let mut v = Vec::new();
                    let _ = e.read_to_end(&mut v);
                    v
                })
            });
            let watch = self.log.as_ref().map(|p| watch_group(p.clone(), c.id()));
            let status = loop {
                let w = wait_or_stopped(&mut c, true, &|m| self.debug(|| m));
                match w {
                    // The child still owns the tty; it only needs to run again.
                    Ok(Waited::Stopped) => unsafe {
                        libc::kill(-pid, libc::SIGCONT);
                    },
                    Ok(Waited::Exited(st)) => {
                        self.debug(|| format!("child exited after {:?}: {st}", started.elapsed()));
                        break Ok(st);
                    }
                    Err(e) => break Err(e),
                }
            };
            // Dropping the sender ends the watcher at once. Joining a thread that slept a
            // whole second here once left the tty cooked and unread for that long: the
            // dashboard looked frozen and keystrokes echoed onto it.
            if let Some((stop, t)) = watch {
                drop(stop);
                let _ = t.join();
            }
            let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
            status.map(|st| (st, stderr))
        });
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        match &r {
            Ok((st, err)) => self.debug(|| {
                format!(
                    "child done after {:?}: {st}; stderr_tail={:?}; {}",
                    started.elapsed(),
                    String::from_utf8_lossy(err)
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty()),
                    term_state()
                )
            }),
            Err(e) => self.debug(|| format!("child failed: {e}; {}", term_state())),
        }
        if let Some(t) = still.as_mut() {
            let _ = t.clear();
        }
        hand_back_tty();
        *terminal = ratatui::init();
        self.feedback = Some(("return_to_draw", Instant::now()));
        self.debug(|| {
            format!(
                "dashboard back: size={:?}; {}",
                terminal.size().ok(),
                term_state()
            )
        });
        self.status = match r {
            Ok((st, _)) if st.success() => format!("back from {what}"),
            Ok((st, err)) => {
                let err = String::from_utf8_lossy(&err);
                let last = err.lines().rev().find(|l| !l.trim().is_empty());
                match last {
                    Some(l) => format!("{what} failed: {}", l.trim_start_matches("Error: ")),
                    None => format!("{what} exited with {st}"),
                }
            }
            Err(e) => format!("{what} failed: {e}"),
        };
    }

    /// After a Codex client launched here returns: keep the thread it opened, so its row stays
    /// and `enter` resumes it. A thread left before its first turn is gone with the client.
    fn record_codex(&mut self, dir: &Path, since: chrono::DateTime<chrono::Utc>) {
        let home = codex::home(&self.claude);
        match codex::launched(&home, dir, since) {
            Some(t) => {
                let short: String = t.id.chars().take(8).collect();
                self.status = match codex::remember(&self.state, t) {
                    Ok(()) => format!("codex thread {short} kept · enter on its row returns to it"),
                    Err(e) => format!("could not record codex thread {short}: {e}"),
                };
            }
            None if !self.status.contains("failed") => {
                self.status = "codex left before its first turn; nothing to come back to".into()
            }
            None => {}
        }
    }

    /// The footer's `enter` verb for the selected row: a session of a harness that cannot be
    /// joined from here says so instead of promising an attach.
    fn enter_label(&self) -> &'static str {
        let row = self.selected();
        if let Some(Kind::Session(id, _)) = row.map(|r| &r.kind)
            && self
                .data
                .sessions
                .iter()
                .any(|s| &s.session_id == id && s.own_terminal())
        {
            return "own terminal";
        }
        enter_verb(row.map(|r| &r.kind))
    }

    fn enter(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(kind) = self.selected().map(|r| r.kind.clone()) else {
            return Ok(());
        };
        if let Some(action) = self
            .stopping
            .iter()
            .find(|a| Some(a.id.as_str()) == kind.key())
        {
            self.status = action.message();
            return Ok(());
        }
        self.debug(|| format!("enter on {:?}: {}", kind.key(), enter_verb(Some(&kind))));
        match kind {
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("started {name}")),
            // A headless run cannot be attached while it runs; follow its log instead. A live
            // session attaches natively, ctrl-z comes back here.
            Kind::Run(id, s) if s == "started" => {
                let mut c = self.me();
                c.args(["logs", &id, "--follow"]);
                self.foreground(terminal, c, "logs");
                self.invalidate();
            }
            // A listed session is live, so `claude attach` runs straight from here; the
            // `cones attach` helper, which reloads the whole fleet first, is for finished runs.
            Kind::Session(id, _) => {
                let Some(s) = self.data.sessions.iter().find(|s| s.session_id == id) else {
                    return Ok(());
                };
                let (harness, cwd, own_terminal) =
                    (s.harness.clone(), s.cwd.clone(), s.own_terminal());
                // A TUI running in another terminal cannot be joined: a Codex TUI, or an
                // interactive Claude, which `claude attach` does not know.
                if own_terminal {
                    self.status = format!(
                        "{harness} runs in its own terminal and cannot be joined from here"
                    );
                    return Ok(());
                }
                // A Codex thread behind the daemon reopens with a client.
                if harness == "codex" {
                    match harness::codex_resume(&id, &cwd) {
                        Ok(c) => self.foreground(terminal, c, "codex"),
                        Err(e) => self.status = format!("codex resume failed: {e:#}"),
                    }
                    self.invalidate();
                    return Ok(());
                }
                match harness::adapter(HarnessKind::Claude)?.attach(&id, &cwd) {
                    Ok(c) => self.foreground(terminal, c, "attach"),
                    Err(e) => self.status = format!("attach failed: {e:#}"),
                }
                // Back on the same row, read again by id: the session may have changed state,
                // or ended, while it was open. Filter and grouping were never touched.
                self.invalidate();
            }
            Kind::Run(id, _) => {
                let mut c = self.me();
                c.args(["attach", &id]);
                self.foreground(terminal, c, "attach");
                self.invalidate();
            }
            _ => {}
        }
        Ok(())
    }

    /// `cones install`, so launchd matches the file the wizard or ctrl+x just changed.
    fn install(&mut self, done: &str) {
        let r = self.me().arg("install").output();
        self.status = match r {
            Ok(o) if o.status.success() => format!("{done} · launchd reinstalled"),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let last = err.lines().rev().find(|l| !l.trim().is_empty());
                format!(
                    "{done} · install failed: {}",
                    last.unwrap_or("").trim_start_matches("Error: ")
                )
            }
            Err(e) => format!("{done} · install failed: {e}"),
        };
    }

    /// Where the composer starts a session and the wizard's placeholder: the selected row's
    /// directory, else the dashboard's own.
    fn target_dir(&self) -> PathBuf {
        self.selected_cwd().unwrap_or_else(|| self.cwd.clone())
    }

    /// Where a job's relative `cwd` is taken from: the jobs file's directory.
    fn jobs_dir(&self) -> PathBuf {
        std::fs::canonicalize(&self.jobs_path)
            .ok()
            .and_then(|p| p.parent().map(Path::to_owned))
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// The composer's `enter`: a session in the selected row's directory with the text as its
    /// first instruction, under the harness `tab` picked. Claude starts in the background on a
    /// thread and its row appears when Claude lists it; Codex opens here and Ctrl+Z leaves it.
    fn start(&mut self, terminal: &mut DefaultTerminal) {
        let dir = self.target_dir();
        let kind = harness::KNOWN[self.harness];
        let prompt = std::mem::take(&mut self.text);
        let what = format!("{kind} in {}", fleet::tilde(&dir));
        // Rollout timestamps are the thread's own clock; a little slack covers it.
        let since = chrono::Utc::now() - chrono::Duration::seconds(5);
        self.debug(|| format!("start {what}: {prompt:?}"));
        match harness::start(kind, &dir, prompt.trim()) {
            Ok(Start::Background(mut c)) => {
                let (tx, rx) = mpsc::channel();
                self.status = format!("starting {what}");
                std::thread::spawn(move || {
                    let msg = match c.stdin(Stdio::null()).output() {
                        Ok(o) if o.status.success() => format!(
                            "started {what}: {}",
                            uncolored(String::from_utf8_lossy(&o.stdout).trim())
                        ),
                        Ok(o) => {
                            let err = String::from_utf8_lossy(&o.stderr);
                            let last = err.lines().rev().find(|l| !l.trim().is_empty());
                            format!("{what} failed: {}", last.unwrap_or("").trim())
                        }
                        Err(e) => format!("{what} failed: {e}"),
                    };
                    let _ = tx.send(msg);
                });
                self.started = Some(rx);
            }
            Ok(Start::Foreground(c)) => {
                self.foreground(terminal, c, &what);
                if kind == HarnessKind::Codex {
                    self.record_codex(&dir, since);
                }
                self.invalidate();
            }
            Err(e) => {
                // The instruction is not lost to a refusal.
                self.text = prompt;
                self.status = e.to_string();
            }
        }
    }

    /// ctrl+x on a job with no run in flight: once arms, again removes the job from jobs.yaml
    /// and reinstalls launchd. Any other key keeps it.
    fn delete_job(&mut self, name: String) {
        match self.armed.take() {
            Some(armed) if armed == name => {
                match config::write_job(&self.jobs_path, Some(&name), None) {
                    Ok(()) => {
                        self.install(&format!("job {name} deleted"));
                        self.invalidate();
                    }
                    Err(e) => self.status = format!("delete failed: {e:#}"),
                }
            }
            _ => {
                self.status = format!("ctrl+x again to delete job {name} · any other key keeps it");
                self.armed = Some(name);
            }
        }
    }

    /// ctrl+x on a run that is not in flight: once arms, again hides it from the dashboard for
    /// good. The ledger, `cones ls` and `cones attach` still have it.
    fn hide_run(&mut self, id: String) {
        match self.armed.take() {
            Some(armed) if armed == id => {
                self.status = match Ledger::new(&self.state).and_then(|l| l.hide(&id)) {
                    Ok(()) => "run hidden · cones ls still has it".into(),
                    Err(e) => format!("hide failed: {e:#}"),
                };
                self.invalidate();
            }
            _ => {
                self.status = "ctrl+x again to hide this run · any other key keeps it".into();
                self.armed = Some(id);
            }
        }
    }

    /// ctrl+e: the wizard on the selected job, filled in from the file as written.
    fn edit_job(&mut self) {
        let Some(Kind::Job(name)) = self.selected().map(|r| r.kind.clone()) else {
            self.status = "select a job to edit · ctrl+n adds one".into();
            return;
        };
        match config::raw_jobs(&self.jobs_path) {
            Ok(jobs) => match jobs.into_iter().find(|j| j.name == name) {
                Some(j) => {
                    self.mode =
                        Mode::Job(Box::new(JobForm::new(&self.jobs_dir(), &self.cwd, Some(j))));
                }
                None => {
                    self.status = format!("{name} is not in {}", fleet::tilde(&self.jobs_path));
                }
            },
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// What ctrl+x does to a session row. A Codex thread behind the daemon has no stop, so its
    /// record is forgotten; a Claude background session is removed with `claude rm`, which drops
    /// the job record `claude agents` shows; anything else is stopped with a signal.
    fn session_verb(&self, id: &str) -> &'static str {
        match self
            .data
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.kind.as_deref())
        {
            Some("daemon") => "forget",
            Some("bg") => "delete",
            _ => "stop",
        }
    }

    /// What ctrl+x does to the selected row, for the hint line; nothing on a row it cannot act on.
    fn stop_verb(&self) -> Option<&'static str> {
        let live = |name: &str| {
            self.data
                .runs
                .iter()
                .any(|r| r.started.job.as_deref() == Some(name) && r.status() == "started")
        };
        match &self.selected()?.kind {
            Kind::Job(name) if live(name) => Some("stop"),
            Kind::Job(_) => Some("delete"),
            Kind::Run(_, s) if s == "started" => Some("stop"),
            Kind::Run(..) => Some("hide"),
            Kind::Session(id, _) => Some(self.session_verb(id)),
            _ => None,
        }
    }

    /// The composer: the harness `tab` picked, then the instruction, or where it would run.
    fn composer(&self) -> Line<'static> {
        let kind = harness::KNOWN[self.harness].to_string();
        let mut spans = vec![Span::styled(
            format!("{} › ", logo(&kind)),
            brand(&kind).add_modifier(Modifier::BOLD),
        )];
        spans.extend(typed(
            &self.text,
            &format!(
                "an instruction for {} · enter starts {kind} there",
                fleet::tilde(&self.target_dir())
            ),
        ));
        Line::from(spans)
    }

    /// The bottom line: the last action's status until the next key, else the keys.
    fn hint_line(&self) -> Line<'static> {
        if !self.status.is_empty() {
            return Line::styled(self.status.clone(), dim());
        }
        if let Some(action) = self
            .stopping
            .iter()
            .find(|a| self.selected().and_then(|r| r.kind.key()) == Some(a.id.as_str()))
        {
            return Line::styled(action.message(), dim());
        }
        let next = harness::KNOWN[(self.harness + 1) % harness::KNOWN.len()].to_string();
        let start = format!(
            "start {} in {}",
            harness::KNOWN[self.harness],
            fleet::tilde(&self.target_dir())
        );
        let mut line = match &self.mode {
            Mode::Filter => hints(&[("enter", "keep the filter"), ("esc", "clear it")]),
            Mode::Job(_) => hints(&[
                ("enter", "next"),
                ("backspace", "on an empty answer goes back"),
                ("esc", "cancel"),
            ]),
            Mode::Harness(_) => Line::default(),
            Mode::Normal if !self.text.is_empty() => {
                hints(&[("enter", &start), ("tab", &next), ("esc", "clear")])
            }
            // Only what acts on the selected row, then the keys that act everywhere.
            Mode::Normal => {
                let mut keys = vec![];
                if self.selected().is_some() {
                    keys.push(("enter", self.enter_label()));
                }
                if let Some(verb) = self.stop_verb() {
                    keys.push(("ctrl+x", verb));
                }
                if let Some(Kind::Job(_)) = self.selected().map(|r| &r.kind) {
                    keys.push(("ctrl+e", "edit"));
                }
                keys.extend([
                    ("tab", next.as_str()),
                    ("ctrl+n", "new job"),
                    ("ctrl+s", "regroup"),
                    ("ctrl+o", "agents"),
                    ("esc", "quit"),
                ]);
                hints(&keys)
            }
        };
        if !self.filter.is_empty() {
            line.spans
                .insert(0, Span::styled(format!("filter: {}  ", self.filter), dim()));
        }
        line
    }

    /// ctrl+x once arms and marks the row, ctrl+x again stops; any other key disarms, so the
    /// mark stays for as long as the user looks at it: the `claude agents` convention.
    fn stop(&mut self) {
        let id = match self.selected().map(|r| r.kind.clone()) {
            Some(Kind::Run(id, s)) if s != "started" => return self.hide_run(id),
            Some(Kind::Session(id, _) | Kind::Run(id, _)) => id,
            // A job row with a run in flight stops that run; with none, ctrl+x deletes the job.
            Some(Kind::Job(name)) => {
                let live =
                    self.data.runs.iter().rev().find(|r| {
                        r.started.job.as_deref() == Some(&name) && r.status() == "started"
                    });
                match live {
                    Some(r) => r.started.run_id.clone(),
                    None => return self.delete_job(name),
                }
            }
            _ => {
                self.status = "select a job, run or session to stop".into();
                return;
            }
        };
        // `codex resume` still has a forgotten thread; `claude --resume` still has a deleted
        // background session's conversation.
        let verb = self.session_verb(&id);
        if let Some(action) = self.stopping.iter().find(|a| a.id == id) {
            self.status = action.message();
            self.armed = None;
            return;
        }
        match self.armed.take() {
            Some(armed) if armed == id => {
                let (state, claude, target) = (self.state.clone(), self.claude.clone(), id.clone());
                self.queue_stop(id, verb, move || {
                    if verb == "forget" {
                        codex::forget(&state, &target)?;
                        Ok(true)
                    } else {
                        Ledger::new(&state).and_then(|l| runner::stop(&l, &claude, &target))
                    }
                });
            }
            _ => {
                self.armed = Some(id);
                self.status = if verb == "forget" {
                    "ctrl+x again to forget this thread · any other key keeps it".into()
                } else {
                    format!("ctrl+x again to {verb} · any other key keeps it")
                };
            }
        }
    }

    fn queue_stop(
        &mut self,
        id: String,
        verb: &'static str,
        work: impl FnOnce() -> Result<bool> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let action = PendingStop {
            id,
            verb,
            result: rx,
        };
        self.status = action.message();
        self.debug(|| self.status.clone());
        let log = self.log.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = work();
            if let Some(log) = log {
                debug_line(
                    &log,
                    format!(
                        "timing command_{verb} ms={:.3}",
                        started.elapsed().as_secs_f64() * 1000.0
                    ),
                );
            }
            let _ = tx.send(result);
        });
        self.stopping.push(action);
    }

    fn poll_stops(&mut self) {
        let mut finished = false;
        for action in std::mem::take(&mut self.stopping) {
            let result = match action.result.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    self.stopping.push(action);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("action worker disconnected"))
                }
                Ok(result) => result,
            };
            finished = true;
            self.status = match result {
                Ok(true) if matches!(action.verb, "delete" | "forget") => {
                    self.removed_sessions.insert(action.id.clone());
                    self.data.sessions.retain(|s| s.session_id != action.id);
                    self.rebuild();
                    if action.verb == "delete" {
                        "deleted · claude --resume still has it".into()
                    } else {
                        "thread forgotten · codex resume still has it".into()
                    }
                }
                Ok(true) => "stop requested".into(),
                Ok(false) => "already finished".into(),
                Err(e) => format!("{} failed: {e:#}", action.verb),
            };
            self.debug(|| format!("{}: {}", action.id, self.status));
        }
        if finished {
            self.feedback
                .get_or_insert(("action_result_to_draw", Instant::now()));
            self.invalidate();
        }
    }

    /// Returns true when the dashboard should exit. Plain keys type into the composer, so every
    /// action is on ctrl or an arrow, as in `claude agents`. The status of the last action shows
    /// until the next key.
    fn key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        self.status.clear();
        match &mut self.mode {
            Mode::Filter => {
                match code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) if !ctrl => self.filter.push(c),
                    _ => {}
                }
                self.apply_filter();
                self.settle();
            }
            // The harness's own list of its agents, as a viewer this dashboard waits on: `claude
            // agents`, or Codex's resume picker on the daemon. No row is needed first.
            Mode::Harness(i) => {
                let i = *i;
                match code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::Char(' ') => {
                        self.mode = Mode::Harness((i + 1) % harness::KNOWN.len());
                    }
                    KeyCode::Enter => {
                        let kind = harness::KNOWN[i];
                        self.mode = Mode::Normal;
                        match harness::agents(kind) {
                            Ok(c) => {
                                self.foreground(terminal, c, &format!("{kind} agents"));
                                self.invalidate();
                            }
                            Err(e) => self.status = e.to_string(),
                        }
                    }
                    _ => {}
                }
            }
            Mode::Job(form) => match form.key(code, ctrl) {
                FormAction::Stay => {}
                FormAction::Cancel => self.mode = Mode::Normal,
                // The file is checked as a whole before it is replaced; a bad answer comes back
                // inline and the wizard stays where it was.
                FormAction::Save(old, job) => {
                    match config::write_job(&self.jobs_path, old.as_deref(), Some(&job)) {
                        Ok(()) => {
                            self.mode = Mode::Normal;
                            self.install(&format!("job {} saved", job.name));
                            self.invalidate();
                        }
                        Err(e) => {
                            if let Mode::Job(form) = &mut self.mode {
                                form.error = Some(format!("{e:#}"));
                            }
                        }
                    }
                }
            },
            Mode::Normal => {
                // Any key but ctrl+x disarms an armed ctrl+x, so the mark stays until the user
                // does something else, as in `claude agents`.
                let armed = self.armed.take();
                match code {
                    KeyCode::Char('c') if ctrl => return Ok(true),
                    KeyCode::Char('x') if ctrl => {
                        self.armed = armed;
                        self.stop();
                    }
                    // esc backs out one thing at a time: the armed ctrl+x, the text, the dashboard.
                    KeyCode::Esc => {
                        if armed.is_some() {
                            self.status = "kept".into();
                        } else if !self.text.is_empty() {
                            self.text.clear();
                        } else {
                            return Ok(true);
                        }
                    }
                    KeyCode::Up => self.step(-1),
                    KeyCode::Down => self.step(1),
                    KeyCode::Tab => self.harness = (self.harness + 1) % harness::KNOWN.len(),
                    KeyCode::Enter if self.text.trim().is_empty() => self.enter(terminal)?,
                    KeyCode::Enter => self.start(terminal),
                    KeyCode::Backspace => {
                        self.text.pop();
                    }
                    KeyCode::Char('s') if ctrl => {
                        self.by_state = !self.by_state;
                        self.rebuild();
                    }
                    KeyCode::Char('n') if ctrl => {
                        let (base, fallback) = (self.jobs_dir(), self.target_dir());
                        self.mode = Mode::Job(Box::new(JobForm::new(&base, &fallback, None)));
                    }
                    KeyCode::Char('e') if ctrl => self.edit_job(),
                    KeyCode::Char('o') if ctrl => self.mode = Mode::Harness(0),
                    KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
                    KeyCode::Char('r') if ctrl => {
                        self.invalidate();
                        self.status = "refresh requested".into();
                    }
                    KeyCode::Char(c) if !ctrl => self.text.push(c),
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [head, list, prompt, foot] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(Paragraph::new(header_lines(self.data.summary())), head);
        self.draw_list(frame, list);
        let line = match &self.mode {
            Mode::Filter => {
                let mut spans = vec![Span::styled("/ ", bold())];
                spans.extend(typed(&self.filter, "text a row must contain"));
                Line::from(spans)
            }
            Mode::Job(f) => f.line(),
            Mode::Harness(i) => {
                let mut spans = vec![Span::styled("open › ", Style::default().fg(ORANGE))];
                choices(
                    &mut spans,
                    &["claude agents", ">_ codex resume"],
                    *i,
                    "open",
                );
                Line::from(spans)
            }
            Mode::Normal => self.composer(),
        };
        // Ruled above and below, as Claude Code frames its input.
        let frame_lines = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(dim());
        frame.render_widget(Paragraph::new(line).block(frame_lines), prompt);
        frame.render_widget(Paragraph::new(self.hint_line()), foot);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let height = area.height as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if height > 0 && self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(height)
            .map(|(n, &i)| {
                let row = &self.rows[i];
                let selected = n == self.cursor;
                let armed = self
                    .armed
                    .as_deref()
                    .is_some_and(|a| row.kind.key() == Some(a));
                let mut spans = Vec::with_capacity(row.cells.len() + 1);
                if row.kind.selectable() {
                    spans.push(Span::styled(
                        if selected { "▌ " } else { "  " },
                        Style::default().fg(if armed { Color::Red } else { ORANGE }),
                    ));
                }
                let (frames, brand) = spinner(row);
                for (c, (text, style)) in row.cells.iter().enumerate() {
                    let (text, style) = if c == 0 && row.working() {
                        (
                            text.replacen('▲', frames[self.tick % frames.len()], 1),
                            brand.unwrap_or(*style),
                        )
                    } else {
                        (text.clone(), *style)
                    };
                    spans.push(Span::styled(
                        text,
                        if armed { style.fg(Color::Red) } else { style },
                    ));
                }
                let line = Line::from(spans);
                if selected { line.style(bold()) } else { line }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

pub fn run(exe: &Path, jobs_path: &Path, state: &Path, claude: &Path, debug: bool) -> Result<i32> {
    let started = Instant::now();
    let mut app = App::new(exe, jobs_path, state, claude)?;
    if debug {
        app.log = Some(state.join("tui-debug.log"));
        app.debug(|| {
            format!(
                "dashboard start pid {}; {}",
                std::process::id(),
                term_state()
            )
        });
    }
    app.timing("startup_load", started);
    app.feedback = Some(("startup_to_draw", started));
    app.rebuild();
    // Raw mode makes ctrl-z a key, but a child that has just restored the terminal and exited
    // leaves a gap in which ctrl-z is SIGTSTP to the whole foreground group; ignored, it cannot
    // suspend the dashboard from under the user. Children get the default back in pre_exec.
    // A foreground child owns the tty as its own process group; taking it back with tcsetpgrp
    // from the background is SIGTTOU unless ignored.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    SHELL_TTY.get_or_init(|| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(0, &mut t) == 0).then_some(t)
    });
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let animation = Instant::now();
        let mut drawn_tick = usize::MAX;
        let mut drawn_refresh = app.refreshed;
        let mut redraw = true;
        loop {
            app.tick = (animation.elapsed().as_millis() / 100) as usize;
            if app.refreshed.elapsed() >= Duration::from_secs(1) {
                app.reload();
            }
            app.poll();
            if redraw
                || app.feedback.is_some()
                || app.tick != drawn_tick
                || app.refreshed != drawn_refresh
            {
                let drawing = Instant::now();
                terminal.draw(|f| app.draw(f))?;
                if let Some((phase, started)) = app.feedback.take() {
                    app.timing("draw", drawing);
                    app.timing(phase, started);
                } else if drawing.elapsed() >= Duration::from_millis(16) {
                    app.timing("slow_draw", drawing);
                }
                drawn_tick = app.tick;
                drawn_refresh = app.refreshed;
                redraw = false;
            }
            // Commands and fresh data can land between animation frames. Check them promptly
            // without repainting idle frames or making the spinner depend on key frequency.
            if event::poll(Duration::from_millis(25))? {
                let e = event::read()?;
                redraw = true;
                app.debug(|| format!("event {e:?}"));
                if let Event::Key(k) = e
                    && k.kind == KeyEventKind::Press
                {
                    app.feedback = Some(("input_to_draw", Instant::now()));
                    if app.key(k.code, k.modifiers, &mut terminal)? {
                        return Ok(());
                    }
                }
            }
        }
    })();
    app.debug(|| format!("dashboard loop ended: {result:?}"));
    ratatui::restore();
    hand_back_tty();
    result.context("dashboard")?;
    Ok(0)
}

/// Wait for the child that holds the terminal. Ctrl-z in a child that leaves ISIG on (Codex,
/// vi and Claude's agents view do; interactive Claude and its attach view eat the key) stops the
/// child's process group. `Child::wait` would then block forever on a cooked terminal nobody
/// reads. With `kill_on_stop` the child is a viewer that has already restored the tty: it is
/// killed where it stands, group and all, and the session it showed is untouched. Otherwise the
/// stop is reported and the caller resumes it.
///
/// To re-check what a program does on ctrl-z, run it as a foreground job of an interactive
/// shell on a pty and read its ps state. A program forked straight onto a pty is an orphaned
/// process group, and the kernel discards its tty stops, so that probe says nothing ever stops.
fn wait_or_stopped(
    child: &mut Child,
    kill_on_stop: bool,
    debug: &dyn Fn(String),
) -> std::io::Result<Waited> {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id() as libc::pid_t;
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r == -1 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if libc::WIFSTOPPED(status) {
            let sig = libc::WSTOPSIG(status);
            if kill_on_stop {
                debug(format!("child stopped by signal {sig}; killing the viewer"));
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                continue;
            }
            debug(format!("child stopped by signal {sig}"));
            return Ok(Waited::Stopped);
        }
        // ponytail: the pid is reaped here, so `Child::wait` would fail; the status is kept.
        return Ok(Waited::Exited(ExitStatusExt::from_raw(status)));
    }
}

fn debug_line(path: &Path, msg: impl std::fmt::Display) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            f,
            "{} pid={} {msg}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
            std::process::id()
        );
    }
}

/// The terminal facts a hand-off can corrupt: the tty's line discipline, who owns the
/// foreground, and what ctrl-z and ctrl-c do to this process.
fn term_state() -> String {
    unsafe {
        let disposition = |sig| {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut old);
            match old.sa_sigaction {
                libc::SIG_DFL => "dfl",
                libc::SIG_IGN => "ign",
                _ => "handler",
            }
        };
        format!(
            "{} pgrp={} tstp={} int={} raw={:?}",
            tty_state(),
            libc::getpgrp(),
            disposition(libc::SIGTSTP),
            disposition(libc::SIGINT),
            ratatui::crossterm::terminal::is_raw_mode_enabled().ok()
        )
    }
}

/// The tty's line discipline and who owns its foreground; readable from the background, so
/// the watcher can sample it while a child holds the terminal.
fn tty_state() -> String {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        let modes = if libc::tcgetattr(0, &mut t) == 0 {
            format!(
                "icanon={} echo={} isig={} ixon={} opost={}",
                t.c_lflag & libc::ICANON != 0,
                t.c_lflag & libc::ECHO != 0,
                t.c_lflag & libc::ISIG != 0,
                t.c_iflag & libc::IXON != 0,
                t.c_oflag & libc::OPOST != 0
            )
        } else {
            format!("tcgetattr: {}", std::io::Error::last_os_error())
        };
        format!("{modes} fg_pgrp={}", libc::tcgetpgrp(0))
    }
}

/// While a child holds the terminal, log every change of the tty's modes and foreground group,
/// sampled twenty times a second, and once a second the dashboard's process group and the
/// child's (its own, shared with what it forks): `T` in the state column is a stopped child.
/// A child that restores a cooked tty and then lingers before exiting shows up as the gap
/// between the `tty` line and `child exited`. Dropping the sender stops the watcher at once.
fn watch_group(log: PathBuf, child: u32) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (stop, rx) = mpsc::channel::<()>();
    let pgrp = format!("{},{child}", unsafe { libc::getpgrp() });
    let t = std::thread::spawn(move || {
        let (mut tree, mut tty) = (String::new(), String::new());
        for n in 0u32.. {
            // ponytail: only changes are logged, so an idle attach costs a few lines.
            let now = tty_state();
            if now != tty {
                debug_line(&log, format!("tty: {now}"));
                tty = now;
            }
            if n % 20 == 0 {
                let out = Command::new("ps")
                    .args(["-o", "pid=,ppid=,stat=,tpgid=,command=", "-g", &pgrp])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
                    .unwrap_or_else(|e| format!("ps failed: {e}"));
                if out != tree {
                    debug_line(&log, format!("child tree:\n{out}"));
                    tree = out;
                }
            }
            if rx.recv_timeout(Duration::from_millis(50)) != Err(mpsc::RecvTimeoutError::Timeout) {
                break;
            }
        }
    });
    (stop, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn typed(f: &mut JobForm, text: &str) {
        for c in text.chars() {
            assert_eq!(f.key(KeyCode::Char(c), false), FormAction::Stay);
        }
    }

    fn enter(f: &mut JobForm) -> FormAction {
        f.key(KeyCode::Enter, false)
    }

    #[test]
    fn watcher_stops_when_its_sender_drops() {
        let d = dir();
        let (stop, t) = watch_group(d.path().join("log"), std::process::id());
        std::thread::sleep(Duration::from_millis(120));
        let t0 = Instant::now();
        drop(stop);
        t.join().unwrap();
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn debug_log_appends_only_when_enabled() {
        let d = dir();
        let path = d.path().join("tui-debug.log");
        let mut app = App::new(Path::new("cones"), &path, d.path(), d.path()).unwrap();
        app.debug(|| panic!("formatted without --debug"));
        assert!(!path.exists());
        app.log = Some(path.clone());
        app.debug(|| "one".into());
        app.debug(|| "two".into());
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" one") && lines[1].ends_with(" two"));
        assert!(term_state().contains("pgrp="));
    }

    fn job(script: &str) -> Child {
        use std::os::unix::process::CommandExt;
        Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_stopped_viewer_is_killed_with_its_group_at_once() {
        use std::os::unix::process::ExitStatusExt;
        let mut c = job("sleep 30 & kill -STOP $$; wait");
        let started = Instant::now();
        let log = std::sync::Mutex::new(vec![]);
        let w = wait_or_stopped(&mut c, true, &|m| log.lock().unwrap().push(m)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "did not hang on the stop"
        );
        let Waited::Exited(st) = w else {
            panic!("viewer must exit")
        };
        assert_eq!(st.signal(), Some(libc::SIGKILL));
        assert!(log.lock().unwrap()[0].ends_with("killing the viewer"));
        std::thread::sleep(Duration::from_millis(50));
        let ps = Command::new("ps")
            .args(["-o", "pid=", "-g", &c.id().to_string()])
            .output()
            .unwrap();
        assert!(
            ps.stdout.trim_ascii().is_empty(),
            "the group died with the viewer: {ps:?}"
        );
    }

    #[test]
    fn a_stopped_harness_is_reported_and_finishes_once_continued() {
        let mut c = job("kill -STOP $$; exit 3");
        let w = wait_or_stopped(&mut c, false, &|_| {}).unwrap();
        assert!(matches!(w, Waited::Stopped));
        assert!(c.try_wait().unwrap().is_none(), "still alive, stopped");
        unsafe { libc::kill(-(c.id() as libc::pid_t), libc::SIGCONT) };
        let Waited::Exited(st) = wait_or_stopped(&mut c, false, &|_| {}).unwrap() else {
            panic!("must exit after SIGCONT")
        };
        assert_eq!(st.code(), Some(3));
    }

    #[test]
    fn an_interactive_claude_row_offers_no_attach_and_says_own_terminal() {
        let d = dir();
        let mut app = App::new(
            Path::new("cones"),
            &d.path().join("jobs.yaml"),
            d.path(),
            d.path(),
        )
        .unwrap();
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        data.sessions.push(Session {
            session_id: "209aa1a4-1700-4242-a71e-57d6293b69ab".into(),
            harness: "claude".into(),
            kind: Some("interactive".into()),
            cwd: PathBuf::from("/x"),
            state: "active".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: Some(88144),
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            cost_usd: None,
            title: None,
            last: None,
        });
        app.apply(data);
        app.filter = "209aa1a4".into();
        app.apply_filter();
        app.settle();
        assert_eq!(app.enter_label(), "own terminal");
    }

    #[test]
    fn the_list_marks_a_session_that_runs_in_its_own_terminal() {
        let d = dir();
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        let session = |id: &str, kind: &str| Session {
            session_id: id.into(),
            harness: "claude".into(),
            kind: Some(kind.into()),
            cwd: PathBuf::from("/x"),
            state: "active".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: Some(1),
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            cost_usd: None,
            title: None,
            last: None,
        };
        data.sessions
            .push(session("aaaa-interactive", "interactive"));
        data.sessions.push(session("bbbb-background", "bg"));
        let marker = |id: &str| {
            data.rows(false)
                .into_iter()
                .find(|r| matches!(&r.kind, Kind::Session(s, _) if s == id))
                .map(|r| r.cells[2].0.trim().to_owned())
                .unwrap()
        };
        assert_eq!(marker("aaaa-interactive"), "own terminal");
        assert_eq!(marker("bbbb-background"), "");
    }

    #[test]
    fn a_codex_row_offers_no_attach_and_says_own_terminal() {
        let d = dir();
        let mut app = App::new(
            Path::new("cones"),
            &d.path().join("jobs.yaml"),
            d.path(),
            d.path(),
        )
        .unwrap();
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        data.sessions.push(Session {
            session_id: "codex-77".into(),
            harness: "codex".into(),
            kind: None,
            cwd: PathBuf::from("/x"),
            state: "-".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: Some(77),
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            cost_usd: None,
            title: None,
            last: None,
        });
        app.apply(data);
        app.filter = "codex-77".into();
        app.apply_filter();
        app.settle();
        assert!(matches!(&app.selected().unwrap().kind, Kind::Session(id, _) if id == "codex-77"));
        assert_eq!(app.enter_label(), "own terminal");
        app.filter.clear();
        app.apply_filter();
        assert_ne!(
            app.enter_label(),
            "own terminal",
            "the verb follows the selected row"
        );
        // A thread behind the daemon is the one Codex row that opens from here.
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        data.sessions.push(Session {
            session_id: "dddd-daemon".into(),
            harness: "codex".into(),
            kind: Some("daemon".into()),
            cwd: PathBuf::from("/x"),
            state: "idle".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: None,
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            cost_usd: None,
            title: None,
            last: None,
        });
        app.apply(data);
        app.filter = "dddd-dae".into();
        app.apply_filter();
        app.settle();
        assert_eq!(app.enter_label(), "attach");
        app.stop();
        assert_eq!(
            app.status,
            "ctrl+x again to forget this thread · any other key keeps it"
        );
    }

    #[test]
    fn launch_dir_expands_tilde_and_relative_paths() {
        let home = dirs::home_dir().unwrap();
        let base = dir();
        std::fs::create_dir(base.path().join("sub")).unwrap();
        let sub = base.path().join("sub").canonicalize().unwrap();
        assert_eq!(
            launch_dir("~", base.path(), base.path()),
            Ok(home.canonicalize().unwrap())
        );
        assert_eq!(launch_dir("sub", base.path(), base.path()), Ok(sub.clone()));
        assert_eq!(
            launch_dir("  ", base.path(), &base.path().join("sub")),
            Ok(sub.clone()),
            "blank means the fallback"
        );
        assert_eq!(
            launch_dir(&sub.display().to_string(), Path::new("/"), Path::new("/")),
            Ok(sub)
        );
    }

    #[test]
    fn launch_dir_rejects_files_missing_paths_and_user_tildes() {
        let base = dir();
        let file = base.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            launch_dir("f", base.path(), base.path()),
            Err(format!("not a directory: {}", file.display()))
        );
        assert!(
            launch_dir("missing", base.path(), base.path())
                .unwrap_err()
                .starts_with("not a directory: ")
        );
        assert!(
            launch_dir("~someone/x", base.path(), base.path())
                .unwrap_err()
                .contains("~user")
        );
        // A fallback that is gone is an error too, not a silent launch elsewhere.
        assert!(launch_dir("", base.path(), &base.path().join("gone")).is_err());
    }

    #[test]
    fn the_job_wizard_checks_each_answer_and_hands_over_a_claude_job() {
        let base = dir();
        let mut f = JobForm::new(base.path(), base.path(), None);
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(
            (f.step, f.error.is_some()),
            (Step::Name, true),
            "an empty name stays"
        );
        typed(&mut f, "bad name");
        assert_eq!(f.error, None, "the next key clears the error");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Name);
        for _ in 0..8 {
            f.key(KeyCode::Backspace, false);
        }
        typed(&mut f, "nightly");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Dir);
        assert!(f.line().to_string().contains("new job · dir › "));
        // An empty directory means the placeholder, kept in ~ form.
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Schedule);
        assert_eq!(f.dir, fleet::tilde(&base.path().canonicalize().unwrap()));
        typed(&mut f, "not cron");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Schedule);
        assert!(f.error.is_some());
        f.schedule.clear();
        typed(&mut f, "0 2 * * *");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Prompt);
        assert_eq!(enter(&mut f), FormAction::Stay, "the task cannot be empty");
        // Backspace on an empty answer steps back, and forward again keeps the answers.
        f.key(KeyCode::Backspace, false);
        assert_eq!(f.step, Step::Schedule);
        assert_eq!(enter(&mut f), FormAction::Stay);
        typed(&mut f, "triage the TODOs");
        match enter(&mut f) {
            FormAction::Save(None, job) => {
                assert_eq!(job.name, "nightly");
                assert_eq!(job.harness, HarnessKind::Claude);
                assert_eq!(job.schedule, "0 2 * * *");
                assert_eq!(job.cwd, PathBuf::from(&f.dir));
                assert_eq!(job.prompt, "triage the TODOs");
                assert_eq!(job.model, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn editing_keeps_the_fields_the_wizard_does_not_ask_about() {
        let base = dir();
        let mut j = config::Job::new("one", "0 9 * * *", Path::new("."), "first");
        j.model = Some("sonnet".into());
        j.budget_usd = Some(0.5);
        let mut f = JobForm::new(base.path(), base.path(), Some(j));
        assert_eq!((f.name.as_str(), f.dir.as_str()), ("one", "."));
        assert!(f.line().to_string().starts_with("edit one · name › one"));
        for _ in 0..3 {
            assert_eq!(enter(&mut f), FormAction::Stay);
        }
        assert_eq!(f.step, Step::Prompt);
        typed(&mut f, ", revised");
        match enter(&mut f) {
            FormAction::Save(Some(old), job) => {
                assert_eq!(old, "one");
                assert_eq!(job.prompt, "first, revised");
                assert_eq!(job.model.as_deref(), Some("sonnet"));
                assert_eq!(job.budget_usd, Some(0.5));
                assert_eq!(
                    job.cwd,
                    PathBuf::from(fleet::tilde(&base.path().canonicalize().unwrap()))
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.key(KeyCode::Esc, false), FormAction::Cancel);
    }

    fn registry(claude: &Path, id: &str, cwd: &str, status: &str, started: i64) {
        fs::create_dir_all(claude.join("sessions")).unwrap();
        fs::write(
            claude.join("sessions").join(format!("{id}.json")),
            serde_json::json!({"pid": std::process::id(), "sessionId": id, "cwd": cwd,
                "kind": "interactive", "status": status, "startedAt": started, "updatedAt": started})
            .to_string(),
        )
        .unwrap();
    }

    /// A dashboard before its first `refresh`, so a test sets filter and grouping first.
    fn app(dir: &Path) -> App {
        App::new(Path::new("cones"), &dir.join("none.yaml"), dir, dir).unwrap()
    }

    fn key(app: &App) -> Option<String> {
        app.selected().and_then(|r| r.kind.key().map(str::to_owned))
    }

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

    #[test]
    fn kind_key_is_the_id_without_the_state() {
        assert_eq!(Kind::Session(A.into(), "idle".into()).key(), Some(A));
        assert_eq!(Kind::Session(A.into(), "active".into()).key(), Some(A));
        assert_eq!(Kind::Run("run-1".into(), "ok".into()).key(), Some("run-1"));
        assert_eq!(Kind::Job("nightly".into()).key(), Some("nightly"));
        assert_eq!(Kind::Header.key(), None);
        assert_eq!(Kind::Columns.key(), None);
        assert_eq!(Kind::Blank.key(), None);
    }

    /// The terminal hand-off invalidates the previous read. The filter and grouping are
    /// fields, and applying the new data finds the selected row again by id.
    #[test]
    fn coming_back_lands_on_the_same_row_with_filter_and_grouping_kept() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        registry(claude, B, "/src/two", "idle", 1_757_682_872_000);
        registry(claude, C, "/src/two", "idle", 1_757_682_873_000);
        let mut app = app(claude);
        app.by_state = true;
        app.filter = "two".into();
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(B), "oldest match first");
        app.step(1);
        assert_eq!(key(&app).as_deref(), Some(C));
        let before = app.cursor;
        // While C was open it started working: grouped by state it now sits in a group of its
        // own, above the idle rows, so its index moved.
        registry(claude, C, "/src/two", "busy", 1_757_682_873_000);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(C));
        assert_ne!(app.cursor, before, "the row moved; the cursor followed it");
        assert!(
            matches!(&app.selected().unwrap().kind, Kind::Session(_, s) if s == "active"),
            "the row shows what the session became"
        );
        assert!(app.by_state, "grouping kept");
        assert_eq!(app.filter, "two", "filter kept");
        assert!(matches!(app.mode, Mode::Normal));
        // The session ended while open: the cursor falls on a remaining row, not on nothing.
        fs::remove_file(claude.join("sessions").join(format!("{C}.json"))).unwrap();
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(B));
        assert!(app.by_state && app.filter == "two");
        // Without the filter, A is back too and C's absence still leaves a selection.
        app.filter.clear();
        app.refresh().unwrap();
        assert!(key(&app).is_some());
    }

    fn poll_until(app: &mut App, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done(app) {
            app.poll();
            assert!(Instant::now() < deadline, "background work did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn returning_discards_the_old_snapshot_and_reads_again_without_waiting_for_the_tick() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let stale = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        registry(d.path(), A, "/src/one", "busy", 1);
        app.invalidate();
        app.invalidate(); // Multiple transitions still queue just one fresh read.
        let refreshed = app.refreshed;
        tx.send(Ok(stale)).ok().unwrap();
        app.poll();
        assert_eq!(app.refreshed, refreshed, "the old snapshot was not applied");
        assert!(app.loading.is_some(), "the next read starts immediately");
        assert!(!app.reload_pending);
        poll_until(&mut app, |a| a.loading.is_none());
        assert_eq!(app.data.sessions[0].state, "active");
        assert_eq!(key(&app).as_deref(), Some(A));
    }

    #[test]
    fn slow_delete_keeps_navigation_live_and_removes_only_after_success() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        registry(d.path(), B, "/src/one", "idle", 2);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let (release, wait) = mpsc::channel();
        app.queue_stop(A.into(), "delete", move || {
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(true)
        });
        assert_eq!(app.status, "deleting aaaaaaaa");
        assert_eq!(app.stopping.len(), 1);
        app.stop();
        assert_eq!(app.stopping.len(), 1, "a repeated delete is not submitted");
        assert!(app.armed.is_none());
        app.poll();
        assert_eq!(app.data.sessions.len(), 2, "no success was reported yet");
        app.step(1);
        assert_eq!(key(&app).as_deref(), Some(B), "input is still handled");
        release.send(()).unwrap();
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert!(app.data.sessions.iter().all(|s| s.session_id != A));
        assert_eq!(key(&app).as_deref(), Some(B));
        poll_until(&mut app, |a| a.loading.is_none());
        // Claude has acknowledged the removal, but its registry can still contain the row.
        app.refresh().unwrap();
        assert!(app.data.sessions.iter().all(|s| s.session_id != A));
        assert!(app.removed_sessions.contains(A));
        fs::remove_file(d.path().join("sessions").join(format!("{A}.json"))).unwrap();
        app.refresh().unwrap();
        assert!(app.removed_sessions.is_empty(), "the registry caught up");
    }

    #[test]
    fn delete_completion_invalidates_a_snapshot_captured_before_the_command() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let stale = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        let (data_tx, data_rx) = mpsc::channel();
        app.loading = Some(data_rx);
        let (tx, rx) = mpsc::channel();
        app.stopping.push(PendingStop {
            id: A.into(),
            verb: "delete",
            result: rx,
        });
        tx.send(Ok(true)).unwrap();
        data_tx.send(Ok(stale)).ok().unwrap();
        app.poll();
        assert!(app.data.sessions.is_empty());
        assert!(app.loading.is_some(), "a fresh read starts on completion");
        poll_until(&mut app, |a| a.loading.is_none());
        assert!(
            app.data.sessions.is_empty(),
            "the stale row cannot reappear"
        );
    }

    #[test]
    fn failed_delete_keeps_the_row_and_reports_the_error() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.queue_stop(A.into(), "delete", || anyhow::bail!("harness refused"));
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert_eq!(app.status, "delete failed: harness refused");
        assert_eq!(key(&app).as_deref(), Some(A));
        assert!(app.removed_sessions.is_empty());
        poll_until(&mut app, |a| a.loading.is_none());
    }

    #[test]
    fn a_failed_reload_keeps_the_screen_and_the_next_read_can_recover() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        tx.send(Err(anyhow::anyhow!("unreadable ledger")))
            .ok()
            .unwrap();
        app.poll();
        assert_eq!(app.status, "reload failed: unreadable ledger");
        assert_eq!(key(&app).as_deref(), Some(A));
        assert!(app.loading.is_none());
        registry(d.path(), A, "/src/one", "busy", 1);
        app.invalidate();
        poll_until(&mut app, |a| a.loading.is_none());
        assert_eq!(app.data.sessions[0].state, "active");
        assert!(
            app.status.is_empty(),
            "a recovered read clears its old error"
        );
    }

    #[test]
    fn concurrent_deletions_reconcile_by_id_when_results_arrive_out_of_order() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        registry(d.path(), B, "/src/one", "idle", 2);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        app.stopping = vec![
            PendingStop {
                id: A.into(),
                verb: "delete",
                result: first_rx,
            },
            PendingStop {
                id: B.into(),
                verb: "delete",
                result: second_rx,
            },
        ];
        second_tx.send(Ok(true)).unwrap();
        app.poll();
        assert_eq!(app.stopping.len(), 1);
        assert_eq!(key(&app).as_deref(), Some(A));
        first_tx.send(Err(anyhow::anyhow!("refused"))).ok().unwrap();
        app.poll();
        assert!(app.stopping.is_empty());
        assert_eq!(key(&app).as_deref(), Some(A));
        assert_eq!(app.status, "delete failed: refused");
        poll_until(&mut app, |a| a.loading.is_none());
        assert_eq!(app.data.sessions.len(), 1);
        assert_eq!(app.data.sessions[0].session_id, A);
    }

    #[test]
    fn a_stop_request_preserves_the_reported_state_until_the_harness_changes_it() {
        let d = dir();
        registry(d.path(), A, "/src/one", "busy", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.queue_stop(A.into(), "stop", || Ok(true));
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert_eq!(app.status, "stop requested");
        assert_eq!(app.data.sessions[0].state, "active");
        assert!(app.removed_sessions.is_empty());
        poll_until(&mut app, |a| a.loading.is_none());
    }

    #[test]
    fn regrouping_uses_the_displayed_data_without_waiting_for_a_load() {
        let d = dir();
        registry(d.path(), A, "/src/one", "busy", 1);
        registry(d.path(), B, "/src/two", "idle", 2);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        app.step(1);
        app.by_state = true;
        app.rebuild();
        assert_eq!(key(&app).as_deref(), Some(B));
        assert!(
            app.rows
                .iter()
                .any(|r| r.kind == Kind::Header && r.text() == "idle")
        );
        assert!(app.loading.is_some());
        drop(tx);
    }

    #[test]
    fn ctrl_x_on_a_job_with_no_run_arms_then_deletes_it() {
        let d = dir();
        let jobs = d.path().join("jobs.yaml");
        fs::write(
            &jobs,
            format!(
                "version: 1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: {}\n    prompt: first\n",
                d.path().display()
            ),
        )
        .unwrap();
        let mut app =
            App::new(Path::new("cones-not-installed"), &jobs, d.path(), d.path()).unwrap();
        app.refresh().unwrap();
        assert!(matches!(app.selected().unwrap().kind, Kind::Job(_)));
        assert_eq!(app.stop_verb(), Some("delete"));
        let hint: String = app
            .hint_line()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            hint.starts_with("enter start job · ctrl+x delete · ctrl+e edit · tab codex"),
            "a job row offers its own keys first: {hint}"
        );
        app.stop();
        assert!(
            app.status.contains("again to delete job one"),
            "{}",
            app.status
        );
        assert!(
            fs::read_to_string(&jobs).unwrap().contains("name: one"),
            "armed only"
        );
        app.stop();
        assert!(!fs::read_to_string(&jobs).unwrap().contains("name: one"));
        assert!(app.status.starts_with("job one deleted"), "{}", app.status);
    }

    #[test]
    fn ctrl_x_on_a_finished_run_arms_then_hides_it_and_the_ledger_keeps_it() {
        let d = dir();
        let ledger = Ledger::new(d.path()).unwrap();
        let mut start = crate::ledger::Record::new(A.into(), crate::ledger::Status::Started);
        start.fired_at = Some(chrono::Utc::now());
        ledger.append(&start).unwrap();
        ledger
            .append(&crate::ledger::Record::new(
                A.into(),
                crate::ledger::Status::Ok,
            ))
            .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert!(matches!(&app.selected().unwrap().kind, Kind::Run(id, s) if id == A && s == "ok"));
        assert_eq!(app.stop_verb(), Some("hide"));
        app.stop();
        assert!(
            app.status.contains("again to hide this run"),
            "{}",
            app.status
        );
        app.refresh().unwrap();
        assert!(key(&app).is_some(), "armed only");
        // The arm marks the row red and has no timer: it stays until the next key.
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let marked = t
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() == "▌" && c.fg == Color::Red);
        assert!(marked, "the armed row is red");
        assert_eq!(app.armed.as_deref(), Some(A));
        app.stop();
        assert!(app.status.starts_with("run hidden"), "{}", app.status);
        app.refresh().unwrap();
        assert!(key(&app).is_none(), "gone from the dashboard");
        assert_eq!(ledger.runs().unwrap().len(), 1, "the ledger keeps it");
    }

    #[test]
    fn the_bottom_lines_name_the_next_harness_and_what_ctrl_x_does() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let text = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(app.composer()).starts_with("claude › an instruction for "));
        let hint = text(app.hint_line());
        assert!(
            hint.starts_with("tab codex · ctrl+n new job"),
            "nothing selected, no row-bound keys: {hint}"
        );
        app.harness = (app.harness + 1) % harness::KNOWN.len();
        assert!(text(app.composer()).starts_with(">_ codex › "));
        assert!(text(app.hint_line()).contains("tab claude"));
        app.text = "fix the tests".into();
        assert!(text(app.hint_line()).starts_with("enter start codex in "));
        app.status = "back from attach".into();
        assert_eq!(
            text(app.hint_line()),
            "back from attach",
            "a status replaces the hints"
        );
        assert_eq!(
            uncolored("backgrounded · \x1b[36m7890c11a\x1b[39m (idle)"),
            "backgrounded · 7890c11a (idle)"
        );
    }
}
