//! `cones tui` is the native dashboard: jobs, every live harness session grouped by directory
//! or by state, and runs, with a details pane, a launch prompt (`n`: any directory, any known
//! harness, interactive or managed) and the actions. ratatui draws;
//! cones supplies rows. `cones __list` prints the same rows as tab-separated text.
//! Run statuses and session states go through the same match arms (`active`, `idle`, `blocked`,
//! `exited` are session states); a run status must not reuse those words or its rows sort and
//! draw as sessions.
use crate::{
    config::{self, HarnessKind, ResolvedJob},
    fleet::{self, Session},
    harness,
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
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

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

/// Exchanges the details pane shows for a session: the last one, or, with `tab`, the last
/// dozen so a session can be read before it is opened.
pub const MORE: usize = 12;

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
        let runs = Ledger::new(state)?.runs()?;
        let sessions = fleet_rows(claude, &runs)?;
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
                    "idle" | "suspended" => 3,
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
        let mut names = vec!["", "", "title"];
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
    for line in header_lines(data.summary(), enter_verb(None), Pane::Hidden) {
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
        Some(Kind::Session(_, s)) if s == "suspended" => "resume",
        Some(Kind::Session(..) | Kind::Run(..)) => "attach",
        _ => "open",
    }
}

/// How much of the screen the details pane has; `tab` cycles it, starting hidden.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pane {
    Hidden,
    Peek,
    More,
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

