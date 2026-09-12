//! `cones tui` is the native dashboard: jobs, every live harness session grouped by directory
//! or by state, and runs, with a details pane, a dispatch prompt and the actions. ratatui draws;
//! cones supplies rows. `cones __list` prints the same rows as tab-separated text.
use crate::{
    config::{self, ResolvedJob},
    fleet::{self, Session},
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
    Blank,
    Job(String),
    /// Session id and state.
    Session(String, String),
    /// Run id and status.
    Run(String, String),
}

impl Kind {
    fn selectable(&self) -> bool {
        !matches!(self, Kind::Header | Kind::Blank)
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
}

impl Data {
    pub fn load(jobs_path: &Path, state: &Path) -> Result<Self> {
        let runs = Ledger::new(state)?.runs()?;
        let sessions = fleet_rows(state, &runs)?;
        Ok(Self {
            jobs: config::read_jobs(jobs_path).unwrap_or_default(),
            runs,
            sessions,
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
                        (format!("last: {last}"), color(&last)),
                    ]
                })
                .collect();
            for (j, cells) in self.jobs.iter().zip(table(cells)) {
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
                vec![
                    (icon(&s.state).into(), color(&s.state)),
                    (logo(&s.harness), brand(&s.harness)),
                    (
                        s.title
                            .clone()
                            .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
                        plain(),
                    ),
                    (label(&s.state).into(), color(&s.state)),
                    (fleet::age(s.updated), dim()),
                    (fleet::tokens(s), dim()),
                    (
                        if by_state {
                            fleet::tilde(&s.cwd)
                        } else {
                            s.last.as_deref().map(|l| clip(l, 100)).unwrap_or_default()
                        },
                        dim(),
                    ),
                ]
            })
            .collect();
        let mut current: Option<&String> = None;
        for ((key, s), cells) in flat.iter().zip(table(cells)) {
            if current != Some(key) {
                header(&mut out, if by_state { &key[1..] } else { key });
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
            for (r, cells) in runs.iter().zip(table(cells)) {
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
                        s.event.as_deref().unwrap_or(""),
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
pub fn list(jobs_path: &Path, state: &Path) -> Result<String> {
    let data = Data::load(jobs_path, state)?;
    let mut out = String::new();
    for line in header_lines(&data.summary()) {
        out += "hdr\t-\t";
        for span in line.spans {
            out += &ansi(&span.content, span.style);
        }
        out.push('\n');
    }
    for row in data.rows(false) {
        let (key, aux) = match &row.kind {
            Kind::Header | Kind::Blank => ("hdr".to_owned(), "-".to_owned()),
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

fn header_lines(summary: &str) -> Vec<Line<'static>> {
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
                "↑↓ move · enter attach / run · ctrl+x twice stop run · ctrl+s group by state / dir · esc quit  ·  cones: n new task · / filter · r refresh",
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

/// Which harness a session or job runs under, written the way each app writes itself: Claude
/// Code's ✻ banner mark, Codex's `>_` startup box title, pi's plain bold word (π is only its
/// window title).
fn logo(harness: &str) -> String {
    match harness {
        "claude" => "✻ claude".into(),
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

/// Sessions from the fleet directory, newest first. Sessions belonging to a ledger run
/// collapse into that run's row, and a session whose harness pid is gone is stale.
pub fn fleet_rows(state: &Path, runs: &[Run]) -> Result<Vec<Session>> {
    let owned: HashSet<&str> = runs
        .iter()
        .filter_map(|r| r.started.session_id.as_deref())
        .collect();
    Ok(fleet::sessions(state)?
        .into_iter()
        .filter(|s| !owned.contains(s.session_id.as_str()) && s.pid.is_none_or(fleet::alive))
        .collect())
}

enum Mode {
    Normal,
    Filter,
    Dispatch(String),
}

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
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
        self.data = Data::load(&self.jobs_path, &self.state)?;
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
    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        let rows = &self.rows;
        let matched: Vec<usize> = (0..rows.len())
            .filter(|&i| {
                !rows[i].kind.selectable()
                    || needle.is_empty()
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

    /// Start something in the background and forget it; the ledger and fleet files report back.
    fn spawn(&mut self, args: &[&str], what: &str) {
        let r = self
            .me()
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
    fn foreground(&mut self, terminal: &mut DefaultTerminal, args: &[&str], what: &str) {
        use std::os::unix::process::CommandExt;
        ratatui::restore();
        let mut c = self.me();
        c.args(args).stderr(Stdio::piped());
        // ponytail: ctrl-c must reach only the child; the dashboard ignores it while waiting.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            c.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
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
            Kind::Job(name) => self.spawn(&["run", &name], &format!("started {name}")),
            // A headless run cannot be attached while it runs; follow its log instead. A live
            // session attaches natively, ctrl-z comes back here.
            Kind::Run(id, s) if s == "started" => {
                self.foreground(terminal, &["logs", &id, "--follow"], "logs")
            }
            Kind::Session(id, _) | Kind::Run(id, _) => {
                self.foreground(terminal, &["attach", &id], "attach")
            }
            _ => {}
        }
    }

    /// ctrl-x once arms, ctrl-x again within two seconds stops: the `claude agents` convention.
    fn stop(&mut self) {
        let Some(Kind::Session(id, _) | Kind::Run(id, _)) = self.selected().map(|r| r.kind.clone())
        else {
            self.status = "select a run or session to stop".into();
            return;
        };
        match self.armed.take() {
            Some((armed, at)) if armed == id && at.elapsed() < Duration::from_secs(2) => {
                self.status = match Ledger::new(&self.state).and_then(|l| runner::stop(&l, &id)) {
                    Ok(true) => "stop requested".into(),
                    Ok(false) => "already finished".into(),
                    Err(e) => format!("stop failed: {e:#}"),
                };
            }
            _ => {
                self.armed = Some((id, Instant::now()));
                self.status = "ctrl-x again to stop this run".into();
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
            Mode::Dispatch(text) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let prompt = text.trim().to_owned();
                    self.mode = Mode::Normal;
                    if !prompt.is_empty() {
                        let label = format!("dispatched: {}", clip(&prompt, 60));
                        self.spawn(&["run", "--prompt", &prompt], &label);
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !ctrl => text.push(c),
                _ => {}
            },
            Mode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Char('c') if ctrl => return Ok(true),
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('a') => self.enter(terminal),
                KeyCode::Char('x') => self.stop(),
                KeyCode::Char('s') => {
                    self.by_state = !self.by_state;
                    self.refresh()?;
                }
                KeyCode::Char('n') => self.mode = Mode::Dispatch(String::new()),
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
        frame.render_widget(Paragraph::new(header_lines(&self.data.summary())), head);
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
            Mode::Dispatch(t) => Line::from(vec![
                Span::styled("new task in cwd › ", Style::default().fg(ORANGE)),
                Span::raw(t.clone()),
                Span::styled("▏", dim()),
            ]),
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

pub fn run(exe: &Path, jobs_path: &Path, state: &Path) -> Result<i32> {
    let mut app = App {
        exe: exe.to_owned(),
        jobs_path: jobs_path.to_owned(),
        state: state.to_owned(),
        data: Data::load(jobs_path, state)?,
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
