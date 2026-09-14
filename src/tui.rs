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
    viewer::{self, Viewer},
};
use anyhow::{Context, Result};
use ratatui::{
    Frame,
    backend::Backend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
            KeyEventKind, KeyModifiers, MouseEvent,
        },
        execute,
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{OnceLock, mpsc},
    time::{Duration, Instant},
};

/// The tty as the shell handed it over, read once at start. Crossterm snapshots the tty at
/// every raw-mode entry and hands that snapshot back on exit; after a child, the snapshot is
/// whatever the child left, so a child that died raw would reach the shell through it.
static SHELL_TTY: OnceLock<Option<libc::termios>> = OnceLock::new();

/// Put the tty back the way the shell had it, after ratatui has left raw mode and the
/// alternate screen: the line discipline from `SHELL_TTY`, and off with every mode the
/// dashboard itself turns on (bracketed paste, mouse reports while a viewer wants them) and
/// every mode a child of an earlier build may have left on: focus events, colour-scheme
/// reports, kitty keys, modifyOtherKeys, synchronized output. A shell with them on echoes
/// garbage on every click, focus change and paste. Invisible on a terminal where nothing was
/// left on. Viewers never reach this terminal, so nothing of theirs is on it.
fn hand_back_tty() {
    if let Some(Some(t)) = SHELL_TTY.get() {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
    reset_terminal_protocols();
}

fn reset_terminal_protocols() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(
        b"\x18\x1b\\\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[<u\x1b[>4m\x1b(B\x1b[0m\x1b[?25h",
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
    /// A top-menu row: `runs`, `agents` or `folder`. From `App::rebuild`, never from `Data::rows`.
    Menu(&'static str),
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
            Kind::Menu(name) => Some(name),
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
        self.rows_excluding(by_state, &HashSet::new(), &mut Widths::new())
    }

    /// A confirmed delete leaves the list immediately while the harness command finishes.
    /// The source data stays intact so a failed command can restore its row.
    fn rows_excluding(
        &self,
        by_state: bool,
        deleting: &HashSet<&str>,
        widths: &mut Widths,
    ) -> Vec<Row> {
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
            let (names, cells) = columns(
                &["", "job", "schedule", "", "enabled", "last run"],
                cells,
                widths,
            );
            out.push(names);
            for (j, cells) in self.jobs.iter().zip(cells) {
                out.push(Row {
                    kind: Kind::Job(j.name.clone()),
                    cells,
                });
            }
        }
        let mut groups: BTreeMap<String, Vec<&Session>> = BTreeMap::new();
        for s in self
            .sessions
            .iter()
            .filter(|s| !deleting.contains(s.session_id.as_str()))
        {
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
        let (names, cells) = columns(&names, cells, widths);
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
                widths,
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
                // Model, start, last activity and context are the harness's words, `-` when it
                // has none; the window shows only when the harness stated one.
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
            Kind::Menu(m) => ("menu".to_owned(), (*m).to_owned()),
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
        Some(Kind::Menu("runs")) => "new job",
        Some(Kind::Menu("agents")) => "agents",
        Some(Kind::Menu(_)) => "pick folder",
        _ => "open",
    }
}

/// The top menu: three rows above the tables, reached with `↑` past the first table. `runs`
/// makes the composer a supervised one-off run, `agents` opens a harness's agents view, `folder`
/// picks the directory the menu works in, whether or not a session runs there.
fn menu_rows(folder: &Path) -> Vec<Row> {
    let dir = fleet::tilde(folder);
    let items = [
        (
            "runs",
            "enter opens the job wizard · an instruction runs once under a job's policy".to_owned(),
        ),
        (
            "agents",
            "enter opens a harness's own agents view".to_owned(),
        ),
        ("folder", format!("{dir} · enter picks another")),
    ];
    items
        .into_iter()
        .map(|(name, text)| Row {
            kind: Kind::Menu(name),
            cells: vec![(format!("{name:<8}"), plain()), (text, dim())],
        })
        .collect()
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

/// The widest each table's columns have been, keyed by its column names. A column only grows
/// for the life of the dashboard, so a cell that changes length (`59s` to `1m`, `working` to
/// `needs input`, a long title leaving) never moves the columns beside it.
pub type Widths = HashMap<Vec<String>, Vec<usize>>;

/// Pad each column to its widest cell, two spaces apart; `widths` remembers across frames.
fn table(rows: Vec<Vec<(String, Style)>>, widths: &mut Vec<usize>) -> Vec<Vec<(String, Style)>> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    widths.resize(widths.len().max(cols), 0);
    for (c, w) in widths.iter_mut().enumerate() {
        let now = rows
            .iter()
            .filter_map(|r| r.get(c))
            .map(|(t, _)| t.chars().count())
            .max()
            .unwrap_or(0);
        *w = (*w).max(now);
    }
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
fn columns(
    names: &[&str],
    rows: Vec<Vec<(String, Style)>>,
    widths: &mut Widths,
) -> (Row, Vec<Vec<(String, Style)>>) {
    let key: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let mut all = vec![names.iter().map(|n| ((*n).to_owned(), dim())).collect()];
    all.extend(rows);
    let mut all = table(all, widths.entry(key).or_default());
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

/// `ctrl+v` in the composer, as in Claude Code: the clipboard's image lands as a PNG under the
/// temp dir and its path is typed into the instruction, where the harness reads it as a file.
/// A terminal paste of text arrives as keys; only an image needs the clipboard itself.
fn paste_image() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("cones");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("pasted-{stamp}.png"));
    // ponytail: macOS clipboard via osascript; wl-paste/xclip if this ever runs on Linux.
    let open = format!(
        "set f to open for access POSIX file \"{}\" with write permission",
        path.display()
    );
    let out = Command::new("osascript")
        .args(["-e", &open])
        .args(["-e", "write (the clipboard as «class PNGf») to f"])
        .args(["-e", "close access f"])
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return Err("no image on the clipboard".into());
    }
    Ok(path)
}