/// The three header lines: the cone, with the fleet summary beside its bands and the keys
/// beside its base, each key lit and its verb dim so the eye finds the key first. `pane` names
/// what the next `tab` does. Two callers: `draw` and the `--tsv` path in `list`, so a change to
/// the signature or the hint text reaches script output too and its tests.
fn header_lines(summary: Line<'static>, enter: &str, pane: Pane) -> Vec<Line<'static>> {
    let tab = match pane {
        Pane::Hidden => "peek",
        Pane::Peek => "more",
        Pane::More => "hide",
    };
    let keys = [
        ("↑↓", "move"),
        ("enter", enter),
        ("tab", tab),
        ("x x", "stop"),
        ("e", "edit jobs"),
        ("s", "regroup"),
        ("n", "new task"),
        ("/", "filter"),
        ("r", "refresh"),
        ("q", "quit"),
    ];
    let [top, mut middle, mut hints] = cone();
    middle.push(Span::raw("  "));
    middle.extend(summary.spans);
    hints.push(Span::raw("  "));
    for (i, (key, verb)) in keys.iter().enumerate() {
        if i > 0 {
            hints.push(Span::styled(" · ", dim()));
        }
        hints.push(Span::styled((*key).to_owned(), bold()));
        hints.push(Span::styled(format!(" {verb}"), dim()));
    }
    vec![Line::from(top), Line::from(middle), Line::from(hints)]
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

/// One glyph per state, cone-shaped where it can be: a solid cone is busy, a hollow one is
/// resting, a warning cone wants a human. `-` is a session whose harness reported no state, a
/// Codex before its first turn; it is not a failure.
fn icon(state: &str) -> &str {
    match state {
        "active" | "started" => "▲",
        "blocked" => "⚠",
        "idle" => "△",
        "suspended" => "▽",
        "exited" => "▵",
        "ok" => "✓",
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
        "active" | "started" | "ok" => Style::default().fg(Color::Green),
        "blocked" | "skipped" => Style::default().fg(Color::Yellow),
        "suspended" => Style::default().fg(Color::Indexed(75)),
        "idle" | "exited" | "-" => dim(),
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
pub fn fleet_rows(claude: &Path, runs: &[Run]) -> Result<Vec<Session>> {
    let owned: HashSet<&str> = runs
        .iter()
        .filter_map(|r| r.started.session_id.as_deref())
        .collect();
    Ok(fleet::all(claude)?
        .into_iter()
        .filter(|s| !owned.contains(s.session_id.as_str()))
        .collect())
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

/// Where the `n` prompt is: each step is one question on the footer line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Dir,
    Harness,
    How,
    Prompt,
}

/// What a key in the `n` prompt asks the dashboard to do.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchAction {
    Stay,
    Cancel,
    /// Suspend the dashboard and run the harness natively in the directory.
    Interactive(PathBuf, HarnessKind),
    /// A supervised `cones run --prompt` in the directory.
    Managed(PathBuf, HarnessKind, String),
}

/// The `n` prompt: a directory, a harness, interactive or managed, then the task for a managed
/// run. `enter` answers a question, `esc` cancels, backspace on an empty answer steps back.
/// Pure: every filesystem fact comes in through `base`, `fallback` and `launch_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub step: Step,
    pub dir: String,
    pub resolved: Option<PathBuf>,
    pub harness: usize,
    pub managed: bool,
    pub prompt: String,
    pub error: Option<String>,
    base: PathBuf,
    fallback: PathBuf,
}

impl Launch {
    /// `fallback` is the directory an empty answer means and is shown as the placeholder.
    pub fn new(base: &Path, fallback: &Path) -> Self {
        Self {
            step: Step::Dir,
            dir: String::new(),
            resolved: None,
            harness: 0,
            managed: false,
            prompt: String::new(),
            error: None,
            base: base.to_owned(),
            fallback: fallback.to_owned(),
        }
    }

    pub fn kind(&self) -> HarnessKind {
        harness::KNOWN[self.harness]
    }

    /// The directory the launch will use so far: the validated one, else the placeholder.
    pub fn target(&self) -> &Path {
        self.resolved.as_deref().unwrap_or(&self.fallback)
    }

    fn cycle(&mut self, delta: isize) {
        let n = harness::KNOWN.len() as isize;
        self.harness = (self.harness as isize + delta).rem_euclid(n) as usize;
    }

    pub fn key(&mut self, code: KeyCode, ctrl: bool) -> LaunchAction {
        if code == KeyCode::Esc {
            return LaunchAction::Cancel;
        }
        self.error = None;
        match self.step {
            Step::Dir => match code {
                KeyCode::Enter => match launch_dir(&self.dir, &self.base, &self.fallback) {
                    Ok(dir) => {
                        self.resolved = Some(dir);
                        self.step = Step::Harness;
                    }
                    Err(e) => self.error = Some(e),
                },
                KeyCode::Backspace => {
                    self.dir.pop();
                }
                KeyCode::Char(c) if !ctrl => self.dir.push(c),
                _ => {}
            },
            Step::Harness => match code {
                KeyCode::Left | KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('h') => {
                    self.cycle(-1)
                }
                KeyCode::Right
                | KeyCode::Down
                | KeyCode::Tab
                | KeyCode::Char(' ')
                | KeyCode::Char('j')
                | KeyCode::Char('l') => self.cycle(1),
                KeyCode::Enter => self.step = Step::How,
                KeyCode::Backspace => self.step = Step::Dir,
                _ => {}
            },
            Step::How => match code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Tab
                | KeyCode::Char(' ')
                | KeyCode::Char('h')
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Char('l') => self.managed = !self.managed,
                KeyCode::Char('i') => self.managed = false,
                KeyCode::Char('m') => self.managed = true,
                KeyCode::Enter => {
                    let dir = self.target().to_owned();
                    if !self.managed {
                        return LaunchAction::Interactive(dir, self.kind());
                    }
                    // A managed run compiles a policy; a harness without an adapter fails at
                    // validation, so say so here rather than as a failed row in the ledger.
                    match harness::adapter(self.kind()) {
                        Ok(_) => self.step = Step::Prompt,
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                KeyCode::Backspace => self.step = Step::Harness,
                _ => {}
            },
            Step::Prompt => match code {
                KeyCode::Enter => {
                    let prompt = self.prompt.trim().to_owned();
                    if !prompt.is_empty() {
                        return LaunchAction::Managed(
                            self.target().to_owned(),
                            self.kind(),
                            prompt,
                        );
                    }
                }
                KeyCode::Backspace if self.prompt.is_empty() => self.step = Step::How,
                KeyCode::Backspace => {
                    self.prompt.pop();
                }
                KeyCode::Char(c) if !ctrl => self.prompt.push(c),
                _ => {}
            },
        }
        LaunchAction::Stay
    }

    /// The footer line: what is asked, the answer so far, the choices with the current one lit,
    /// the inline error when there is one.
    fn line(&self) -> Line<'static> {
        let ask = Style::default().fg(ORANGE);
        let lit = Style::default().fg(ORANGE).add_modifier(Modifier::BOLD);
        let choices = |spans: &mut Vec<Span<'static>>, options: &[&str], picked: usize| {
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
            spans.push(Span::styled("  ←→ pick · enter next · esc cancel", dim()));
        };
        let mut spans = vec![Span::styled("new task", ask)];
        match self.step {
            Step::Dir => {
                spans.push(Span::styled(" · dir › ", ask));
                if self.dir.is_empty() {
                    spans.push(Span::styled(fleet::tilde(&self.fallback), dim()));
                } else {
                    spans.push(Span::raw(self.dir.clone()));
                }
                spans.push(Span::styled("▏", dim()));
            }
            Step::Harness => {
                spans.push(Span::styled(
                    format!(" in {} · harness › ", fleet::tilde(self.target())),
                    ask,
                ));
                let names: Vec<String> = harness::KNOWN
                    .iter()
                    .map(|h| logo(&h.to_string()))
                    .collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                choices(&mut spans, &names, self.harness);
            }
            Step::How => {
                spans.push(Span::styled(
                    format!(
                        " in {} · {} · start › ",
                        fleet::tilde(self.target()),
                        logo(&self.kind().to_string())
                    ),
                    ask,
                ));
                choices(
                    &mut spans,
                    &["interactive", "managed"],
                    usize::from(self.managed),
                );
            }
            Step::Prompt => {
                spans.push(Span::styled(
                    format!(
                        " in {} · {} managed › ",
                        fleet::tilde(self.target()),
                        logo(&self.kind().to_string())
                    ),
                    ask,
                ));
                spans.push(Span::raw(self.prompt.clone()));
                spans.push(Span::styled("▏", dim()));
            }
        }
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
    Launch(Launch),
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
    details: Vec<String>,
    /// `tab` cycles the pane: hidden, a peek at the bottom of the screen, then most of it
    /// with a session showing `MORE` exchanges.
    pane: Pane,
    /// Pane lines hidden below the bottom edge: 0 pins the pane to the end of the transcript so
    /// a working session keeps scrolling by itself; paging up raises it.
    pane_scroll: usize,
    /// Rows the pane had at the last draw, so a page is a screenful.
    pane_height: usize,
    tick: usize,
    refreshed: Instant,
    /// A reload in flight on its own thread; the loop applies it when it lands, so a slow read
    /// never holds the spinner or a keypress.
    loading: Option<mpsc::Receiver<Result<Data>>>,
    /// A run id and when ctrl-x was first pressed on it; the second press within two seconds stops it.
    armed: Option<(String, Instant)>,
    /// `cones tui --debug`: every terminal hand-off and input event is appended here.
    log: Option<PathBuf>,
    suspended: Vec<Suspended>,
}

/// A harness launched with `n` and parked by ctrl-z: stopped in its own process group, off the
/// terminal, until `enter` on its row brings it back or quitting kills it.
struct Suspended {
    child: Child,
    what: String,
    harness: String,
    cwd: PathBuf,
}

/// What a foreground child stopping on ctrl-z means.
#[derive(Clone, PartialEq, Eq)]
enum OnStop {
    /// A viewer (attach client, log follower): killed, the dashboard is back at once.
    Kill,
    /// The editor: resumed, so ctrl-z is a no-op and unsaved edits are safe.
    Resume,
    /// A harness launched here: parked as a `suspended` row, the dashboard is back at once.
    Suspend { harness: String, cwd: PathBuf },
}

enum Waited {
    Exited(std::process::ExitStatus),
    Stopped,
}

/// What `foreground` hands the terminal to: a new command, or a suspended child coming back.
enum Start {
    Spawn(Command),
    Resume(Child),
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
            details: vec![],
            pane: Pane::Hidden,
            pane_scroll: 0,
            pane_height: 0,
            tick: 0,
            refreshed: Instant::now(),
            loading: None,
            armed: None,
            log: None,
            suspended: vec![],
        })
    }

    /// Append one timestamped line to the debug log, if `--debug` named one.
    fn debug(&self, msg: impl FnOnce() -> String) {
        if let Some(path) = &self.log {
            debug_line(path, msg());
        }
    }

    fn selected(&self) -> Option<&Row> {
        self.visible.get(self.cursor).map(|&i| &self.rows[i])
    }

    /// Reload and stay on the selected row, found again by its key: a session whose state
    /// changed is still the same row, a row that is gone leaves the cursor at its position, on
    /// the neighbor. The filter and the grouping are fields, so a reload never touches them.
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
        std::thread::spawn(move || {
            let _ = tx.send(Data::load(&jobs, &state, &claude));
        });
        self.loading = Some(rx);
    }

    /// Apply a finished reload, if one has landed. A failed read shows in the status line and
    /// the last good data stays on screen.
    fn poll(&mut self) {
        // A suspended harness that ended on its own (killed elsewhere, or continued and quit)
        // leaves the list, or its row would promise a resume that cannot happen.
        let mut i = 0;
        while i < self.suspended.len() {
            match self.suspended[i].child.try_wait() {
                Ok(Some(st)) => {
                    let s = self.suspended.remove(i);
                    self.debug(|| format!("suspended {} ended on its own: {st}", s.what));
                    self.status = format!("{} ended while suspended", s.what);
                }
                _ => i += 1,
            }
        }
        let Some(rx) = &self.loading else {
            return;
        };
        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return,
            Ok(Ok(data)) => self.apply(data),
            Ok(Err(e)) => self.status = format!("reload failed: {e:#}"),
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        self.loading = None;
    }

    fn apply(&mut self, data: Data) {
        let keep = self
            .selected()
            .and_then(|r| r.kind.key().map(str::to_owned));
        self.data = data;
        // A suspended harness is a session row in state `suspended`: the fleet's own row for
        // its pid when it lists one (Codex, from the process table), a stand-in until then.
        for s in &self.suspended {
            let pid = s.child.id();
            match self.data.sessions.iter_mut().find(|x| x.pid == Some(pid)) {
                Some(x) => x.state = "suspended".into(),
                None => self.data.sessions.push(Session {
                    session_id: format!("{}-{pid}", s.harness),
                    harness: s.harness.clone(),
                    kind: None,
                    cwd: s.cwd.clone(),
                    state: "suspended".into(),
                    started: None,
                    last_activity: None,
                    model: None,
                    pid: Some(pid),
                    transcript_path: None,
                    tokens_in: None,
                    tokens_out: None,
                    context_tokens: None,
                    cost_usd: None,
                    title: None,
                    last: Some("parked by ctrl-z".into()),
                }),
            }
        }
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
        self.refreshed = Instant::now();
    }

    /// The pane lines on screen: `[from, to)` of `details`, from the end less `pane_scroll`.
    fn window(&self) -> (usize, usize) {
        let max = self.details.len().saturating_sub(self.pane_height);
        let from = max - self.pane_scroll.min(max);
        (from, (from + self.pane_height).min(self.details.len()))
    }

    /// Page the pane: up towards the start of the transcript, down back to its end.
    fn scroll_pane(&mut self, pages: isize) {
        let max = self.details.len().saturating_sub(self.pane_height) as isize;
        let by = self.pane_height.max(1) as isize;
        self.pane_scroll = (self.pane_scroll as isize + pages * by).clamp(0, max) as usize;
    }

    /// `tab`: hidden, peek, more, hidden again.
    fn toggle_more(&mut self) {
        self.pane = match self.pane {
            Pane::Hidden => Pane::Peek,
            Pane::Peek => Pane::More,
            Pane::More => Pane::Hidden,
        };
        self.pane_scroll = 0;
        self.settle();
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
            self.details.clear();
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
        let depth = if self.pane == Pane::More { MORE } else { 1 };
        self.details = self
            .selected()
            .map(|r| self.data.details(&r.kind, depth))
            .unwrap_or_default();
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
        // A new row reads from its end, whatever the last one was scrolled to.
        self.pane_scroll = 0;
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
    /// Hand the terminal to a child, take it back when it exits or stops, and surface its last
    /// stderr line. The child runs in its own process group and owns the tty, like a shell job,
    /// so ctrl-c and ctrl-z reach it and everything it forked, and nothing else; `on_stop` says
    /// what a stop means. `Start::Resume` brings a suspended child back instead of spawning.
    fn foreground(
        &mut self,
        terminal: &mut DefaultTerminal,
        start: Start,
        what: &str,
        on_stop: OnStop,
    ) {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        let height = terminal.size().map(|s| s.height).unwrap_or(0);
        self.debug(|| {
            let start = match &start {
                Start::Spawn(c) => format!("{c:?} in {:?}", c.get_current_dir()),
                Start::Resume(c) => format!("resume pid {}", c.id()),
            };
            format!(
                "foreground {what}: {start}; size={:?}; {}",
                terminal.size().ok(),
                term_state()
            )
        });
        ratatui::restore();
        self.debug(|| format!("restored; {}", term_state()));
        // The frame stays on the normal screen: the child starts over it, and on the way out
        // leaves its own screen to it, so neither gap shows the shell. Erased before the
        // dashboard is back.
        let mut still = Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .ok();
        if let Some(t) = still.as_mut() {
            let _ = t.draw(|f| self.draw(f));
        }
        // ponytail: ctrl-c must reach only the child; the dashboard ignores it while waiting.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        let spawned = match start {
            Start::Spawn(mut c) => {
                // A harness that may be suspended keeps its own stderr: the pipe would outlive
                // the wait, and a TUI that finds fd 2 is not a tty may misbehave.
                if !matches!(on_stop, OnStop::Suspend { .. }) {
                    c.stderr(Stdio::piped());
                }
                unsafe {
                    c.pre_exec(|| {
                        libc::setpgid(0, 0);
                        libc::tcsetpgrp(0, libc::getpid());
                        for sig in [libc::SIGINT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN] {
                            libc::signal(sig, libc::SIG_DFL);
                        }
                        Ok(())
                    });
                }
                c.spawn()
            }
            Start::Resume(c) => {
                let pid = c.id() as libc::pid_t;
                unsafe {
                    libc::tcsetpgrp(0, pid);
                    libc::kill(-pid, libc::SIGCONT);
                }
                Ok(c)
            }
        };
        let started = Instant::now();
        let r = spawned.and_then(|mut c| {
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
            let waited = loop {
                let w = wait_or_stopped(&mut c, on_stop == OnStop::Kill, &|m| self.debug(|| m));
                match w {
                    // The child still owns the tty; it only needs to run again.
                    Ok(Waited::Stopped) if on_stop == OnStop::Resume => unsafe {
                        libc::kill(-pid, libc::SIGCONT);
                    },
                    w => break w,
                }
            };
            if let Some((stop, t)) = watch {
                stop.store(true, Ordering::Relaxed);
                let _ = t.join();
            }
            let stderr = match &waited {
                Ok(Waited::Exited(_)) => stderr.and_then(|t| t.join().ok()).unwrap_or_default(),
                _ => vec![],
            };
            waited.map(|w| (c, w, stderr))
        });
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        match &r {
            Ok((_, Waited::Exited(st), err)) => self.debug(|| {
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
            Ok((c, Waited::Stopped, _)) => self.debug(|| {
                format!(
                    "child pid {} suspended after {:?}; {}",
                    c.id(),
                    started.elapsed(),
                    term_state()
                )
            }),
            Err(e) => self.debug(|| format!("child failed: {e}; {}", term_state())),
        }
        if let Some(t) = still.as_mut() {
            let _ = t.clear();
        }
        *terminal = ratatui::init();
        self.debug(|| {
            format!(
                "dashboard back: size={:?}; {}",
                terminal.size().ok(),
                term_state()
            )
        });
        self.status = match r {
            Ok((child, Waited::Stopped, _)) => {
                if let OnStop::Suspend { harness, cwd } = on_stop {
                    self.suspended.push(Suspended {
                        child,
                        what: what.to_owned(),
                        harness,
                        cwd,
                    });
                }
                format!("{what} suspended · enter on its row to return")
            }
            Ok((_, Waited::Exited(st), _)) if st.success() => format!("back from {what}"),
            Ok((_, Waited::Exited(st), err)) => {
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

    /// SIGKILL every suspended harness, process group and all. Left stopped with no dashboard
    /// to resume them, they would sit in the process table forever.
    fn kill_suspended(&mut self) {
        for s in &mut self.suspended {
            unsafe {
                libc::kill(-(s.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = s.child.wait();
        }
        self.suspended.clear();
    }

    /// `q` with suspended harnesses arms, `q` again within two seconds kills them and quits.
    fn quit(&mut self) -> bool {
        if self.suspended.is_empty() {
            return true;
        }
        match self.armed.take() {
            Some((k, at)) if k == "quit" && at.elapsed() < Duration::from_secs(2) => {
                self.kill_suspended();
                true
            }
            _ => {
                let names: Vec<&str> = self.suspended.iter().map(|s| s.what.as_str()).collect();
                self.status = format!(
                    "{} suspended · q again kills it and quits",
                    names.join(", ")
                );
                self.armed = Some(("quit".into(), Instant::now()));
                false
            }
        }
    }

    fn enter(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(kind) = self.selected().map(|r| r.kind.clone()) else {
            return Ok(());
        };
        self.debug(|| format!("enter on {:?}: {}", kind.key(), enter_verb(Some(&kind))));
        match kind {
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("started {name}")),
            // A headless run cannot be attached while it runs; follow its log instead. A live
            // session attaches natively, ctrl-z comes back here.
            Kind::Run(id, s) if s == "started" => {
                let mut c = self.me();
                c.args(["logs", &id, "--follow"]);
                self.foreground(terminal, Start::Spawn(c), "logs", OnStop::Kill)
            }
            // A listed session is live, so `claude attach` runs straight from here; the
            // `cones attach` helper, which reloads the whole fleet first, is for finished runs.
            Kind::Session(id, _) => {
                let Some(s) = self.data.sessions.iter().find(|s| s.session_id == id) else {
                    return Ok(());
                };
                let (harness, cwd, pid) = (s.harness.clone(), s.cwd.clone(), s.pid);
                // A harness parked here by ctrl-z comes back the way it left.
                if let Some(i) =
                    pid.and_then(|p| self.suspended.iter().position(|x| x.child.id() == p))
                {
                    let s = self.suspended.remove(i);
                    let on_stop = OnStop::Suspend {
                        harness: s.harness,
                        cwd: s.cwd,
                    };
                    self.foreground(terminal, Start::Resume(s.child), &s.what, on_stop);
                    self.reload();
                    return Ok(());
                }
                if harness != "claude" {
                    self.status = format!(
                        "{harness} sessions started elsewhere cannot be opened here; n launches one that can"
                    );
                    return Ok(());
                }
                match harness::adapter(HarnessKind::Claude)?.attach(&id, &cwd) {
                    Ok(c) => self.foreground(terminal, Start::Spawn(c), "attach", OnStop::Kill),
                    Err(e) => self.status = format!("attach failed: {e:#}"),
                }
                // Back on the same row, read again by id: the session may have changed state,
                // or ended, while it was open. Filter and grouping were never touched.
                self.reload();
            }
            Kind::Run(id, _) => {
                let mut c = self.me();
                c.args(["attach", &id]);
                self.foreground(terminal, Start::Spawn(c), "attach", OnStop::Kill);
                self.reload();
            }
            _ => {}
        }
        Ok(())
    }

    /// `e`: jobs.yaml in $VISUAL or $EDITOR, then `cones install` so launchd matches the file.
    /// Jobs are the owner's file, so the dashboard never rewrites yaml itself; comments survive.
    fn edit_jobs(&mut self, terminal: &mut DefaultTerminal) {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg("exec ${VISUAL:-${EDITOR:-vi}} \"$0\"")
            .arg(&self.jobs_path);
        self.foreground(terminal, Start::Spawn(c), "editor", OnStop::Resume);
        let r = self.me().arg("install").output();
        self.status = match r {
            Ok(o) if o.status.success() => "jobs.yaml saved · launchd reinstalled".into(),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let last = err.lines().rev().find(|l| !l.trim().is_empty());
                format!(
                    "install failed: {}",
                    last.unwrap_or("").trim_start_matches("Error: ")
                )
            }
            Err(e) => format!("install failed: {e}"),
        };
    }

    /// ctrl-x once arms, ctrl-x again within two seconds stops: the `claude agents` convention.
    fn stop(&mut self) {
        let id = match self.selected().map(|r| r.kind.clone()) {
            Some(Kind::Session(id, _) | Kind::Run(id, _)) => id,
            // A job row means its run in flight; a job itself is edited with `e`, not stopped.
            Some(Kind::Job(name)) => {
                let live =
                    self.data.runs.iter().rev().find(|r| {
                        r.started.job.as_deref() == Some(&name) && r.status() == "started"
                    });
                match live {
                    Some(r) => r.started.run_id.clone(),
                    None => {
                        self.status = format!("{name} has no run in flight · e edits jobs.yaml");
                        return;
                    }
                }
            }
            _ => {
                self.status = "select a job, run or session to stop".into();
                return;
            }
        };
        match self.armed.take() {
            Some((armed, at)) if armed == id && at.elapsed() < Duration::from_secs(2) => {
                self.status = match Ledger::new(&self.state)
                    .and_then(|l| runner::stop(&l, &self.claude, &id))
                {
                    Ok(true) => "stop requested".into(),
                    Ok(false) => "already finished".into(),
                    Err(e) => format!("stop failed: {e:#}"),
                };
            }
            _ => {
                self.armed = Some((id, Instant::now()));
                self.status = "x again to stop this run".into();
            }
        }
    }

    /// Returns true when the dashboard should exit.
    fn key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let shift = mods.contains(KeyModifiers::SHIFT);
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
            Mode::Launch(launch) => match launch.key(code, ctrl) {
                LaunchAction::Stay => {}
                LaunchAction::Cancel => self.mode = Mode::Normal,
                // The harness natively in the directory; the dashboard waits and takes the
                // terminal back. Claude runs as a background session under a viewer, so
                // leaving the viewer keeps it in the fleet; a Codex is a foreground process
                // that ctrl-z parks as a `suspended` row.
                LaunchAction::Interactive(dir, kind) => {
                    self.mode = Mode::Normal;
                    let what = format!("{kind} in {}", fleet::tilde(&dir));
                    let on_stop = match kind {
                        HarnessKind::Claude => OnStop::Kill,
                        _ => OnStop::Suspend {
                            harness: kind.to_string(),
                            cwd: dir.clone(),
                        },
                    };
                    match harness::interactive(kind, &dir) {
                        Ok(c) => self.foreground(terminal, Start::Spawn(c), &what, on_stop),
                        Err(e) => self.status = format!("{what} failed: {e}"),
                    }
                    self.reload();
                }
                // The same `cones run --prompt` the dashboard has always dispatched, with the
                // subprocess's cwd set to the chosen directory.
                LaunchAction::Managed(dir, _, prompt) => {
                    self.mode = Mode::Normal;
                    let label = format!(
                        "dispatched in {}: {}",
                        fleet::tilde(&dir),
                        clip(&prompt, 60)
                    );
                    self.spawn(&["run", "--prompt", &prompt], Some(&dir), &label);
                }
            },
            Mode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(self.quit()),
                KeyCode::Char('c') if ctrl => return Ok(self.quit()),
                KeyCode::PageUp => self.scroll_pane(1),
                KeyCode::PageDown => self.scroll_pane(-1),
                KeyCode::Up if shift => self.scroll_pane(1),
                KeyCode::Down if shift => self.scroll_pane(-1),
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Tab => self.toggle_more(),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('a') => self.enter(terminal)?,
                KeyCode::Char('x') => self.stop(),
                KeyCode::Char('e') => self.edit_jobs(terminal),
                KeyCode::Char('s') => {
                    self.by_state = !self.by_state;
                    self.refresh()?;
                }
                KeyCode::Char('n') => {
                    let fallback = self.selected_cwd().unwrap_or_else(|| self.cwd.clone());
                    self.mode = Mode::Launch(Launch::new(&self.cwd, &fallback));
                }
                KeyCode::Char('/') => self.mode = Mode::Filter,
                KeyCode::Char('r') => {
                    self.refresh()?;
                    self.status = "refreshed".into();
                }
                _ => {}
            },
        }
        Ok(false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        // Hidden, the list has the screen; expanded, the pane takes most of it and the list
        // keeps the cursor in view.
        let pane_size = match self.pane {
            Pane::Hidden => 0,
            Pane::Peek => 40,
            Pane::More => 75,
        };
        let [head, list, pane, foot] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Percentage(pane_size),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let enter = enter_verb(self.selected().map(|r| &r.kind));
        frame.render_widget(
            Paragraph::new(header_lines(self.data.summary(), enter, self.pane)),
            head,
        );
        self.draw_list(frame, list);
        let mut title = self
            .selected()
            .map(|r| r.text().trim().to_owned())
            .unwrap_or_default();
        self.pane_height = pane.height.saturating_sub(1) as usize;
        let (from, to) = self.window();
        // Reading rather than glancing: the title says where in the transcript the pane is.
        if (self.pane == Pane::More || self.pane_scroll > 0) && to > from {
            title = format!(
                "{title} · lines {}-{to} of {} · pgup pgdn scroll",
                from + 1,
                self.details.len()
            );
        }
        let lines: Vec<Line> = self.details[from..to]
            .iter()
            .map(|l| Line::raw(l.as_str()))
            .collect();
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::new()
                    .borders(Borders::TOP)
                    .border_style(dim())
                    .title(Span::styled(format!(" {title} "), dim())),
            ),
            pane,
        );
        let footer = match &self.mode {
            Mode::Filter => Line::from(vec![
                Span::styled("/", bold()),
                Span::raw(self.filter.clone()),
                Span::styled("▏", dim()),
            ]),
            Mode::Launch(l) => l.line(),
            Mode::Normal if !self.filter.is_empty() => Line::from(vec![
                Span::styled(format!("filter: {}  ", self.filter), dim()),
                Span::styled(self.status.clone(), dim()),
            ]),
            Mode::Normal => Line::styled(self.status.clone(), dim()),
        };
        frame.render_widget(Paragraph::new(footer), foot);
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
                let mut spans = Vec::with_capacity(row.cells.len() + 1);
                if row.kind.selectable() {
                    spans.push(Span::styled(
                        if selected { "▌ " } else { "  " },
                        Style::default().fg(ORANGE),
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
                    spans.push(Span::styled(text, style));
                }
                let line = Line::from(spans);
                if selected { line.style(bold()) } else { line }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

pub fn run(exe: &Path, jobs_path: &Path, state: &Path, claude: &Path, debug: bool) -> Result<i32> {
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
    app.refresh()?;
    // Raw mode makes ctrl-z a key, but a child that has just restored the terminal and exited
    // leaves a gap in which ctrl-z is SIGTSTP to the whole foreground group; ignored, it cannot
    // suspend the dashboard from under the user. Children get the default back in pre_exec.
    // A foreground child owns the tty as its own process group; taking it back with tcsetpgrp
    // from the background is SIGTTOU unless ignored.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            if app.refreshed.elapsed() >= Duration::from_secs(1) {
                app.reload();
            }
            app.poll();
            terminal.draw(|f| app.draw(f))?;
            // ponytail: one poll cadence drives both the spinner and input.
            if event::poll(Duration::from_millis(100))? {
                let e = event::read()?;
                app.debug(|| format!("event {e:?}"));
                if let Event::Key(k) = e
                    && k.kind == KeyEventKind::Press
                    && app.key(k.code, k.modifiers, &mut terminal)?
                {
                    return Ok(());
                }
            } else {
                app.tick += 1;
                if matches!(app.armed, Some((_, at)) if at.elapsed() >= Duration::from_secs(2)) {
                    app.armed = None;
                    app.status.clear();
                }
            }
        }
    })();
    app.debug(|| format!("dashboard loop ended: {result:?}"));
    app.kill_suspended();
    ratatui::restore();
    result.context("dashboard")?;
    Ok(0)
}

/// Wait for the child that holds the terminal. Ctrl-z in a child that leaves ISIG on (Codex,
/// vi and Claude's agents view do; interactive Claude and its attach view eat the key) stops the
/// child's process group. `Child::wait` would then block forever on a cooked terminal nobody
/// reads. With `kill_on_stop` the child is a viewer that has already restored the tty: it is
/// killed where it stands, group and all, and the session it showed is untouched. Otherwise the
/// stop is reported and the caller decides: resume it, or park it as a suspended row.
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
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// The terminal facts a hand-off can corrupt: the tty's line discipline, who owns the
/// foreground, and what ctrl-z and ctrl-c do to this process.
fn term_state() -> String {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        let tty = if libc::tcgetattr(0, &mut t) == 0 {
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
            "{tty} fg_pgrp={} pgrp={} tstp={} int={} raw={:?}",
            libc::tcgetpgrp(0),
            libc::getpgrp(),
            disposition(libc::SIGTSTP),
            disposition(libc::SIGINT),
            ratatui::crossterm::terminal::is_raw_mode_enabled().ok()
        )
    }
}

