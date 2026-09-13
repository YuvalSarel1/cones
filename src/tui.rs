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
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const ORANGE: Color = Color::Indexed(208);
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

    pub fn summary(&self) -> String {
        format!(
            "{} working · {} need input · {} idle · {} jobs · {} runs",
            self.count("active"),
            self.count("blocked"),
            self.count("idle"),
            self.jobs.len(),
            self.runs.len()
        )
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
                    (
                        s.title
                            .clone()
                            .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
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

    /// The details pane for one row: a job's policy and prompt, a session's last prompt and reply,
    /// or a run's captured output.
    pub fn details(&self, kind: &Kind) -> Vec<String> {
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
                let mut out = vec![
                    fleet::tilde(&s.cwd),
                    format!(
                        "{} · {} {} · {} · {} tokens · pid {} · {}",
                        logo(&s.harness),
                        label(&s.state),
                        s.kind.as_deref().unwrap_or(""),
                        fleet::age(s.updated),
                        fleet::tokens(s),
                        s.pid.map(|p| p.to_string()).unwrap_or_default(),
                        s.session_id
                    ),
                    String::new(),
                ];
                if let Some(t) = &s.transcript_path {
                    out.extend(fleet::exchange(t));
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
    for line in header_lines(&data.summary(), enter_verb(None)) {
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

fn header_lines(summary: &str, enter: &str) -> Vec<Line<'static>> {
    let orange = Style::default().fg(ORANGE);
    let white = Style::default().fg(Color::White);
    vec![
        Line::from(vec![
            Span::styled("  ▲  ", orange),
            Span::raw("  "),
            Span::styled("cones", bold()),
        ]),
        Line::from(vec![
            Span::styled(" ▟█▙ ", white),
            Span::raw("  "),
            Span::raw(summary.to_owned()),
        ]),
        Line::from(vec![
            Span::styled("▟███▙", orange),
            Span::raw("  "),
            Span::styled(
                format!(
                    "↑↓ move · enter {enter} · x x stop · e edit jobs · s regroup · n new task · / filter · r refresh · q quit"
                ),
                dim(),
            ),
        ]),
    ]
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
/// since the group title no longer names it.
fn cell(column: &str, s: &Session, by_state: bool) -> (String, Style) {
    match column {
        "state" => (label(&s.state).into(), color(&s.state)),
        "age" => (fleet::age(s.updated), dim()),
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
/// resting, a warning cone wants a human.
fn icon(state: &str) -> &str {
    match state {
        "active" | "started" => "▲",
        "blocked" => "⚠",
        "idle" => "△",
        "exited" => "▵",
        "ok" => "✓",
        "skipped" => "–",
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

/// Sessions from Claude's registry, oldest first. Sessions belonging to a ledger run collapse
/// into that run's row.
pub fn fleet_rows(claude: &Path, runs: &[Run]) -> Result<Vec<Session>> {
    let owned: HashSet<&str> = runs
        .iter()
        .filter_map(|r| r.started.session_id.as_deref())
        .collect();
    Ok(fleet::sessions(claude)?
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
    tick: usize,
    refreshed: Instant,
    /// A run id and when ctrl-x was first pressed on it; the second press within two seconds stops it.
    armed: Option<(String, Instant)>,
}

impl App {
    fn selected(&self) -> Option<&Row> {
        self.visible.get(self.cursor).map(|&i| &self.rows[i])
    }

    fn refresh(&mut self) -> Result<()> {
        let keep = self.selected().map(|r| r.kind.clone());
        self.data = Data::load(&self.jobs_path, &self.state, &self.claude)?;
        self.rows = self.data.rows(self.by_state);
        self.apply_filter();
        if let Some(k) = keep
            && let Some(i) = self.visible.iter().position(|&i| self.rows[i].kind == k)
        {
            self.cursor = i;
        }
        self.settle();
        self.refreshed = Instant::now();
        Ok(())
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
        self.details = self
            .selected()
            .map(|r| self.data.details(&r.kind))
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
    fn foreground(&mut self, terminal: &mut DefaultTerminal, mut c: Command, what: &str) {
        use std::os::unix::process::CommandExt;
        ratatui::restore();
        c.stderr(Stdio::piped());
        // ponytail: ctrl-c must reach only the child; the dashboard ignores it while waiting.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            c.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                Ok(())
            });
        }
        let r = c.spawn().and_then(|c| c.wait_with_output());
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        *terminal = ratatui::init();
        self.status = match r {
            Ok(o) if o.status.success() => format!("back from {what}"),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let last = err.lines().rev().find(|l| !l.trim().is_empty());
                match last {
                    Some(l) => format!("{what} failed: {}", l.trim_start_matches("Error: ")),
                    None => format!("{what} exited with {}", o.status),
                }
            }
            Err(e) => format!("{what} failed: {e}"),
        };
    }

    fn enter(&mut self, terminal: &mut DefaultTerminal) {
        let Some(kind) = self.selected().map(|r| r.kind.clone()) else {
            return;
        };
        match kind {
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("started {name}")),
            // A headless run cannot be attached while it runs; follow its log instead. A live
            // session attaches natively, ctrl-z comes back here.
            Kind::Run(id, s) if s == "started" => {
                let mut c = self.me();
                c.args(["logs", &id, "--follow"]);
                self.foreground(terminal, c, "logs")
            }
            Kind::Session(id, _) | Kind::Run(id, _) => {
                let mut c = self.me();
                c.args(["attach", &id]);
                self.foreground(terminal, c, "attach")
            }
            _ => {}
        }
    }

    /// `e`: jobs.yaml in $VISUAL or $EDITOR, then `cones install` so launchd matches the file.
    /// Jobs are the owner's file, so the dashboard never rewrites yaml itself; comments survive.
    fn edit_jobs(&mut self, terminal: &mut DefaultTerminal) {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg("exec ${VISUAL:-${EDITOR:-vi}} \"$0\"")
            .arg(&self.jobs_path);
        self.foreground(terminal, c, "editor");
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
                // terminal back, as for attach.
                LaunchAction::Interactive(dir, kind) => {
                    self.mode = Mode::Normal;
                    let what = format!("{kind} in {}", fleet::tilde(&dir));
                    match harness::interactive(kind, &dir) {
                        Ok(c) => self.foreground(terminal, c, &what),
                        Err(e) => self.status = format!("{what} failed: {e}"),
                    }
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
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Char('c') if ctrl => return Ok(true),
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('a') => self.enter(terminal),
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
        let [head, list, pane, foot] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Percentage(40),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let enter = enter_verb(self.selected().map(|r| &r.kind));
        frame.render_widget(
            Paragraph::new(header_lines(&self.data.summary(), enter)),
            head,
        );
        self.draw_list(frame, list);
        let title = self
            .selected()
            .map(|r| r.text().trim().to_owned())
            .unwrap_or_default();
        let height = pane.height.saturating_sub(1) as usize;
        let skip = self.details.len().saturating_sub(height);
        let lines: Vec<Line> = self.details[skip..]
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

pub fn run(exe: &Path, jobs_path: &Path, state: &Path, claude: &Path) -> Result<i32> {
    let mut app = App {
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
        tick: 0,
        refreshed: Instant::now(),
        armed: None,
    };
    app.refresh()?;
    // Raw mode makes ctrl-z a key, but a child that has just restored the terminal and exited
    // leaves a gap in which ctrl-z is SIGTSTP to the whole foreground group; ignored, it cannot
    // suspend the dashboard from under the user. Children get the default back in pre_exec.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
    }
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            if app.refreshed.elapsed() >= Duration::from_secs(1) {
                app.refresh()?;
            }
            terminal.draw(|f| app.draw(f))?;
            // ponytail: one poll cadence drives both the spinner and input.
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()?
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
    ratatui::restore();
    result.context("dashboard")?;
    Ok(0)
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
}