/// Splice an attachment's path into the instruction, spaced from what is typed either side.
fn attach(text: &mut String, path: &Path) {
    if !text.is_empty() && !text.ends_with(' ') {
        text.push(' ');
    }
    text.push_str(&path.display().to_string());
    text.push(' ');
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

/// Tab in the folder prompt, as a shell completes `cd`: `text` grown to the longest prefix
/// every matching directory shares, with a `/` when only one is left, and the names that still
/// match when there are several. Hidden directories match only a `.` prefix.
pub fn complete_dir(text: &str, base: &Path) -> (String, Vec<String>) {
    let (parent, partial) = match text.rfind('/') {
        Some(i) => (&text[..=i], &text[i + 1..]),
        None => ("", text),
    };
    let dir = if parent.is_empty() {
        base.to_path_buf()
    } else {
        match crate::expand_path(Path::new(parent), base) {
            Ok(d) => d,
            Err(_) => return (text.to_string(), Vec::new()),
        }
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(partial) && (!n.starts_with('.') || partial.starts_with('.')))
        .collect();
    names.sort();
    match names.as_slice() {
        [] => (text.to_string(), names),
        [one] => (format!("{parent}{one}/"), Vec::new()),
        _ => {
            let first = &names[0];
            let common = first
                .char_indices()
                .find(|&(i, _)| {
                    !names
                        .iter()
                        .all(|n| n.get(..i + 1).is_some_and(|p| first.starts_with(p)))
                })
                .map_or(first.len(), |(i, _)| i);
            (format!("{parent}{}", &first[..common]), names)
        }
    }
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
    /// The menu's `folder` prompt: the path typed so far.
    Folder(String),
}

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
    claude: PathBuf,
    /// The menu's folder: the dashboard's own working directory until the `folder` row picks
    /// another. Where a launch goes from a menu row or with nothing selected.
    cwd: PathBuf,
    data: Data,
    rows: Vec<Row>,
    /// Indexes into `rows` that pass the filter; the cursor indexes this list.
    visible: Vec<usize>,
    cursor: usize,
    scroll: usize,
    by_state: bool,
    /// Column widths so far, so a value changing length never shifts the table.
    widths: Widths,
    filter: String,
    mode: Mode,
    status: String,
    /// The composer: the instruction a session in the selected row's directory starts with.
    text: String,
    /// The harness the next session starts under; `tab` cycles it. An index into `harness::KNOWN`.
    harness: usize,
    /// A `claude --bg` in flight on its own thread, keyed by its placeholder row's id; its one
    /// line lands in the status.
    started: Vec<(String, mpsc::Receiver<Launched>)>,
    /// Sessions the composer started that Claude does not list yet: a row from the moment
    /// `enter` is pressed, handed over to the registry's row once that appears.
    pending: Vec<Pending>,
    opening: Option<Opening>,
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
    /// The viewers alive inside the dashboard, focused or parsing off-screen; at most
    /// `MAX_VIEWERS`, the least recently focused closes when another opens.
    viewers: Vec<Open>,
    /// The viewer that has the whole frame and the keys; an index into `viewers`.
    focus: Option<usize>,
    /// The real terminal's default colors, probed once at start, for viewers that ask.
    colors: viewer::Colors,
    /// Whether the real terminal reports the mouse to the dashboard right now; on only while
    /// the focused viewer asks for mouse reports.
    mouse_capture: bool,
    /// Clear the terminal before the next frame: set when a viewer leaves the frame.
    needs_clear: bool,
    /// The last frame's rows and columns, which is the pane a viewer opens at.
    size: (u16, u16),
}

/// The viewers a dashboard keeps alive at once; opening another closes the least recently used.
const MAX_VIEWERS: usize = 3;

/// What a launch thread reports: its status line, and the instruction to hand back if it failed.
type Launched = (String, Option<String>);

/// A row for a session the composer started, until Claude lists it. `short` is the id
/// `claude --bg` printed, None until it returns; the row is matched to the registry by it.
struct Pending {
    session: Session,
    short: Option<String>,
    at: Instant,
}

impl Pending {
    fn matches(&self, s: &Session) -> bool {
        s.harness == "claude"
            && s.cwd == self.session.cwd
            && self
                .short
                .as_deref()
                .is_some_and(|short| s.session_id.starts_with(short))
    }
}

// ponytail: a launch whose row never shows up (claude changed what --bg prints) leaves after
// this long instead of sitting there forever; matching on the registry's own start time if it bites.
const PENDING_TTL: Duration = Duration::from_secs(90);