/// While a child holds the terminal, log the dashboard's process group and the child's (its own,
/// shared with what it forks) and the tty's foreground group once a second: `T` in the state
/// column is a stopped child.
fn watch_group(log: PathBuf, child: u32) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let pgrp = format!("{},{child}", unsafe { libc::getpgrp() });
    let t = std::thread::spawn(move || {
        let mut last = String::new();
        while !flag.load(Ordering::Relaxed) {
            let out = Command::new("ps")
                .args(["-o", "pid=,ppid=,stat=,tpgid=,command=", "-g", &pgrp])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
                .unwrap_or_else(|e| format!("ps failed: {e}"));
            // ponytail: only changes are logged, so an idle attach costs one line.
            if out != last {
                debug_line(
                    &log,
                    format!("child tree:\n{out}\n  fg_pgrp={}", unsafe {
                        libc::tcgetpgrp(0)
                    }),
                );
                last = out;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    (stop, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn typed(l: &mut Launch, text: &str) {
        for c in text.chars() {
            assert_eq!(l.key(KeyCode::Char(c), false), LaunchAction::Stay);
        }
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
    fn a_suspended_harness_is_a_row_enter_resumes_and_quit_asks_twice_then_kills() {
        let d = dir();
        let mut app = App::new(
            Path::new("cones"),
            &d.path().join("jobs.yaml"),
            d.path(),
            d.path(),
        )
        .unwrap();
        let child = job("kill -STOP $$; sleep 30");
        let pid = child.id();
        app.suspended.push(Suspended {
            child,
            what: "codex in ~/x".into(),
            harness: "codex".into(),
            cwd: PathBuf::from("/x"),
        });
        app.refresh().unwrap();
        let row = app
            .rows
            .iter()
            .find(|r| matches!(&r.kind, Kind::Session(id, _) if id == &format!("codex-{pid}")))
            .expect("a stand-in row for the parked harness");
        assert!(matches!(&row.kind, Kind::Session(_, s) if s == "suspended"));
        assert_eq!(enter_verb(Some(&row.kind)), "resume");
        assert!(!app.quit(), "first q arms");
        assert!(app.status.contains("codex in ~/x suspended"));
        assert!(app.quit(), "second q quits");
        assert!(app.suspended.is_empty());
        let ps = Command::new("ps")
            .args(["-o", "pid=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(ps.stdout.trim_ascii().is_empty(), "killed on quit");
        // Reaped on its own when it ends elsewhere.
        let child = job("exit 0");
        app.suspended.push(Suspended {
            child,
            what: "w".into(),
            harness: "codex".into(),
            cwd: PathBuf::from("/x"),
        });
        std::thread::sleep(Duration::from_millis(200));
        app.poll();
        assert!(app.suspended.is_empty(), "ended children leave the list");
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
    fn empty_dir_means_the_fallback_and_interactive_launches_there() {
        let base = dir();
        let mut l = Launch::new(base.path(), base.path());
        assert_eq!(l.step, Step::Dir);
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::Harness);
        assert_eq!(l.kind(), HarnessKind::Claude);
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::How);
        assert!(!l.managed);
        assert_eq!(
            l.key(KeyCode::Enter, false),
            LaunchAction::Interactive(base.path().canonicalize().unwrap(), HarnessKind::Claude)
        );
    }

    #[test]
    fn a_bad_directory_stays_on_the_question_with_an_inline_error() {
        let base = dir();
        let mut l = Launch::new(base.path(), base.path());
        typed(&mut l, "nope");
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::Dir);
        assert!(l.error.as_deref().unwrap().starts_with("not a directory: "));
        assert!(l.line().to_string().contains("not a directory"));
        // The next key clears the error; a corrected path goes through.
        for _ in 0..4 {
            l.key(KeyCode::Backspace, false);
        }
        assert_eq!(l.error, None);
        std::fs::create_dir(base.path().join("ok")).unwrap();
        typed(&mut l, "ok");
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::Harness);
        assert_eq!(
            l.target(),
            base.path().join("ok").canonicalize().unwrap().as_path()
        );
    }

    #[test]
    fn managed_asks_for_a_prompt_and_dispatches_it() {
        let base = dir();
        let mut l = Launch::new(base.path(), base.path());
        l.key(KeyCode::Enter, false);
        l.key(KeyCode::Enter, false);
        assert_eq!(l.key(KeyCode::Right, false), LaunchAction::Stay);
        assert!(l.managed);
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::Prompt);
        // An empty prompt does not dispatch.
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::Prompt);
        typed(&mut l, " fix the test ");
        assert_eq!(
            l.key(KeyCode::Enter, false),
            LaunchAction::Managed(
                base.path().canonicalize().unwrap(),
                HarnessKind::Claude,
                "fix the test".into()
            )
        );
    }

    #[test]
    fn harness_cycles_through_the_known_list_and_managed_codex_is_refused_inline() {
        let base = dir();
        let mut l = Launch::new(base.path(), base.path());
        l.key(KeyCode::Enter, false);
        assert_eq!(l.kind(), harness::KNOWN[0]);
        l.key(KeyCode::Right, false);
        assert_eq!(l.kind(), HarnessKind::Codex);
        l.key(KeyCode::Right, false);
        assert_eq!(l.kind(), HarnessKind::Claude, "wraps around");
        l.key(KeyCode::Left, false);
        assert_eq!(l.kind(), HarnessKind::Codex);
        assert!(l.line().to_string().contains("[>_ codex]"));
        l.key(KeyCode::Enter, false);
        assert_eq!(
            l.key(KeyCode::Enter, false),
            LaunchAction::Interactive(base.path().canonicalize().unwrap(), HarnessKind::Codex),
            "interactive needs no adapter"
        );
        l.key(KeyCode::Char('m'), false);
        assert!(l.managed);
        assert_eq!(l.key(KeyCode::Enter, false), LaunchAction::Stay);
        assert_eq!(l.step, Step::How, "no adapter, so no prompt step");
        assert!(l.error.as_deref().unwrap().contains("not available"));
    }

    #[test]
    fn esc_cancels_anywhere_and_backspace_steps_back() {
        let base = dir();
        let mut l = Launch::new(base.path(), base.path());
        l.key(KeyCode::Enter, false);
        l.key(KeyCode::Enter, false);
        l.key(KeyCode::Char('m'), false);
        l.key(KeyCode::Enter, false);
        assert_eq!(l.step, Step::Prompt);
        typed(&mut l, "a");
        l.key(KeyCode::Backspace, false);
        assert_eq!(l.step, Step::Prompt, "backspace edits text first");
        l.key(KeyCode::Backspace, false);
        assert_eq!(l.step, Step::How);
        l.key(KeyCode::Backspace, false);
        assert_eq!(l.step, Step::Harness);
        l.key(KeyCode::Backspace, false);
        assert_eq!(l.step, Step::Dir);
        assert_eq!(l.key(KeyCode::Esc, false), LaunchAction::Cancel);
        // Control characters never reach the text.
        let mut l = Launch::new(base.path(), base.path());
        l.key(KeyCode::Char('c'), true);
        assert_eq!(l.dir, "");
    }

    #[test]
    fn the_dir_question_shows_the_fallback_as_a_placeholder() {
        let base = dir();
        let l = Launch::new(base.path(), base.path());
        let text = l.line().to_string();
        assert!(text.starts_with("new task · dir › "));
        assert!(text.contains(&fleet::tilde(base.path())));
    }

    use std::fs;

    /// A live registry entry for this test process, so `ps` vouches for the pid.
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

    /// The trip through the harness is `foreground` then `refresh`; the terminal part cannot
    /// run under a test, the state part can. The filter and grouping are fields, the row is
    /// found again by id.
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

    #[test]
    fn tab_shows_more_of_the_transcript_and_the_pane_pages() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        let project = claude.join("projects/-src-one");
        fs::create_dir_all(&project).unwrap();
        let turn = |n: usize| {
            format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"prompt {n}\"}}}}\n{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"reply {n}\"}}]}}}}\n"
            )
        };
        fs::write(
            project.join(format!("{A}.jsonl")),
            (0..20).map(turn).collect::<String>(),
        )
        .unwrap();
        let mut app = app(claude);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        let prompts = |app: &App| app.details.iter().filter(|l| l.starts_with("> ")).count();
        assert_eq!(app.pane, Pane::Hidden, "the pane starts hidden");
        assert_eq!(prompts(&app), 1, "collapsed: the last exchange");
        assert!(app.details.contains(&"reply 19".to_owned()));
        app.toggle_more();
        assert_eq!(app.pane, Pane::Peek, "one tab peeks");
        assert_eq!(prompts(&app), 1);
        app.toggle_more();
        assert_eq!(app.pane, Pane::More);
        assert_eq!(prompts(&app), MORE, "expanded: the last {MORE} exchanges");
        assert!(app.details.contains(&"> prompt 8".to_owned()));
        assert!(!app.details.contains(&"> prompt 7".to_owned()));
        // Paging: the pane starts pinned to the end, pages up in screenfuls, clamps at the top
        // and comes back down; a reload keeps the place, moving rows resets it.
        let n = app.details.len();
        app.pane_height = 10;
        assert_eq!(app.window(), (n - 10, n));
        app.scroll_pane(1);
        assert_eq!(app.window(), (n - 20, n - 10));
        for _ in 0..50 {
            app.scroll_pane(1);
        }
        assert_eq!(app.window(), (0, 10), "clamped at the start");
        app.scroll_pane(-1);
        assert_eq!(app.window(), (10, 20));
        let place = app.pane_scroll;
        app.refresh().unwrap();
        assert_eq!(
            app.pane_scroll, place,
            "a reload does not yank the reader back"
        );
        app.step(1);
        assert_eq!(app.window(), (n - 10, n), "a new row reads from its end");
        app.scroll_pane(1);
        app.toggle_more();
        assert_eq!(app.pane, Pane::Hidden, "a third tab hides the pane again");
        assert_eq!(prompts(&app), 1);
        assert_eq!(app.pane_scroll, 0, "tab back pins to the end again");
        // A pane taller than the text shows all of it and cannot scroll.
        app.pane_height = 100;
        assert_eq!(app.window(), (0, app.details.len()));
        app.scroll_pane(1);
        assert_eq!(app.pane_scroll, 0);
    }

    #[test]
    fn hint_line_names_what_tab_does_next() {
        let text = |pane: Pane| {
            header_lines(Line::raw("s"), "attach", pane)[2]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(Pane::Hidden).contains("enter attach · tab peek · x x stop"));
        assert!(text(Pane::Peek).contains("enter attach · tab more · x x stop"));
        assert!(text(Pane::More).contains("enter attach · tab hide · x x stop"));
    }
}