/// The id in `claude --bg`'s one line, `backgrounded · <short id> (idle)`.
fn short_id(status: &str) -> Option<String> {
    status
        .split("backgrounded · ")
        .nth(1)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// The row a just-started Claude session gets before Claude lists it: the instruction's first
/// line as its title, working, in the directory it was started in.
fn placeholder(id: &str, dir: &Path, prompt: &str) -> Session {
    Session {
        session_id: id.to_owned(),
        harness: "claude".into(),
        kind: Some("bg".into()),
        cwd: dir.to_owned(),
        state: "started".into(),
        started: Some(chrono::Utc::now()),
        last_activity: None,
        model: None,
        pid: None,
        transcript_path: None,
        tokens_in: None,
        tokens_out: None,
        context_tokens: None,
        context_window: None,
        cost_usd: None,
        title: Some(prompt.lines().next().unwrap_or("").trim().to_owned()),
        last: Some("starting".into()),
    }
}

/// A viewer and what the dashboard knows about it.
struct Open {
    /// The row key it opened from, or `agents:<harness>` for a harness's own agents view.
    key: String,
    /// `attach`, `codex`, `claude agents`, `logs`: the word in the status line.
    what: String,
    viewer: Viewer,
    /// A Codex thread to record from its rollout once the viewer is left or ends.
    record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    recorded: bool,
    first_paint_logged: bool,
    last_focused: Instant,
}

struct PendingStop {
    id: String,
    label: String,
    verb: &'static str,
    result: mpsc::Receiver<Result<bool>>,
}

struct Opening {
    what: String,
    key: String,
    command: mpsc::Receiver<Result<Command>>,
    record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    prompt: Option<String>,
}

impl PendingStop {
    fn message(&self) -> String {
        let action = match self.verb {
            "delete" => "deleting",
            "forget" => "forgetting",
            _ => "stopping",
        };
        format!("{action} {}", self.label)
    }
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
            widths: Widths::new(),
            filter: String::new(),
            mode: Mode::Normal,
            status: String::new(),
            text: String::new(),
            harness: 0,
            started: Vec::new(),
            pending: Vec::new(),
            opening: None,
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
            viewers: Vec::new(),
            focus: None,
            colors: viewer::Colors::default(),
            mouse_capture: false,
            needs_clear: false,
            size: (24, 80),
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
        let mut launched = false;
        for (id, rx) in std::mem::take(&mut self.started) {
            match rx.try_recv() {
                Ok((message, retry)) => {
                    match retry {
                        // A failed launch takes its row with it and puts the instruction back.
                        Some(prompt) => {
                            self.pending.retain(|p| p.session.session_id != id);
                            if self.text.is_empty() {
                                self.text = prompt;
                            }
                        }
                        None => {
                            if let Some(p) =
                                self.pending.iter_mut().find(|p| p.session.session_id == id)
                            {
                                p.short = short_id(&message);
                            }
                        }
                    }
                    self.status = message;
                    launched = true;
                }
                Err(mpsc::TryRecvError::Empty) => self.started.push((id, rx)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending.retain(|p| p.session.session_id != id);
                    self.status = "session launch stopped unexpectedly".into();
                    launched = true;
                }
            }
        }
        if launched {
            self.data.sessions.retain(|s| {
                !s.session_id.starts_with("starting:")
                    || self
                        .pending
                        .iter()
                        .any(|p| p.session.session_id == s.session_id)
            });
            self.rebuild();
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
        // A started session's row is handed over once the registry lists it.
        self.pending.retain(|p| {
            !data.sessions.iter().any(|s| p.matches(s)) && p.at.elapsed() < PENDING_TTL
        });
        data.sessions
            .extend(self.pending.iter().map(|p| p.session.clone()));
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
        let deleting = self
            .stopping
            .iter()
            .filter(|a| matches!(a.verb, "delete" | "forget"))
            .map(|a| a.id.as_str())
            .collect();
        self.rows = menu_rows(&self.cwd);
        self.rows.extend(
            self.data
                .rows_excluding(self.by_state, &deleting, &mut self.widths),
        );
        self.apply_filter();
        if let Some(k) = &keep
            && let Some(i) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].kind.key() == Some(k.as_str()))
        {
            self.cursor = i;
        } else if keep.is_none() {
            // A fresh dashboard opens on the first table; the menu is where `↑` ends.
            let below = |i: &usize| {
                let k = &self.rows[*i].kind;
                k.selectable() && !matches!(k, Kind::Menu(_))
            };
            self.cursor = self.visible.iter().position(below).unwrap_or(0);
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

    /// The viewer key a row opens: a session's id, `run:<id>` for a run's log or attach.
    fn viewer_key(kind: &Kind) -> Option<String> {
        match kind {
            Kind::Session(id, _) => Some(id.clone()),
            Kind::Run(id, _) => Some(format!("run:{id}")),
            _ => None,
        }
    }

    fn viewer_index(&self, key: &str) -> Option<usize> {
        self.viewers.iter().position(|o| o.key == key)
    }

    /// Give a viewer the frame and the keys.
    fn focus(&mut self, i: usize) {
        self.focus = Some(i);
        let size = self.size;
        let open = &mut self.viewers[i];
        open.last_focused = Instant::now();
        open.viewer.resize(size.0, size.1);
        let line = format!("focus {} ({})", open.key, open.what);
        self.debug(|| line);
    }

    /// Open `c` as a viewer under `key`, or return to the live viewer that already has that
    /// key. The viewer takes the whole frame; a fourth viewer closes the least recently
    /// focused one. The background agent stays in its daemon throughout.
    fn open(
        &mut self,
        terminal_size: (u16, u16),
        c: Command,
        what: &str,
        key: String,
        record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    ) {
        self.size = terminal_size;
        if let Some(i) = self.viewer_index(&key) {
            self.focus(i);
            return;
        }
        self.debug(|| format!("open {what} as {key}: {c:?}"));
        let normal = SHELL_TTY.get().and_then(|t| t.as_ref());
        match Viewer::spawn(
            c,
            terminal_size.0,
            terminal_size.1,
            normal,
            self.colors.clone(),
        ) {
            Ok(viewer) => {
                // The least recently focused makes room only once the new one is running.
                while self.viewers.len() >= MAX_VIEWERS {
                    let oldest = self
                        .viewers
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, o)| o.last_focused)
                        .map(|(i, _)| i)
                        .unwrap();
                    self.close(oldest);
                }
                self.viewers.push(Open {
                    key,
                    what: what.to_owned(),
                    viewer,
                    record,
                    recorded: false,
                    first_paint_logged: false,
                    last_focused: Instant::now(),
                });
                let pid = self.viewers.last().unwrap().viewer.pid();
                self.debug(|| format!("viewer pid {pid}; the dashboard keeps the terminal"));
                self.focus(self.viewers.len() - 1);
            }
            Err(e) => self.status = format!("{what} failed: {e}"),
        }
    }

    /// Close a viewer for good: its process group dies with it. A Codex thread it opened is
    /// recorded first, so its row stays.
    fn close(&mut self, i: usize) {
        let open = self.viewers.remove(i);
        match self.focus {
            Some(f) if f == i => {
                self.focus = None;
                self.needs_clear = true;
            }
            Some(f) if f > i => self.focus = Some(f - 1),
            _ => {}
        }
        self.debug(|| format!("close {} ({})", open.key, open.what));
        if let Some((dir, since)) = &open.record
            && !open.recorded
        {
            self.record_codex(dir, *since);
        }
    }

    /// Ctrl+Z inside a viewer: the dashboard takes the frame back and the viewer keeps
    /// parsing off-screen, so `enter` on its row returns to its current screen at once.
    fn unfocus(&mut self) {
        let Some(i) = self.focus.take() else {
            return;
        };
        self.needs_clear = true;
        self.feedback = Some(("return_to_draw", Instant::now()));
        let open = &mut self.viewers[i];
        open.last_focused = Instant::now();
        self.status = format!("left {} · enter returns to it", open.what);
        let record = (!open.recorded).then(|| open.record.clone()).flatten();
        if let Some((dir, since)) = record {
            self.viewers[i].recorded = true;
            // A thread started from the composer had no id when its viewer opened; now that
            // it has one, its row's `enter` returns here instead of opening a second client.
            if let Some(id) = self.record_codex(&dir, since) {
                self.viewers[i].key = id;
            }
        }
        self.invalidate();
        let open = &self.viewers[i];
        let line = format!(
            "dashboard back: {}; viewer pid {} title {:?}",
            self.status,
            open.viewer.pid(),
            open.viewer.title()
        );
        self.debug(|| line);
    }

    /// Feed every viewer: read what it wrote, answer its queries, hand it its input, and
    /// take a viewer that ended off the list with its exit in the status line. Returns true
    /// when the focused viewer's screen changed.
    fn pump(&mut self) -> bool {
        let mut dirty = false;
        let mut i = 0;
        while i < self.viewers.len() {
            let focused = self.focus == Some(i);
            let open = &mut self.viewers[i];
            let mut lines = Vec::new();
            let mut failed = None;
            match open.viewer.pump() {
                Ok(changed) => dirty |= changed && focused,
                Err(e) => failed = Some(format!("{} failed: {e}", open.what)),
            }
            if !open.first_paint_logged
                && let Some(d) = open.viewer.first_paint()
            {
                open.first_paint_logged = true;
                lines.push(format!(
                    "timing viewer_first_paint ms={:.3}",
                    d.as_secs_f64() * 1000.0
                ));
            }
            let exited = open.viewer.exited();
            for line in lines {
                self.debug(|| line);
            }
            // A viewer the dashboard cannot pump is as gone as one that exited.
            if let Some(message) = failed {
                if focused {
                    self.feedback = Some(("return_to_draw", Instant::now()));
                }
                self.status = message;
                self.close(i);
                self.invalidate();
                self.debug(|| format!("viewer dropped: {}", self.status));
                continue;
            }
            let Some(status) = exited else {
                i += 1;
                continue;
            };
            let open = &self.viewers[i];
            let what = open.what.clone();
            let message = if status.success() {
                format!("back from {what}")
            } else {
                let error = String::from_utf8_lossy(open.viewer.stderr_tail());
                match error.lines().rev().find(|line| !line.trim().is_empty()) {
                    Some(line) => format!("{what} failed: {}", line.trim_start_matches("Error: ")),
                    None => format!("{what} exited with {status}"),
                }
            };
            if focused {
                self.feedback = Some(("return_to_draw", Instant::now()));
            }
            // The exit message first: a Codex thread recorded in `close` replaces it.
            self.status = message;
            self.close(i);
            self.invalidate();
            self.debug(|| format!("viewer exited: {}", self.status));
        }
        dirty
    }

    fn focused(&mut self) -> Option<&mut Open> {
        let i = self.focus?;
        self.viewers.get_mut(i)
    }

    /// Whether the real terminal should report the mouse: only while the focused viewer asks.
    fn wants_mouse(&self) -> bool {
        self.focus
            .and_then(|i| self.viewers.get(i))
            .is_some_and(|o| {
                o.viewer.screen().mouse_protocol_mode() != viewer::MouseProtocolMode::None
            })
    }

    /// Pasted text: wrapped for a focused viewer that asked for bracketed paste, raw
    /// otherwise; into the composer when nothing is focused.
    fn paste(&mut self, text: &str) {
        if let Some(open) = self.focused() {
            if open.viewer.screen().bracketed_paste() {
                let mut bytes = b"\x1b[200~".to_vec();
                bytes.extend_from_slice(text.as_bytes());
                bytes.extend_from_slice(b"\x1b[201~");
                open.viewer.write(&bytes);
            } else {
                open.viewer.write(text.as_bytes());
            }
        } else if matches!(self.mode, Mode::Normal) {
            self.text.push_str(text);
        }
    }

    fn mouse(&mut self, ev: MouseEvent) {
        if let Some(open) = self.focused() {
            let mode = open.viewer.screen().mouse_protocol_mode();
            let bytes = viewer::encode_mouse(ev, (0, 0), mode);
            if !bytes.is_empty() {
                open.viewer.write(&bytes);
            }
        }
    }

    fn prepare_viewer(
        &mut self,
        what: String,
        key: String,
        record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
        prompt: Option<String>,
        prepare: impl FnOnce() -> Result<Command> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        self.status = format!("opening {what} · esc cancels");
        std::thread::spawn(move || {
            let _ = tx.send(prepare());
        });
        self.opening = Some(Opening {
            what,
            key,
            command: rx,
            record,
            prompt,
        });
    }

    /// Called after drawing so even a slow native daemon start has immediate feedback.
    fn poll_opening(&mut self) -> bool {
        let Some(opening) = &self.opening else {
            return false;
        };
        let command = match opening.command.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("viewer preparation stopped"))
            }
            Ok(command) => command,
        };
        let opening = self.opening.take().unwrap();
        match command {
            Ok(command) => {
                self.open(
                    self.size,
                    command,
                    &opening.what,
                    opening.key,
                    opening.record,
                );
            }
            Err(error) => {
                self.status = format!("{} failed: {error:#}", opening.what);
                if self.text.is_empty()
                    && let Some(prompt) = opening.prompt
                {
                    self.text = prompt;
                }
            }
        }
        self.feedback = Some(("opening_result_to_draw", Instant::now()));
        true
    }

    fn cancel_opening(&mut self) -> bool {
        let Some(opening) = self.opening.take() else {
            return false;
        };
        if self.text.is_empty()
            && let Some(prompt) = opening.prompt
        {
            self.text = prompt;
        }
        self.status = "opening cancelled".into();
        true
    }

    /// After a Codex client launched here is left or returns: keep the thread it opened, so
    /// its row stays and `enter` resumes it, and hand back the thread's id. A thread left
    /// before its first turn is gone with the client.
    fn record_codex(&mut self, dir: &Path, since: chrono::DateTime<chrono::Utc>) -> Option<String> {
        let home = codex::home(&self.claude);
        match codex::launched(&home, dir, since) {
            Some(t) => {
                let id = t.id.clone();
                let short: String = id.chars().take(8).collect();
                self.status = match codex::remember(&self.state, t) {
                    Ok(()) => format!("codex thread {short} kept · enter on its row returns to it"),
                    Err(e) => format!("could not record codex thread {short}: {e}"),
                };
                Some(id)
            }
            None => {
                if !self.status.contains("failed") {
                    self.status = "codex thread will appear when the harness reports it".into();
                }
                None
            }
        }
    }

    /// The footer's `enter` verb for the selected row: `return` on a row whose viewer is
    /// alive inside the dashboard; a session of a harness that cannot be joined from here
    /// says so instead of promising an attach.
    fn enter_label(&self) -> &'static str {
        let row = self.selected();
        if row
            .and_then(|r| Self::viewer_key(&r.kind))
            .is_some_and(|k| self.viewer_index(&k).is_some())
        {
            return "return";
        }
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

    fn enter(&mut self) -> Result<()> {
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
        // A row whose viewer is alive returns to its current screen; nothing is started.
        if let Some(i) = Self::viewer_key(&kind).and_then(|k| self.viewer_index(&k)) {
            self.focus(i);
            return Ok(());
        }
        match kind {
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("started {name}")),
            // A headless run cannot be attached while it runs; follow its log instead. A live
            // session attaches natively, ctrl-z comes back here.
            Kind::Run(id, s) if s == "started" => {
                let mut c = self.me();
                c.args(["logs", &id, "--follow"]);
                self.open(self.size, c, "logs", format!("run:{id}"), None);
            }
            // A listed session is live, so `claude attach` runs straight from here; the
            // `cones attach` helper, which reloads the whole fleet first, is for finished runs.
            Kind::Session(id, _) if id.starts_with("starting:") => {
                self.status = "still starting · its row fills in when Claude lists it".into();
            }
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
                    let key = id.clone();
                    self.prepare_viewer("codex".into(), key, None, None, move || {
                        harness::codex_resume(&id, &cwd)
                    });
                    return Ok(());
                }
                match harness::adapter(HarnessKind::Claude)?.attach(&id, &cwd) {
                    Ok(c) => self.open(self.size, c, "attach", id, None),
                    Err(e) => self.status = format!("attach failed: {e:#}"),
                }
            }
            Kind::Run(id, _) => {
                let mut c = self.me();
                c.args(["attach", &id]);
                self.open(self.size, c, "attach", format!("run:{id}"), None);
            }
            Kind::Menu("runs") => self.new_job(),
            Kind::Menu("agents") => self.mode = Mode::Harness(0),
            Kind::Menu(_) => self.mode = Mode::Folder(String::new()),
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
    fn start(&mut self) {
        // The menu's `runs` row: a supervised one-off run under the first job's policy, in the
        // ledger like any other, instead of a bare session.
        if matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu("runs"))) {
            let prompt = std::mem::take(&mut self.text);
            let dir = self.cwd.clone();
            let what = format!("started a run in {}", fleet::tilde(&dir));
            self.spawn(&["run", "--prompt", prompt.trim()], Some(&dir), &what);
            return;
        }
        let dir = self.target_dir();
        let kind = harness::KNOWN[self.harness];
        let prompt = std::mem::take(&mut self.text);
        let what = format!("{kind} in {}", fleet::tilde(&dir));
        // Rollout timestamps are the thread's own clock; a little slack covers it.
        let since = chrono::Utc::now() - chrono::Duration::seconds(5);
        self.debug(|| format!("start {what}: {prompt:?}"));
        if kind == HarnessKind::Codex {
            let record = Some((dir.clone(), since));
            let retry = Some(prompt.clone());
            // A new thread has no id yet; the key is unique to this launch until the thread is
            // recorded on the first ctrl+z, when the viewer takes the thread's id as its key.
            let key = format!("codex:start:{}", since.timestamp_millis());
            self.prepare_viewer(what, key, record, retry, move || {
                match harness::start(kind, &dir, prompt.trim())? {
                    Start::Foreground(command) => Ok(command),
                    Start::Background(_) => anyhow::bail!("expected a Codex viewer"),
                }
            });
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.status = format!("starting {what}");
        // The row is there the moment enter is pressed; the registry fills it in when it lists
        // the session.
        let id = format!("starting:{}", since.timestamp_millis());
        let session = placeholder(&id, &dir, &prompt);
        self.data.sessions.push(session.clone());
        self.pending.push(Pending {
            session,
            short: None,
            at: Instant::now(),
        });
        self.rebuild();
        std::thread::spawn(move || {
            // Capability checks and the command both run off the input thread.
            let result = (|| -> Result<String> {
                let Start::Background(mut command) = harness::start(kind, &dir, prompt.trim())?
                else {
                    anyhow::bail!("expected a background Claude session");
                };
                let output = command.stdin(Stdio::null()).output()?;
                anyhow::ensure!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Ok(format!(
                    "started {what}: {}",
                    uncolored(String::from_utf8_lossy(&output.stdout).trim())
                ))
            })();
            let feedback = match result {
                Ok(message) => (message, None),
                Err(error) => (format!("{what} failed: {error:#}"), Some(prompt)),
            };
            let _ = tx.send(feedback);
        });
        self.started.push((id, rx));
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

    /// ctrl+n, and enter on the menu's `runs` row: the wizard on a new job, its directory
    /// defaulting to the selected row's.
    fn new_job(&mut self) {
        let (base, fallback) = (self.jobs_dir(), self.target_dir());
        self.mode = Mode::Job(Box::new(JobForm::new(&base, &fallback, None)));
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
        let start = if matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu("runs"))) {
            format!("run once in {}", fleet::tilde(&self.cwd))
        } else {
            format!(
                "start {} in {}",
                harness::KNOWN[self.harness],
                fleet::tilde(&self.target_dir())
            )
        };
        let mut line = match &self.mode {
            Mode::Filter => hints(&[("enter", "keep the filter"), ("esc", "clear it")]),
            Mode::Job(form) if form.step == Step::Dir => hints(&[
                ("enter", "next"),
                ("tab", "complete"),
                ("backspace", "on an empty answer goes back"),
                ("esc", "cancel"),
            ]),
            Mode::Job(_) => hints(&[
                ("enter", "next"),
                ("backspace", "on an empty answer goes back"),
                ("esc", "cancel"),
            ]),
            Mode::Harness(_) => Line::default(),
            Mode::Folder(_) => hints(&[
                ("enter", "work there"),
                ("tab", "complete"),
                ("esc", "cancel"),
            ]),
            Mode::Normal if !self.text.is_empty() => hints(&[
                ("enter", &start),
                ("tab", &next),
                ("ctrl+v", "paste image"),
                ("esc", "clear"),
            ]),
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
            Some(Kind::Session(id, _)) if id.starts_with("starting:") => {
                self.status = "still starting · nothing to stop yet".into();
                return;
            }
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
                // A viewer on the row goes first: a client of a session being removed has
                // nothing left to show.
                for key in [id.clone(), format!("run:{id}")] {
                    if let Some(i) = self.viewer_index(&key) {
                        self.close(i);
                    }
                }
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
        let label = self
            .data
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.title.as_deref())
            .and_then(fleet::headline)
            .unwrap_or_else(|| id.chars().take(8).collect());
        let action = PendingStop {
            id,
            label,
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
        self.rebuild();
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
                    if action.verb == "delete" {
                        format!("deleted {} · claude --resume still has it", action.label)
                    } else {
                        format!("forgot {} · codex resume still has it", action.label)
                    }
                }
                Ok(true) => "stop requested".into(),
                Ok(false) => "already finished".into(),
                Err(e) => format!("{} failed: {}: {e:#}", action.verb, action.label),
            };
            self.debug(|| format!("{}: {}", action.id, self.status));
        }
        if finished {
            self.rebuild();
            self.feedback
                .get_or_insert(("action_result_to_draw", Instant::now()));
            self.invalidate();
        }
    }

    /// Returns true when the dashboard should exit. Plain keys type into the composer, so every
    /// action is on ctrl or an arrow, as in `claude agents`. The status of the last action shows
    /// until the next key. While a viewer has the frame every key but ctrl+z is its, in the
    /// classic encoding; ctrl+z leaves it running and comes back here.
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        if let Some(open) = self.focused() {
            if ctrl && code == KeyCode::Char('z') {
                self.unfocus();
                return Ok(false);
            }
            let bytes = viewer::encode_key(code, mods, open.viewer.screen().application_cursor());
            if !bytes.is_empty() {
                open.viewer.write(&bytes);
            }
            return Ok(false);
        }
        self.status.clear();
        if self.cancel_opening() && (code == KeyCode::Esc || (ctrl && code == KeyCode::Char('z'))) {
            return Ok(false);
        }
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
                        let key = format!("agents:{kind}");
                        if let Some(i) = self.viewer_index(&key) {
                            self.focus(i);
                            return Ok(false);
                        }
                        if kind == HarnessKind::Codex {
                            self.prepare_viewer(
                                "codex agents".into(),
                                key,
                                None,
                                None,
                                move || harness::agents(kind),
                            );
                            return Ok(false);
                        }
                        match harness::agents(kind) {
                            Ok(c) => self.open(self.size, c, &format!("{kind} agents"), key, None),
                            Err(e) => self.status = e.to_string(),
                        }
                    }
                    _ => {}
                }
            }
            // The menu's folder: a directory, relative to the current one, checked before it
            // is taken; the menu rows then show and launch into it.
            Mode::Folder(text) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !ctrl => text.push(c),
                // One tab grows the path as far as it is unambiguous; a second, changing
                // nothing, lists what still matches, as bash and zsh do.
                KeyCode::Tab => {
                    let (grown, names) = complete_dir(text, &self.cwd);
                    if grown == *text {
                        self.status = names.join("  ");
                    } else {
                        *text = grown;
                        self.status.clear();
                    }
                }
                KeyCode::Enter => {
                    let text = text.clone();
                    match launch_dir(&text, &self.cwd, &self.cwd) {
                        Ok(dir) => {
                            self.cwd = dir;
                            self.mode = Mode::Normal;
                            self.status = format!("working in {}", fleet::tilde(&self.cwd));
                            self.rebuild();
                        }
                        Err(e) => self.status = e,
                    }
                }
                _ => {}
            },
            // The wizard's directory completes as the folder prompt does, from the jobs
            // file's directory, where a relative answer is taken from.
            Mode::Job(form) if code == KeyCode::Tab && form.step == Step::Dir => {
                let (grown, names) = complete_dir(&form.dir, &form.base);
                if grown == form.dir {
                    self.status = names.join("  ");
                } else {
                    form.dir = grown;
                    self.status.clear();
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
                    KeyCode::Enter if self.text.trim().is_empty() => self.enter()?,
                    KeyCode::Enter => self.start(),
                    KeyCode::Backspace => {
                        self.text.pop();
                    }
                    KeyCode::Char('s') if ctrl => {
                        self.by_state = !self.by_state;
                        self.rebuild();
                    }
                    KeyCode::Char('n') if ctrl => self.new_job(),
                    KeyCode::Char('e') if ctrl => self.edit_job(),
                    KeyCode::Char('o') if ctrl => self.mode = Mode::Harness(0),
                    KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
                    KeyCode::Char('r') if ctrl => {
                        self.invalidate();
                        self.status = "refresh requested".into();
                    }
                    KeyCode::Char('v') if ctrl => match paste_image() {
                        Ok(path) => attach(&mut self.text, &path),
                        Err(e) => self.status = e,
                    },
                    KeyCode::Char(c) if !ctrl => self.text.push(c),
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.size = (area.height, area.width);
        // A focused viewer has the whole frame: its emulated screen, cell for cell, and the
        // terminal's own cursor where the screen puts it, off a wide character's second half.
        if let Some(open) = self.focused() {
            open.viewer.resize(area.height, area.width);
            let screen = open.viewer.screen();
            viewer::render(screen, area, frame.buffer_mut());
            if !screen.hide_cursor() {
                let (row, mut col) = screen.cursor_position();
                col = col.min(area.width.saturating_sub(1));
                if col > 0
                    && screen
                        .cell(row, col)
                        .is_some_and(|c| c.is_wide_continuation())
                {
                    col -= 1;
                }
                if row < area.height {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            }
            return;
        }
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
            Mode::Folder(text) => {
                let mut spans = vec![Span::styled("folder › ", Style::default().fg(ORANGE))];
                spans.extend(typed(text, &fleet::tilde(&self.cwd)));
                Line::from(spans)
            }
            Mode::Normal => self.composer(),
        };
        // Ruled above and below, as Claude Code frames its input; grows with the text, as its input does.
        let frame_lines = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(dim());
        let input = Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .block(frame_lines);
        // line_count already counts the two rules, so this is the whole framed box.
        let rows = input.line_count(frame.area().width).clamp(3, 10) as u16;
        let [head, list, prompt, foot] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(rows),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(Paragraph::new(header_lines(self.data.summary())), head);
        self.draw_list(frame, list);
        frame.render_widget(input, prompt);
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
    // Ctrl+Z is a dashboard key: it leaves the focused viewer running off-screen. It never
    // suspends the dashboard, and a viewer never sees it. Viewers get their default signal
    // handlers back on their own pty in pre_exec.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    SHELL_TTY.get_or_init(|| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(0, &mut t) == 0).then_some(t)
    });
    let mut terminal = ratatui::init();
    // ratatui's hook leaves raw mode and the alternate screen; the modes the dashboard turns
    // on itself (bracketed paste, mouse reports) come off here on a panic as on a quit.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hand_back_tty();
        hook(info);
    }));
    // Before crossterm's first poll, so the replies do not land as keystrokes.
    app.colors = viewer::probe_colors(Duration::from_millis(150));
    app.debug(|| format!("terminal colors {:?}", app.colors));
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
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
            let dirty = app.pump();
            let wants_mouse = app.wants_mouse();
            if wants_mouse != app.mouse_capture {
                if wants_mouse {
                    execute!(std::io::stdout(), EnableMouseCapture)?;
                } else {
                    execute!(std::io::stdout(), DisableMouseCapture)?;
                }
                app.mouse_capture = wants_mouse;
            }
            if redraw
                || dirty
                || app.feedback.is_some()
                || app.tick != drawn_tick
                || app.refreshed != drawn_refresh
            {
                let drawing = Instant::now();
                if app.needs_clear {
                    // A belt over ratatui's diff: the frame a viewer left is not trusted.
                    // Not `Terminal::clear`, which asks the terminal where its cursor is and
                    // fails on one that does not answer; a plain clear and a forgotten
                    // previous buffer give the same full repaint.
                    terminal.backend_mut().clear()?;
                    terminal.swap_buffers();
                    app.needs_clear = false;
                }
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
            if app.poll_opening() {
                continue;
            }
            // Commands and fresh data can land between animation frames. Check them promptly
            // without repainting idle frames or making the spinner depend on key frequency. A
            // focused viewer's output is polled tighter, so typing into it feels direct.
            let wait = Duration::from_millis(if app.focus.is_some() { 8 } else { 25 });
            if event::poll(wait)? {
                let e = event::read()?;
                redraw = true;
                app.debug(|| format!("event {e:?}"));
                match e {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        app.feedback = Some(("input_to_draw", Instant::now()));
                        if app.key(k.code, k.modifiers)? {
                            return Ok(());
                        }
                    }
                    Event::Paste(text) => app.paste(&text),
                    Event::Mouse(m) => app.mouse(m),
                    _ => {}
                }
            }
        }
    })();
    app.debug(|| format!("dashboard loop ended: {result:?}"));
    // Viewers die with the dashboard: their process groups, never the agents behind them.
    app.viewers.clear();
    ratatui::restore();
    hand_back_tty();
    result.context("dashboard")?;
    Ok(0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use std::fs;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn attach_spaces_the_path_from_the_text() {
        let png = Path::new("/tmp/cones/pasted-1.png");
        let mut text = String::new();
        attach(&mut text, png);
        assert_eq!(text, "/tmp/cones/pasted-1.png ");
        let mut text = "look at".to_owned();
        attach(&mut text, png);
        assert_eq!(text, "look at /tmp/cones/pasted-1.png ");
        let mut text = "look at ".to_owned();
        attach(&mut text, png);
        assert_eq!(text, "look at /tmp/cones/pasted-1.png ");
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
            context_window: None,
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
            context_window: None,
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
            context_window: None,
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
            context_window: None,
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

    /// The folder prompt completes as `cd` does: one match fills in with a trailing `/`,
    /// several fill in the shared prefix and are listed, hidden folders need a `.` first.
    #[test]
    fn tab_completes_a_folder_as_the_shell_completes_cd() {
        let d = dir();
        for name in ["alpha", "alps", "beta", ".hidden"] {
            fs::create_dir(d.path().join(name)).unwrap();
        }
        fs::write(d.path().join("alpine"), "").unwrap();
        fs::create_dir(d.path().join("beta/inner")).unwrap();
        let base = d.path();
        assert_eq!(complete_dir("b", base), ("beta/".to_string(), vec![]));
        assert_eq!(
            complete_dir("beta/", base),
            ("beta/inner/".to_string(), vec![])
        );
        assert_eq!(
            complete_dir("a", base),
            (
                "alp".to_string(),
                vec!["alpha".to_string(), "alps".to_string()]
            ),
            "a file is not offered and two folders grow to the shared prefix"
        );
        assert_eq!(
            complete_dir("alp", base).0,
            "alp",
            "no growth means a second tab lists"
        );
        assert_eq!(complete_dir("zzz", base), ("zzz".to_string(), vec![]));
        assert_eq!(
            complete_dir("", base).1,
            vec!["alpha", "alps", "beta"],
            "hidden folders stay out until a dot is typed"
        );
        assert_eq!(complete_dir(".h", base).0, ".hidden/");
        let abs = format!("{}/be", base.display());
        assert_eq!(
            complete_dir(&abs, Path::new("/nowhere")).0,
            format!("{}/beta/", base.display())
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

    /// `enter` in the composer puts a row up at once, titled with the instruction, and the
    /// registry's row takes over when Claude lists the id `--bg` printed. A failed launch takes
    /// the row away and hands the instruction back.
    #[test]
    fn a_started_session_has_a_row_at_once_until_claude_lists_it() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        let mut app = app(claude);
        app.refresh().unwrap();
        let session = placeholder("starting:1", claude, "fix the tests\nplease");
        app.data.sessions.push(session.clone());
        app.pending.push(Pending {
            session,
            short: None,
            at: Instant::now(),
        });
        app.rebuild();
        let has = |app: &App, id: &str| app.rows.iter().any(|r| r.kind.key() == Some(id));
        let row = app
            .rows
            .iter()
            .find(|r| r.kind.key() == Some("starting:1"))
            .unwrap();
        assert!(row.text().contains("fix the tests") && row.working());
        // Claude printed the id; the registry does not list it yet, so the row stays.
        app.pending[0].short = short_id("started claude in ~: backgrounded · aaaaaaaa (idle)");
        assert_eq!(app.pending[0].short.as_deref(), Some("aaaaaaaa"));
        app.refresh().unwrap();
        assert!(has(&app, "starting:1"));
        registry(
            claude,
            A,
            claude.to_str().unwrap(),
            "idle",
            1_757_682_871_000,
        );
        app.refresh().unwrap();
        assert!(
            !has(&app, "starting:1") && has(&app, A),
            "the listed row took over"
        );
        // A launch that fails: its row goes and the instruction is back in the composer.
        let session = placeholder("starting:2", claude, "again");
        app.data.sessions.push(session.clone());
        app.pending.push(Pending {
            session,
            short: None,
            at: Instant::now(),
        });
        app.rebuild();
        assert!(has(&app, "starting:2"));
        let (tx, rx) = mpsc::channel();
        tx.send((
            "claude in ~ failed: no".to_owned(),
            Some("again".to_owned()),
        ))
        .unwrap();
        app.started.push(("starting:2".to_owned(), rx));
        app.poll();
        assert!(!has(&app, "starting:2") && app.pending.is_empty());
        assert_eq!(app.text, "again");
    }

    #[test]
    fn kind_key_is_the_id_without_the_state() {
        assert_eq!(Kind::Session(A.into(), "idle".into()).key(), Some(A));
        assert_eq!(Kind::Session(A.into(), "active".into()).key(), Some(A));
        assert_eq!(Kind::Run("run-1".into(), "ok".into()).key(), Some("run-1"));
        assert_eq!(Kind::Job("nightly".into()).key(), Some("nightly"));
        assert_eq!(Kind::Menu("folder").key(), Some("folder"));
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
    fn slow_viewer_preparation_can_be_cancelled_without_losing_the_instruction() {
        let d = dir();
        let mut app = app(d.path());
        let (release, wait) = mpsc::channel();
        app.prepare_viewer(
            "codex".into(),
            "codex:test".into(),
            None,
            Some("fix the lag".into()),
            move || {
                wait.recv_timeout(Duration::from_secs(2))?;
                Ok(Command::new("not-executed"))
            },
        );
        assert!(app.status.starts_with("opening codex"));
        assert!(app.cancel_opening());
        assert_eq!(app.text, "fix the lag");
        assert!(app.opening.is_none());
        release.send(()).unwrap();
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
    fn slow_delete_removes_the_row_at_confirmation_and_keeps_navigation_live() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        registry(d.path(), B, "/src/one", "idle", 2);
        registry(d.path(), C, "/src/one", "idle", 3);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let (release, wait) = mpsc::channel();
        app.queue_stop(A.into(), "delete", move || {
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(true)
        });
        assert_eq!(app.status, "deleting aaaaaaaa");
        assert_eq!(app.stopping.len(), 1);
        assert_eq!(
            key(&app).as_deref(),
            Some(B),
            "the neighbor is selected immediately"
        );
        assert!(app.rows.iter().all(|r| r.kind.key() != Some(A)));
        app.poll();
        assert_eq!(
            app.data.sessions.len(),
            3,
            "source values remain until acknowledgement"
        );
        app.step(1);
        assert_eq!(key(&app).as_deref(), Some(C), "input is still handled");
        app.refresh().unwrap();
        assert!(
            app.rows.iter().all(|r| r.kind.key() != Some(A)),
            "refresh cannot restore a pending deletion"
        );
        release.send(()).unwrap();
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert!(app.data.sessions.iter().all(|s| s.session_id != A));
        assert_eq!(key(&app).as_deref(), Some(C));
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
            label: "aaaaaaaa".into(),
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
    fn failed_delete_restores_the_row_and_reports_the_error() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.queue_stop(A.into(), "delete", || anyhow::bail!("harness refused"));
        assert!(app.rows.iter().all(|r| r.kind.key() != Some(A)));
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert_eq!(app.status, "delete failed: aaaaaaaa: harness refused");
        assert!(app.rows.iter().any(|r| r.kind.key() == Some(A)));
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
                label: "aaaaaaaa".into(),
                verb: "delete",
                result: first_rx,
            },
            PendingStop {
                id: B.into(),
                label: "bbbbbbbb".into(),
                verb: "delete",
                result: second_rx,
            },
        ];
        second_tx.send(Ok(true)).unwrap();
        app.poll();
        assert_eq!(app.stopping.len(), 1);
        assert!(
            app.rows
                .iter()
                .all(|r| !matches!(r.kind, Kind::Session(..))),
            "the other deletion is still pending"
        );
        first_tx.send(Err(anyhow::anyhow!("refused"))).ok().unwrap();
        app.poll();
        assert!(app.stopping.is_empty());
        assert!(app.rows.iter().any(|r| r.kind.key() == Some(A)));
        assert_eq!(app.status, "delete failed: aaaaaaaa: refused");
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
        assert!(
            !app.rows.iter().any(|r| matches!(r.kind, Kind::Run(..))),
            "gone from the dashboard"
        );
        assert_eq!(ledger.runs().unwrap().len(), 1, "the ledger keeps it");
    }

    #[test]
    fn the_composer_wraps_a_long_instruction_instead_of_cutting_it() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.text = "one two three four five six seven eight nine ten eleven twelve LAST".into();
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(40, 14)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let screen = t
            .backend()
            .buffer()
            .content()
            .chunks(40)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(screen.iter().any(|r| r.contains("LAST")), "{screen:#?}");
        assert!(screen.iter().any(|r| r.contains("one two")), "{screen:#?}");
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
            hint.starts_with("enter new job · tab codex · ctrl+n new job"),
            "an empty dashboard opens on the menu's runs row: {hint}"
        );
        app.harness = (app.harness + 1) % harness::KNOWN.len();
        assert!(text(app.composer()).starts_with(">_ codex › "));
        assert!(text(app.hint_line()).contains("tab claude"));
        app.text = "fix the tests".into();
        assert!(text(app.hint_line()).starts_with("enter run once in "));
        app.step(1);
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

    /// The menu sits above the tables: a fresh dashboard opens on the first table and `↑` from
    /// there lands on `folder`. Picking a folder moves every menu row's target, so a session
    /// or a run can start in a directory nothing runs in yet.
    #[test]
    fn the_top_menu_is_reached_going_up_and_its_folder_moves_the_target() {
        let d = dir();
        let claude = d.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(claude);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A), "opens on the first table");
        app.step(-1);
        assert_eq!(key(&app).as_deref(), Some("folder"));
        app.step(-1);
        app.step(-1);
        assert_eq!(key(&app).as_deref(), Some("runs"));
        assert_eq!(
            app.target_dir(),
            app.cwd,
            "a menu row launches into the menu's folder"
        );
        let inside = claude.join("inside");
        fs::create_dir(&inside).unwrap();
        assert!(launch_dir("nowhere-such-dir", &app.cwd, &app.cwd).is_err());
        app.cwd = launch_dir(&inside.display().to_string(), &app.cwd, &app.cwd).unwrap();
        app.rebuild();
        assert_eq!(
            key(&app).as_deref(),
            Some("runs"),
            "the cursor stays on its row"
        );
        app.step(2);
        assert_eq!(key(&app).as_deref(), Some("folder"));
        assert!(
            app.selected().unwrap().text().contains("inside"),
            "the folder row names the folder"
        );
        assert_eq!(app.target_dir(), inside.canonicalize().unwrap());
        app.refresh().unwrap();
        assert_eq!(
            key(&app).as_deref(),
            Some("folder"),
            "a reload keeps the menu row"
        );
    }

    #[test]
    fn columns_never_shrink() {
        let row = |a: &str, b: &str| vec![(a.to_owned(), plain()), (b.to_owned(), plain())];
        let mut widths = Widths::new();
        let (_, wide) = columns(
            &["state", "age"],
            vec![row("needs input", "59s")],
            &mut widths,
        );
        let (_, narrow) = columns(&["state", "age"], vec![row("idle", "1m")], &mut widths);
        assert_eq!(
            wide[0][0].0.len(),
            narrow[0][0].0.len(),
            "a shorter value keeps the width"
        );
        let (_, wider) = columns(
            &["state", "age"],
            vec![row("needs more input", "1m")],
            &mut widths,
        );
        assert!(
            wider[0][0].0.len() > wide[0][0].0.len(),
            "a longer value widens it"
        );
        let (_, other) = columns(&["j", "age"], vec![row("x", "1m")], &mut widths);
        assert_eq!(other[0][0].0, "x  ", "another table has its own widths");
    }

    /// A shell on a pty that draws `text` at the top left and then waits, as a viewer.
    fn viewer_open(key: &str, what: &str, text: &str) -> Open {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &format!("printf '\\033[H{text}'; sleep 5")]);
        Open {
            key: key.into(),
            what: what.into(),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
        }
    }

    fn rows(t: &Terminal<ratatui::backend::TestBackend>, width: usize) -> Vec<String> {
        t.backend()
            .buffer()
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn a_focused_viewer_takes_the_frame_and_ctrl_z_brings_the_composer_back() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.viewers.push(viewer_open("run:r1", "attach", "VIEW"));
        app.focus = Some(0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.pump() {
            assert!(Instant::now() < deadline, "the viewer never drew");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 80);
        assert!(screen[0].starts_with("VIEW"), "{screen:#?}");
        assert!(
            !screen.iter().any(|r| r.contains("an instruction for")),
            "the composer is not drawn under a viewer: {screen:#?}"
        );
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, None);
        assert!(app.status.starts_with("left attach"), "{}", app.status);
        assert_eq!(app.viewers.len(), 1, "the viewer is alive off-screen");
        assert!(app.needs_clear);
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 80);
        assert!(
            screen.iter().any(|r| r.contains("an instruction for")),
            "{screen:#?}"
        );
        assert!(!screen[0].starts_with("VIEW"), "{screen:#?}");
    }

    #[test]
    fn enter_on_a_row_with_a_live_viewer_returns_to_it_without_spawning() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.viewers.push(viewer_open(A, "attach", "SESSION"));
        assert_eq!(app.enter_label(), "return");
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        assert_eq!(app.viewers.len(), 1, "nothing was spawned");
        assert!(!app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap());
        assert_eq!(app.focus, Some(0), "a plain key goes to the viewer");
        app.unfocus();
        assert_eq!(app.enter_label(), "return");
        assert!(
            app.text.is_empty(),
            "the key went to the viewer, not the composer"
        );
        app.close(0);
        assert!(app.viewers.is_empty());
        assert_eq!(
            app.enter_label(),
            "own terminal",
            "with the viewer gone the row's own verb is back"
        );
    }
}
