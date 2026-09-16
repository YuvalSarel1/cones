//! Native dashboard; user-facing behavior is documented in docs/dashboard.md.
//! Run statuses must not reuse `active`, `idle`, `blocked` or `exited`;
//! shared match arms would sort and render those runs as sessions.
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
        SynchronizedUpdate,
        event::{
            self, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
            KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Capture the shell tty before raw mode; a later snapshot could preserve a child's raw settings.
static SHELL_TTY: OnceLock<Option<libc::termios>> = OnceLock::new();

/// Restore the shell's line discipline and disable reporting modes that would leak input to it.
fn hand_back_tty() {
    if let Some(Some(t)) = SHELL_TTY.get() {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
    reset_terminal_protocols();
}

/// The signals that end the dashboard, as a flag its loop reads rather than a death it never sees.
/// A default disposition kills the process inside raw mode, which hands the shell back a terminal
/// with no echo and cones's reporting modes still on; the flag routes a `kill` or a closed terminal
/// through the same `hand_back_tty` and viewer reaping as `q`. Ctrl+c is a key here, not a signal,
/// so SIGINT arrives only from an explicit kill.
fn quit_on_signals() -> Result<Arc<AtomicBool>> {
    let signalled = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&signalled))?;
    }
    Ok(signalled)
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
const QUIT_CONFIRM: Duration = Duration::from_millis(1500);
const QUIT_HINT: &str = "ctrl+c again quits · any other key stays";
/// Repeated endpoints slow the pulse. Use ▇ because █ touches the row above.
const SPINNER: [&str; 16] = [
    "▁", "▁", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "▇", "▇", "▆", "▅", "▄", "▃", "▂",
];
/// Milliseconds per spinner frame.
const FRAME_MS: usize = 160;
/// The mascot stays still.
const CONE: &str = "▲";

fn spinner_frame(tick: usize) -> usize {
    tick * 100 / FRAME_MS % SPINNER.len()
}

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
    /// Inserted by `App::rebuild`, outside `Data::rows`.
    Menu,
    /// A pinned folder with no live sessions, in `~` form.
    Folder(String),
    NewJob,
}

impl Kind {
    fn selectable(&self) -> bool {
        !matches!(self, Kind::Header | Kind::Columns | Kind::Blank)
    }

    /// Exclude state so a row keeps its identity across reloads.
    pub fn key(&self) -> Option<&str> {
        match self {
            Kind::Job(name) => Some(name),
            Kind::Session(id, _) | Kind::Run(id, _) => Some(id),
            Kind::Menu => Some("menu"),
            Kind::Folder(dir) => Some(dir),
            Kind::NewJob => Some("new job"),
            _ => None,
        }
    }
}

enum Entry<'a> {
    Job(&'a ResolvedJob),
    Session(&'a Session),
    Folder(&'a Path),
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

pub struct Data {
    pub jobs: Vec<ResolvedJob>,
    pub runs: Vec<Run>,
    pub sessions: Vec<Session>,
    /// Session column names from jobs.yaml's `columns:`.
    pub columns: Vec<String>,
    pub pane: config::Pane,
    /// Applied at startup only.
    pub start: config::Start,
    pub spark: config::Activity,
    /// Seconds an armed `ctrl+x` mark stays with no key pressed; 0 keeps it until a key.
    pub confirm_secs: f64,
    /// Leave a table column out rather than draw the part of it that fits.
    pub whole_columns: bool,
    /// Pinned folders retained as rows when empty.
    pub folders: Vec<PathBuf>,
    /// Previously seen session folders, newest first.
    pub recent: Vec<PathBuf>,
    /// Git state for pinned folders without sessions.
    pub git: BTreeMap<PathBuf, String>,
}

impl Data {
    pub fn load(jobs_path: &Path, state: &Path, claude: &Path) -> Result<Self> {
        let ledger = Ledger::new(state)?;
        let hidden = ledger.hidden()?;
        let mut runs = ledger.runs()?;
        runs.retain(|r| !hidden.contains(&r.started.run_id));
        let sessions = fleet_rows(claude, state, &runs)?;
        let seen: Vec<PathBuf> = sessions.iter().map(|s| s.cwd.clone()).collect();
        let folders = ledger.folders()?;
        let jobs = config::read_jobs(jobs_path).unwrap_or_default();
        let git = folders
            .iter()
            .filter(|f| !seen.contains(f) && !jobs.iter().any(|j| &j.cwd == *f))
            .filter_map(|f| git_state(f).map(|g| (f.clone(), g)))
            .collect();
        Ok(Self {
            jobs,
            runs,
            sessions,
            columns: config::columns(jobs_path),
            pane: config::pane(jobs_path),
            start: config::start(jobs_path),
            spark: config::activity(jobs_path),
            confirm_secs: config::confirm_secs(jobs_path),
            whole_columns: config::whole_columns(jobs_path),
            folders,
            recent: ledger.recent(&seen)?,
            git,
        })
    }

    fn has_rows_in(&self, dir: &Path) -> bool {
        self.sessions.iter().any(|s| s.cwd == dir)
    }

    fn count(&self, state: &str) -> usize {
        self.sessions.iter().filter(|s| s.state == state).count()
    }

    pub fn summary(&self, frame: usize) -> Line<'static> {
        let sep = || Span::styled("  ", plain());
        let mut spans = Vec::new();
        for (state, title) in [
            ("active", "working"),
            ("blocked", "input"),
            ("idle", "idle"),
            ("done", "done"),
        ] {
            let n = self.count(state);
            let style = if n == 0 { dim() } else { color(state) };
            let glyph = if state == "active" && n > 0 {
                SPINNER[frame]
            } else {
                icon(state)
            };
            spans.push(Span::styled(format!("{glyph} "), style));
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

    pub fn rows(&self, by_state: bool) -> Vec<Row> {
        self.rows_excluding(by_state, false, &HashSet::new(), &mut Widths::new())
    }

    /// Hide pending deletions without changing source data, so failures can restore their rows.
    fn rows_excluding(
        &self,
        by_state: bool,
        jobs_view: bool,
        deleting: &HashSet<&str>,
        widths: &mut Widths,
    ) -> Vec<Row> {
        let by_state = by_state || jobs_view;
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
        // Sort folders case-insensitively; state groups use a rank prefix to put input first.
        let folder = |dir: &Path| {
            let name = if dir.as_os_str().is_empty() {
                "no directory".to_owned()
            } else {
                fleet::tilde(dir)
            };
            (name.to_lowercase(), name)
        };
        let ranked = |rank: u8, name: &str| (format!("{rank}{name}"), name.to_owned());
        let mut groups: BTreeMap<(String, String), Vec<Entry>> = BTreeMap::new();
        for j in self.jobs.iter().filter(|_| jobs_view) {
            groups
                .entry(ranked(0, "jobs"))
                .or_default()
                .push(Entry::Job(j));
        }
        for s in self
            .sessions
            .iter()
            .filter(|s| !jobs_view && !deleting.contains(s.session_id.as_str()))
        {
            let key = if by_state {
                let rank = match s.state.as_str() {
                    "blocked" => 1,
                    "active" => 2,
                    "idle" => 3,
                    _ => 4,
                };
                ranked(rank, label(&s.state))
            } else {
                folder(&s.cwd)
            };
            groups.entry(key).or_default().push(Entry::Session(s));
        }
        for dir in self.folders.iter().filter(|_| !jobs_view) {
            let (sort, name) = folder(dir);
            let key = if by_state {
                (format!("6{sort}"), name)
            } else {
                (sort, name)
            };
            let group = groups.entry(key).or_default();
            if group.is_empty() && !self.has_rows_in(dir) {
                group.push(Entry::Folder(dir));
            }
        }
        // One table across all groups, so columns line up between directories.
        let flat: Vec<(&(String, String), &Entry)> = groups
            .iter()
            .flat_map(|(key, group)| group.iter().map(move |e| (key, e)))
            .collect();
        let table = flat.iter().any(|(_, e)| !matches!(e, Entry::Folder(_)));
        let job_columns = ["model".to_owned(), "age".to_owned(), "last".to_owned()];
        // Keep the same columns at every width; narrow panes clip the right edge.
        let set = &self.columns;
        let has_state = jobs_view || set.iter().any(|c| c == "state");
        // The harness column is the name after its mark; the mark itself always shows.
        let has_harness = jobs_view || set.iter().any(|c| c == "harness");
        let harness = |h: &str| {
            if has_harness {
                logo(h)
            } else {
                mark(h).to_owned()
            }
        };
        let cols: Vec<&String> = if jobs_view {
            job_columns.iter().collect()
        } else {
            set.iter()
                .filter(|c| *c != "state" && *c != "harness")
                .collect()
        };
        let sparks = fleet::sparklines(&self.sessions, &self.spark, chrono::Utc::now());
        let cells = flat
            .iter()
            .filter(|(_, e)| !matches!(e, Entry::Folder(_)))
            .map(|(_, e)| match e {
                Entry::Folder(_) => vec![],
                Entry::Session(s) => {
                    let mut row = vec![
                        (icon(&s.state).into(), color(&s.state)),
                        (harness(&s.harness), brand(&s.harness)),
                    ];
                    if has_state {
                        row.push(cell("state", s, by_state, None));
                    }
                    // A long title would push every metric column off a 120-column screen.
                    let title = clip(
                        &s.title
                            .clone()
                            .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
                        40,
                    );
                    // The coordinator is a mark and a colour, never a word in the table.
                    row.push(if s.coordinator {
                        (format!("{COORDINATOR} {title}"), lit())
                    } else {
                        (title, plain())
                    });
                    let spark = sparks.get(&s.session_id).map(String::as_str);
                    row.extend(cols.iter().map(|c| cell(c, s, by_state, spark)));
                    row
                }
                Entry::Job(j) => {
                    let last = self
                        .runs
                        .iter()
                        .rev()
                        .find(|r| r.started.job.as_deref() == Some(&j.name));
                    let status = last.map_or("-".to_owned(), |r| r.status());
                    let mut row = vec![
                        (if j.enabled { "◆" } else { "◇" }.into(), color(&status)),
                        (
                            harness(&j.harness.to_string()),
                            brand(&j.harness.to_string()),
                        ),
                    ];
                    if has_state {
                        let (word, style) = job_cell("state", j, last, &status, by_state);
                        row.push((format!("{word} · {}", j.schedule), style));
                    }
                    row.push((j.name.clone(), plain()));
                    row.extend(cols.iter().map(|c| job_cell(c, j, last, &status, by_state)));
                    row
                }
            })
            .collect();
        let mut names = vec!["", ""];
        if has_state {
            names.push("state");
        }
        names.push("title");
        names.extend(cols.iter().map(|c| match c.as_str() {
            "tokens" => "tokens in/out",
            "last" if by_state => "dir",
            c => c,
        }));
        let (names, cells) = columns(&names, cells, widths);
        if table {
            out.push(Row {
                kind: Kind::Blank,
                cells: vec![],
            });
            out.push(names);
        }
        let mut cells = cells.into_iter();
        let mut current: Option<&(String, String)> = None;
        for (key, e) in flat.iter().copied() {
            if current != Some(key) {
                if current.is_none() && table {
                    out.push(Row {
                        kind: Kind::Header,
                        cells: vec![(key.1.clone(), bold())],
                    });
                } else {
                    header(&mut out, &key.1);
                }
                current = Some(key);
            }
            let row = match e {
                Entry::Session(s) => Row {
                    kind: Kind::Session(s.session_id.clone(), s.state.clone()),
                    cells: cells.next().unwrap_or_default(),
                },
                Entry::Job(j) => Row {
                    kind: Kind::Job(j.name.clone()),
                    cells: cells.next().unwrap_or_default(),
                },
                Entry::Folder(dir) => {
                    let mut cells = vec![];
                    if let Some(g) = self.git.get(*dir) {
                        cells.push((format!("{g} · "), plain()));
                    }
                    cells.push((
                        "nothing runs here · an instruction and enter start a session · ctrl+x removes the folder"
                            .to_owned(),
                        dim(),
                    ));
                    Row {
                        kind: Kind::Folder(fleet::tilde(dir)),
                        cells,
                    }
                }
            };
            out.push(row);
        }
        if jobs_view {
            if !table {
                header(&mut out, "jobs");
            }
            out.push(Row {
                kind: Kind::NewJob,
                cells: vec![
                    ("+ new job".to_owned(), lit()),
                    (" · a task once or on a schedule".to_owned(), dim()),
                ],
            });
        }
        if !jobs_view && !self.runs.is_empty() {
            header(&mut out, "runs");
            // Limit the run list to the newest 200 entries.
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

    /// A job's policy, a session's last `exchanges` transcript entries, or a run's output.
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
                        "timeout {:.0}m · write {} · overlap {:?}",
                        j.timeout_min,
                        if j.write { "yes" } else { "no" },
                        j.overlap,
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
                let stamp = |t: Option<chrono::DateTime<chrono::Utc>>| {
                    t.map_or_else(|| "-".into(), |t| t.format("%m-%d %H:%M:%S").to_string())
                };
                let mut out = vec![
                    fleet::tilde(&s.cwd),
                    format!(
                        "{} · {} {}{} · {} · started {} · last activity {} · {} context · {} tokens · pid {} · {}",
                        logo(&s.harness),
                        label(&s.state),
                        s.kind.as_deref().unwrap_or(""),
                        if s.coordinator { " orchestrator" } else { "" },
                        s.model.as_deref().map_or_else(|| "-".into(), fleet::model),
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

/// TSV: hidden key, hidden auxiliary value, then ANSI display text.
/// The first three lines are the mascot and summary header.
pub fn list(jobs_path: &Path, state: &Path, claude: &Path) -> Result<String> {
    let data = Data::load(jobs_path, state, claude)?;
    let mut out = String::new();
    let summary = data.summary(0);
    let width = summary.width() + 16;
    for line in header_lines(summary, width) {
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
            Kind::Menu => ("menu".to_owned(), "-".to_owned()),
            Kind::Folder(dir) => ("folder".to_owned(), dir.clone()),
            Kind::NewJob => ("new job".to_owned(), "-".to_owned()),
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
        Some(Color::Rgb(r, g, b)) => codes.push(format!("38;2;{r};{g};{b}")),
        _ => {}
    }
    match style.bg {
        Some(Color::Indexed(n)) => codes.push(format!("48;5;{n}")),
        Some(Color::Rgb(r, g, b)) => codes.push(format!("48;2;{r};{g};{b}")),
        _ => {}
    }
    if codes.is_empty() {
        text.to_owned()
    } else {
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

/// Button name, action verb, explanation.
const MENU: [(&str, &str, &str); 4] = [
    (
        "folder",
        "add folder",
        "a row for a folder nothing runs in, to start work there",
    ),
    ("jobs", "jobs", "the jobs: start, edit, add one"),
    ("config", "defaults", "job defaults and dashboard settings"),
    ("help", "guide", "the keys and what they do"),
];

fn enter_verb(kind: Option<&Kind>, menu: usize) -> &'static str {
    match kind {
        Some(Kind::Job(_)) => "start job",
        Some(Kind::Run(_, s)) if s == "started" => "follow log",
        Some(Kind::Session(..) | Kind::Run(..)) => "attach",
        Some(Kind::Menu) => MENU[menu].1,
        Some(Kind::Folder(_)) => "start here",
        Some(Kind::NewJob) => "new job",
        _ => "open",
    }
}

fn menu_rows() -> Vec<Row> {
    // Menu cells are drawn by `App::menu_cells` because selection changes without a rebuild.
    vec![
        Row {
            kind: Kind::Blank,
            cells: vec![],
        },
        Row {
            kind: Kind::Menu,
            cells: vec![],
        },
    ]
}

/// Little feet, shared with the README artwork. Each terminal cell holds two vertical pixels.
fn cone() -> [Vec<Span<'static>>; 3] {
    let mut rows = include_str!("../assets/little-feet.txt").lines();
    let color = |pixel| match pixel {
        b'o' => Some(ORANGE),
        b'w' => Some(Color::Rgb(241, 234, 223)),
        _ => None,
    };
    std::array::from_fn(|_| {
        let top = rows.next().unwrap();
        let bottom = rows.next().unwrap();
        top.bytes()
            .zip(bottom.bytes())
            .map(|(top, bottom)| match (color(top), color(bottom)) {
                (None, None) => Span::raw(" "),
                (Some(top), Some(bottom)) if top == bottom => {
                    Span::styled("█", Style::default().fg(top))
                }
                (Some(top), None) => Span::styled("▀", Style::default().fg(top)),
                (None, Some(bottom)) => Span::styled("▄", Style::default().fg(bottom)),
                (Some(top), Some(bottom)) => Span::styled("▀", Style::default().fg(top).bg(bottom)),
            })
            .collect()
    })
}

/// Keep the header's right border visible when clipping counts.
fn header_lines(summary: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mascot = cone();
    if width < 24 {
        return mascot
            .into_iter()
            .enumerate()
            .map(|(i, mut spans)| {
                if i == 1 {
                    spans.push(Span::raw("  "));
                    spans.extend(summary.spans.clone());
                }
                Line::from(fit(spans, width))
            })
            .collect();
    }
    // The box wraps the counts, so it stops where they do. The floor keeps "── cones ─" whole,
    // and `fit` below never returns more than it was given, so the padding cannot underflow.
    let inner = (summary.width() + 2).max(10).min(width - 14);
    let top = vec![
        Span::styled("── ", dim()),
        Span::styled("cones ", lit()),
        Span::styled("─".repeat(inner - 9), dim()),
    ];
    let summary = fit(summary.spans, inner - 2);
    let used: usize = summary.iter().map(Span::width).sum();
    let mut middle = vec![Span::raw(" ")];
    middle.extend(summary);
    middle.push(Span::raw(" ".repeat(inner - used - 1)));
    let bottom = vec![Span::styled("─".repeat(inner), dim())];
    mascot
        .into_iter()
        .zip([("┌", top, "┐"), ("│", middle, "│"), ("└", bottom, "┘")])
        .map(|(mascot, (left, contents, right))| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(mascot);
            spans.push(Span::raw("    "));
            spans.push(Span::styled(left, dim()));
            spans.extend(contents);
            spans.push(Span::styled(right, dim()));
            Line::from(spans)
        })
        .collect()
}

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

fn guide(top: usize, columns: u16) -> Paragraph<'static> {
    let width = GUIDE
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    // Wrap what a key does under the key's column, not back at the frame's edge.
    let indent = 2 + width + 2;
    let mut lines = vec![];
    for (key, what) in GUIDE {
        if key.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                (*what).to_owned(),
                Style::default().fg(ORANGE),
            )));
            continue;
        }
        lines.extend(hang(
            vec![
                Span::styled(format!("  {key:width$}  "), bold()),
                Span::styled((*what).to_owned(), dim()),
            ],
            indent,
            columns as usize,
        ));
    }
    Paragraph::new(lines).scroll((top as u16, 0))
}

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

/// Column widths only grow during a dashboard session, preventing shifts as values change.
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

/// `last` shows the directory when grouping by state; session-only columns stay blank.
fn job_cell(
    column: &str,
    j: &ResolvedJob,
    last: Option<&Run>,
    status: &str,
    by_state: bool,
) -> (String, Style) {
    match column {
        "state" if !j.enabled => ("off".into(), dim()),
        "state" => (status.to_owned(), color(status)),
        "model" => (
            j.model.as_deref().map_or_else(|| "-".into(), fleet::model),
            dim(),
        ),
        "age" => (
            last.and_then(|r| r.started.fired_at)
                .map_or_else(|| "-".into(), fleet::age),
            dim(),
        ),
        "last" if by_state => (fleet::tilde(&j.cwd), dim()),
        _ => (String::new(), dim()),
    }
}

/// `spark` is scaled once for the fleet so rows share a bound.
fn cell(column: &str, s: &Session, by_state: bool, spark: Option<&str>) -> (String, Style) {
    let since = |t: Option<chrono::DateTime<chrono::Utc>>| t.map_or_else(|| "-".into(), fleet::age);
    match column {
        "state" => (label(&s.state).into(), color(&s.state)),
        "activity" => {
            let bars = spark.unwrap_or_default().to_owned();
            let quiet = bars.chars().all(|c| c == '▁');
            (bars, if quiet { dim() } else { plain() })
        }
        "model" => (
            s.model.as_deref().map_or_else(|| "-".into(), fleet::model),
            dim(),
        ),
        "age" => (since(s.started), dim()),
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
fn button() -> Style {
    Style::default().bg(Color::Indexed(237)).fg(Color::White)
}
fn pressed() -> Style {
    Style::default().bg(ORANGE).fg(Color::Black)
}
fn lit() -> Style {
    Style::default().fg(ORANGE).add_modifier(Modifier::BOLD)
}

/// Pad an editor row out to the pane and shade it, so the row the cursor is on reads as one line.
fn on_row(lines: &mut [Line<'static>], columns: u16) {
    for l in lines {
        let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = (columns as usize).saturating_sub(w);
        if pad > 0 {
            l.spans.push(Span::raw(" ".repeat(pad)));
        }
        l.style = l.style.bg(Color::Indexed(237));
    }
}

/// Render the cursor at a byte offset, or on the placeholder when empty.
fn typed(value: &str, cursor: usize, placeholder: &str) -> Vec<Span<'static>> {
    let block = Modifier::REVERSED;
    if !value.is_empty() {
        let (before, rest) = value.split_at(snap(value, cursor));
        let mut rest = rest.chars();
        let under = rest.next().map_or(" ".to_owned(), |c| c.to_string());
        return vec![
            Span::raw(before.to_owned()),
            Span::styled(under, Style::default().add_modifier(block)),
            Span::raw(rest.as_str().to_owned()),
        ];
    }
    let cursor = block;
    let mut rest = placeholder.chars();
    let first = rest.next().map_or(" ".to_owned(), |c| c.to_string());
    vec![
        Span::styled(first, dim().add_modifier(cursor)),
        Span::styled(rest.as_str().to_owned(), dim()),
    ]
}

/// Wrap a label's row at spaces so what spills hangs under the label, not at the frame's edge.
/// `indent` is the columns the label keeps and the spans passed in include it. Unlike `flow`
/// this breaks inside a span, which prose needs and a control's chips do not.
fn hang(spans: Vec<Span<'static>>, indent: usize, columns: usize) -> Vec<Line<'static>> {
    let width = columns.max(2);
    // Leave the continuation at least one column, however narrow the frame is.
    let indent = indent.min(width - 1);
    let cells: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().zip(std::iter::repeat(s.style)))
        .collect();
    if cells.len() <= width {
        return vec![Line::from(spans)];
    }
    let mut lines = vec![];
    let (mut rest, mut pad) = (&cells[..], 0);
    loop {
        let room = width - pad;
        if rest.len() <= room {
            lines.push(padded(rest, pad));
            return lines;
        }
        // Break after the last space that fits; a word wider than the line breaks at the edge.
        let cut = rest[..=room]
            .iter()
            .rposition(|(c, _)| *c == ' ')
            .map_or(room, |i| i + 1);
        lines.push(padded(&rest[..cut], pad));
        rest = &rest[cut..];
        pad = indent;
    }
}

/// Regroup styled characters into spans behind `pad` columns of indent.
fn padded(cells: &[(char, Style)], pad: usize) -> Line<'static> {
    let mut spans = vec![];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    let mut cells = cells;
    while let Some((_, style)) = cells.first().copied() {
        let n = cells.iter().take_while(|(_, s)| *s == style).count();
        let text: String = cells[..n].iter().map(|(c, _)| *c).collect();
        spans.push(Span::styled(text, style));
        cells = &cells[n..];
    }
    Line::from(spans)
}

/// Clamp a possibly stale byte offset to a character boundary.
fn snap(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn word_left(text: &str, at: usize) -> usize {
    let t = text[..at].trim_end();
    t.rfind(char::is_whitespace).map_or(0, |i| i + 1)
}

fn word_right(text: &str, at: usize) -> usize {
    let t = &text[at..];
    let from = t.len() - t.trim_start().len();
    at + t[from..]
        .find(char::is_whitespace)
        .map_or(t.len(), |i| from + i)
}

/// Readline editing; return the new byte offset, or `None` for an unhandled key.
/// macOS cmd shortcuts arrive as control keys, and option shortcuts as alt keys.
fn edit(text: &mut String, cursor: usize, code: KeyCode, mods: KeyModifiers) -> Option<usize> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let at = snap(text, cursor);
    // All word keys treat a word as a run of non-spaces.
    let prev = text[..at]
        .chars()
        .next_back()
        .map_or(at, |c| at - c.len_utf8());
    let next = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
    let (wl, wr) = (word_left(text, at), word_right(text, at));
    let cut = |text: &mut String, from: usize, to: usize| {
        text.replace_range(from..to, "");
        from
    };
    Some(match code {
        KeyCode::Left if ctrl || alt => wl,
        KeyCode::Right if ctrl || alt => wr,
        KeyCode::Char('b') if alt && !ctrl => wl,
        KeyCode::Char('f') if alt && !ctrl => wr,
        KeyCode::Left => prev,
        KeyCode::Right => next,
        KeyCode::Home => 0,
        KeyCode::Char('a') if ctrl => 0,
        KeyCode::End => text.len(),
        KeyCode::Char('e') if ctrl && !text.is_empty() => text.len(),
        KeyCode::Backspace if alt => cut(text, wl, at),
        KeyCode::Char('w') if ctrl => cut(text, wl, at),
        KeyCode::Backspace => cut(text, prev, at),
        KeyCode::Delete => cut(text, at, next),
        KeyCode::Char('d') if alt && !ctrl => cut(text, at, wr),
        KeyCode::Char('u') if ctrl => cut(text, 0, at),
        KeyCode::Char('k') if ctrl => cut(text, at, text.len()),
        KeyCode::Char(c) if !ctrl => {
            text.insert(at, c);
            at + c.len_utf8()
        }
        _ => return None,
    })
}

/// Editable text with a cursor stored as a byte offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub text: String,
    pub at: usize,
}

impl Input {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            at: text.len(),
            text,
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match edit(&mut self.text, self.at, code, mods) {
            Some(at) => {
                self.at = at;
                true
            }
            None => false,
        }
    }

    /// Complete the path; return matching names only when the prefix cannot grow.
    fn complete(&mut self, base: &Path) -> Vec<String> {
        let (grown, names) = complete_dir(&self.text, base);
        if grown == self.text {
            return names;
        }
        *self = Self::new(grown);
        Vec::new()
    }

    fn spans(&self, placeholder: &str) -> Vec<Span<'static>> {
        typed(&self.text, self.at, placeholder)
    }
}

/// Save a clipboard image as a temporary PNG for the harness to read by path.
fn paste_image() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("cones");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("pasted-{stamp}.png"));
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

/// Private-use markers encode image indices as single editable characters.
/// Expand them to labels for display and PNG paths at launch.
const IMAGE: u32 = 0xE000;

fn image_marker(n: usize) -> char {
    char::from_u32(IMAGE + n as u32).expect("private-use range")
}

fn image_index(c: char) -> Option<usize> {
    (IMAGE..IMAGE + 0x100)
        .contains(&(c as u32))
        .then(|| (c as u32 - IMAGE) as usize)
}

/// Insert image `n`'s marker at byte offset `at` and return the new cursor.
fn attach(text: &mut String, at: usize, n: usize) -> usize {
    let at = snap(text, at);
    let mut piece = String::new();
    if !text[..at].is_empty() && !text[..at].ends_with(' ') {
        piece.push(' ');
    }
    piece.push(image_marker(n));
    if !text[at..].starts_with(' ') {
        piece.push(' ');
    }
    text.insert_str(at, &piece);
    at + piece.len()
}

/// Replace image markers with labels or paths from `f`.
fn expand(text: &str, mut f: impl FnMut(usize) -> String) -> String {
    text.chars()
        .map(|c| image_index(c).map_or_else(|| c.to_string(), &mut f))
        .collect()
}

fn icon(state: &str) -> &str {
    match state {
        "active" | "started" => "▁",
        "blocked" => "▇",
        "idle" | "exited" | "stopped" => "▁",
        "ok" | "done" => "✓",
        "skipped" | "-" => "–",
        _ => "✗",
    }
}

/// The harness's mark alone; an unknown harness has only its name.
fn mark(harness: &str) -> &str {
    match harness {
        "claude" => "✻",
        "codex" => ">_",
        "pi" => "π",
        other => other,
    }
}

fn logo(harness: &str) -> String {
    match harness {
        "claude" | "codex" | "pi" => format!("{} {harness}", mark(harness)),
        other => other.to_owned(),
    }
}

/// Prefix on the coordinator's title, which is also drawn in orange.
const COORDINATOR: &str = "★";

fn brand(harness: &str) -> Style {
    match harness {
        "claude" => Style::default().fg(Color::Rgb(215, 119, 87)),
        "pi" => Style::default().fg(Color::Rgb(138, 190, 183)),
        _ => dim(),
    }
}

fn label(state: &str) -> &str {
    match state {
        "active" => "working",
        "blocked" => "input",
        s => s,
    }
}

fn color(status: &str) -> Style {
    match status {
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

fn fit(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let mut text = String::new();
        let mut taken = 0;
        for c in span.content.chars() {
            let cw = Span::raw(c.to_string()).width();
            if used + taken + cw > width {
                break;
            }
            taken += cw;
            text.push(c);
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        break;
    }
    out
}

/// Keep whole columns: a cell the width cuts through is left out instead of drawn in part.
/// The first `keep` cells are cut as before, so a row still names itself in a narrow list.
fn whole_cells(spans: Vec<Span<'static>>, width: usize, keep: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0;
    for (i, span) in spans.into_iter().enumerate() {
        // The two spaces between columns are the cell's own; falling off the edge is no cut.
        let pad = span.content.len() - span.content.trim_end_matches(' ').len();
        if used + span.width() - pad > width {
            if i < keep {
                out.extend(fit(vec![span], width.saturating_sub(used)));
            }
            break;
        }
        used += span.width();
        out.push(span);
    }
    out
}

/// The column names above a row say which cell names it: `title` for a session or job,
/// `job` for a run. Cells up to it are never dropped.
fn named_cell(rows: &[Row], i: usize) -> usize {
    rows[..=i]
        .iter()
        .rev()
        .find(|r| r.kind == Kind::Columns)
        .and_then(|h| {
            h.cells
                .iter()
                .position(|(t, _)| matches!(t.trim(), "title" | "job"))
        })
        .map_or(usize::MAX, |n| n + 1)
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
    // Detached daemon threads have no client in the process table; include their saved records.
    for home in codex::homes(claude) {
        let rows = codex::thread_rows(&home, state, &out);
        out.extend(rows);
    }
    fleet::sort(&mut out);
    Ok(out)
}

/// Resolve `text` relative to `base`; empty text uses `fallback`. Require an existing directory.
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

pub fn git_state(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--branch"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    // `## main...origin/main [ahead 1]`, `## HEAD (no branch)`, `## No commits yet on main`.
    let head = lines.next()?.strip_prefix("## ")?;
    let branch = head.split("...").next().unwrap_or(head);
    Some(match lines.count() {
        0 => format!("{branch} · clean"),
        1 => format!("{branch} · 1 change"),
        n => format!("{branch} · {n} changes"),
    })
}

/// Complete to the longest shared directory prefix; append `/` for a single match.
/// Hidden directories require a `.` prefix.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    What,
    Where,
    When,
    At,
    Name,
}

/// A wizard row: one of the answers a job needs, the settings section's head, or one of the
/// job's own fields under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRow {
    Ask(Step),
    Head,
    Set(usize),
}

#[derive(Debug, PartialEq)]
pub enum FormAction {
    Stay,
    Cancel,
    RunOnce(String, PathBuf),
    /// Write the job, replacing the one with this name when editing.
    Save(Option<String>, Box<config::Job>),
}

const WHEN: [&str; 6] = ["once", "hourly", "daily", "weekdays", "weekly", "cron"];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn picks(spans: &mut Vec<Span<'static>>, options: &[&str], picked: usize) {
    let lit = lit();
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
}

/// Map common cron expressions to wizard presets; preserve others as raw cron.
fn from_cron(schedule: &str) -> (usize, String) {
    let cron = (5, schedule.to_owned());
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields == ["0", "*", "*", "*", "*"] {
        return (1, String::new());
    }
    let &[m, h, "*", "*", d] = fields.as_slice() else {
        return cron;
    };
    let (Ok(m), Ok(h)) = (m.parse::<u8>(), h.parse::<u8>()) else {
        return cron;
    };
    let time = format!("{h:02}:{m:02}");
    match d {
        "*" => (2, time),
        "1-5" => (3, time),
        d => match d.parse::<usize>() {
            Ok(d) if d < 7 => (4, format!("{} {time}", DAYS[d])),
            _ => cron,
        },
    }
}

fn to_cron(when: usize, at: &str) -> Result<String, String> {
    let at = at.trim();
    let time = |t: &str| -> Result<(u8, u8), String> {
        let bad = || format!("a local time as HH:MM, not {t:?}");
        let (h, m) = t.split_once(':').ok_or_else(bad)?;
        match (h.trim().parse::<u8>(), m.trim().parse::<u8>()) {
            (Ok(h), Ok(m)) if h < 24 && m < 60 => Ok((h, m)),
            _ => Err(bad()),
        }
    };
    Ok(match WHEN[when] {
        "hourly" => "0 * * * *".into(),
        "daily" => {
            let (h, m) = time(at)?;
            format!("{m} {h} * * *")
        }
        "weekdays" => {
            let (h, m) = time(at)?;
            format!("{m} {h} * * 1-5")
        }
        "weekly" => {
            let (day, t) = at
                .split_once(' ')
                .ok_or_else(|| format!("a day and a time, as in mon 09:00, not {at:?}"))?;
            let d = DAYS
                .iter()
                .position(|d| day.eq_ignore_ascii_case(d))
                .ok_or_else(|| format!("a day, sun to sat, not {day:?}"))?;
            let (h, m) = time(t.trim())?;
            format!("{m} {h} * * {d}")
        }
        _ => {
            launchd::calendar_intervals(at).map_err(|e| format!("{e:#}"))?;
            at.to_owned()
        }
    })
}

fn slug(prompt: &str) -> String {
    let mut s = String::new();
    for c in prompt.trim().chars().take(60) {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    s.trim_end_matches('-').to_owned()
}

/// Job wizard state. Every row has a default, so an answer left alone still writes a job;
/// `FormAction::Save` leaves persistence to the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct JobForm {
    pub row: JobRow,
    pub prompt: String,
    pub dir: String,
    /// An index into `WHEN`.
    pub when: usize,
    pub at: String,
    pub name: String,
    pub error: Option<String>,
    /// Byte offset in the current answer.
    cursor: usize,
    /// Values for `RUN_FIELDS`; empty inherits `defaults`.
    values: Vec<String>,
    /// The settings section stays folded until the job already sets one of them, or `enter`
    /// on its head opens it.
    shut: bool,
    /// The file's defaults, for what an empty settings row inherits.
    defaults: config::Policy,
    /// The job being edited, as written in the file; `None` adds one.
    original: Option<config::Job>,
    base: PathBuf,
    fallback: PathBuf,
}

impl JobForm {
    /// `base` resolves relative paths; `fallback` supplies an empty directory; `seed` fills the
    /// prompt; `defaults` is what a settings row left empty inherits.
    pub fn new(
        base: &Path,
        fallback: &Path,
        original: Option<config::Job>,
        seed: &str,
        defaults: &config::Policy,
    ) -> Self {
        let (name, dir, prompt, (when, at)) = match &original {
            Some(j) => (
                j.name.clone(),
                j.cwd.display().to_string(),
                j.prompt.clone(),
                from_cron(&j.schedule),
            ),
            None => Default::default(),
        };
        let values: Vec<String> = (0..RUN_FIELDS.len())
            .map(|i| match &original {
                Some(j) => job_value(run_field(i), j),
                None => String::new(),
            })
            .collect();
        Self {
            row: JobRow::Ask(Step::What),
            prompt: if original.is_some() {
                prompt
            } else {
                seed.to_owned()
            },
            dir,
            when,
            at,
            name,
            error: None,
            cursor: usize::MAX,
            // A job that already carries settings of its own opens on them.
            shut: values.iter().all(String::is_empty),
            values,
            defaults: defaults.clone(),
            original,
            base: base.to_owned(),
            fallback: fallback.to_owned(),
        }
    }

    fn go(&mut self, row: JobRow) {
        if let JobRow::Set(_) = row {
            self.shut = false;
        }
        self.row = row;
        self.cursor = usize::MAX;
    }

    /// The rows on screen, in order. `once` writes no line, so it takes the defaults and asks
    /// for neither a name nor settings of its own.
    fn rows(&self) -> Vec<JobRow> {
        let mut rows = vec![
            JobRow::Ask(Step::What),
            JobRow::Ask(Step::Where),
            JobRow::Ask(Step::When),
        ];
        if WHEN[self.when] == "once" {
            return rows;
        }
        if self.asks_at() {
            rows.push(JobRow::Ask(Step::At));
        }
        rows.push(JobRow::Ask(Step::Name));
        rows.push(JobRow::Head);
        if !self.shut {
            rows.extend((0..RUN_FIELDS.len()).map(JobRow::Set));
        }
        rows
    }

    /// Move to the neighbouring row, staying put at either end.
    fn walk(&mut self, back: bool) {
        let rows = self.rows();
        let at = rows.iter().position(|r| *r == self.row).unwrap_or(0);
        let next = if back {
            at.saturating_sub(1)
        } else {
            (at + 1).min(rows.len() - 1)
        };
        self.go(rows[next]);
    }

    pub fn complete(&mut self) -> Vec<String> {
        let mut input = Input::new(std::mem::take(&mut self.dir));
        let names = input.complete(&self.base);
        self.dir = input.text;
        self.cursor = usize::MAX;
        names
    }

    fn field(&mut self) -> Option<&mut String> {
        match self.row {
            JobRow::Ask(Step::What) => Some(&mut self.prompt),
            JobRow::Ask(Step::Where) => Some(&mut self.dir),
            JobRow::Ask(Step::At) => Some(&mut self.at),
            JobRow::Ask(Step::Name) => Some(&mut self.name),
            JobRow::Set(i) if run_field(i).typed() => Some(&mut self.values[i]),
            _ => None,
        }
    }

    fn asks_at(&self) -> bool {
        self.when >= 2
    }

    fn at_placeholder(&self) -> &'static str {
        match WHEN[self.when] {
            "weekly" => "mon 09:00",
            "cron" => "0 9 * * 1-5",
            _ => "09:00",
        }
    }

    /// Rows whose arrows change the value in place rather than move the cursor through it.
    /// A typed value the words do not offer keeps the arrows for its own cursor.
    fn turns(&self) -> bool {
        match self.row {
            JobRow::Ask(Step::When) => true,
            JobRow::Set(i) => match run_field(i).input {
                Answer::Typed | Answer::Columns => false,
                Answer::Number(_) | Answer::Pick(_) => true,
                Answer::PickOrType(..) => run_field(i).picked(&self.values[i]),
            },
            _ => false,
        }
    }

    /// What `enter` does on this row, for the hint line and for the key itself.
    fn enter_does(&self) -> &'static str {
        match self.row {
            JobRow::Ask(Step::When) if WHEN[self.when] == "once" => "run now",
            JobRow::Ask(Step::What | Step::Where | Step::When | Step::At) => "next",
            _ => "save",
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> FormAction {
        if code == KeyCode::Esc {
            return FormAction::Cancel;
        }
        self.error = None;
        // The head takes no value: it opens or shuts the section, or moves off itself.
        if self.row == JobRow::Head {
            match code {
                KeyCode::Enter | KeyCode::Right => self.go(JobRow::Set(0)),
                KeyCode::Left => self.shut = true,
                KeyCode::Up => self.walk(true),
                KeyCode::Down | KeyCode::Tab => self.walk(false),
                _ => {}
            }
            return FormAction::Stay;
        }
        let mut cursor = self.cursor;
        match code {
            KeyCode::Enter => return self.enter(),
            KeyCode::Up => self.walk(true),
            KeyCode::Down | KeyCode::Tab => self.walk(false),
            KeyCode::Left | KeyCode::Right if self.turns() => {
                self.turn(code == KeyCode::Left);
            }
            // A settings row goes back to the default; an empty answer walks back.
            KeyCode::Backspace if self.field().is_none_or(|f| f.is_empty()) => match self.row {
                JobRow::Set(i) if !self.values[i].is_empty() => self.values[i].clear(),
                _ => self.walk(true),
            },
            _ => {
                if let JobRow::Set(i) = self.row {
                    let (f, default) = (run_field(i), self.inherited(i));
                    if !f.typed() {
                        if let KeyCode::Char(c) = code
                            && let Some(o) = f
                                .picks()
                                .unwrap_or_default()
                                .iter()
                                .find(|o| f.label_from(&default, o).starts_with(c))
                        {
                            self.values[i] = if *o == "-" {
                                String::new()
                            } else {
                                (*o).to_owned()
                            };
                        }
                        return FormAction::Stay;
                    }
                    // The first key on a word the row offers types over it; later keys go on
                    // typing, even where what is typed so far is a word of its own.
                    if cursor == usize::MAX
                        && matches!(code, KeyCode::Char(_))
                        && matches!(f.input, Answer::PickOrType(..))
                        && f.picked(&self.values[i])
                    {
                        self.values[i].clear();
                        cursor = usize::MAX;
                    }
                }
                if let Some(f) = self.field()
                    && let Some(at) = edit(f, cursor, code, mods)
                {
                    self.cursor = at;
                }
            }
        }
        FormAction::Stay
    }

    /// Step a number on its grid from what it inherits, or turn the words a row offers.
    fn turn(&mut self, back: bool) {
        if self.row == JobRow::Ask(Step::When) {
            let n = WHEN.len();
            self.when = (self.when + if back { n - 1 } else { 1 }) % n;
            self.at.clear();
            return;
        }
        let JobRow::Set(i) = self.row else { return };
        let f = run_field(i);
        let value = self.values[i].clone();
        if let Some(step) = f.step() {
            let base = if value.is_empty() {
                self.inherited(i)
            } else {
                value
            };
            let now: f64 = base.parse().unwrap_or(0.0);
            let next = (now + if back { -step } else { step }).max(0.0);
            self.values[i] = ConfigForm::trim_num((next / step).round() * step);
            return;
        }
        let default = self.inherited(i);
        let ring = f.ring_from(&default, &value);
        if ring.is_empty() {
            return;
        }
        let at = f.stop_from(&default, &ring, &value);
        self.values[i] = ring[(at + if back { ring.len() - 1 } else { 1 }) % ring.len()].clone();
        self.cursor = usize::MAX;
    }

    /// `enter` answers the row and moves on where an answer follows. Where nothing is left to
    /// answer it writes the job, and on `once` it starts the run instead.
    fn enter(&mut self) -> FormAction {
        match self.row {
            JobRow::Ask(Step::What) if self.prompt.trim().is_empty() => {
                self.error = Some("the task cannot be empty".into());
            }
            JobRow::Ask(Step::What) => self.walk(false),
            JobRow::Ask(Step::Where) => match launch_dir(&self.dir, &self.base, &self.fallback) {
                Ok(dir) => {
                    self.dir = fleet::tilde(&dir);
                    self.walk(false);
                }
                Err(e) => self.error = Some(e),
            },
            JobRow::Ask(Step::When) if WHEN[self.when] == "once" => {
                if self.prompt.trim().is_empty() {
                    self.go(JobRow::Ask(Step::What));
                    self.error = Some("the task cannot be empty".into());
                    return FormAction::Stay;
                }
                return match launch_dir(&self.dir, &self.base, &self.fallback) {
                    Ok(dir) => FormAction::RunOnce(self.prompt.trim().to_owned(), dir),
                    Err(e) => {
                        self.error = Some(e);
                        FormAction::Stay
                    }
                };
            }
            JobRow::Ask(Step::When) => self.walk(false),
            JobRow::Ask(Step::At) => {
                let cron = to_cron(self.when, self.time());
                match cron {
                    Ok(_) => self.walk(false),
                    Err(e) => self.error = Some(e),
                }
            }
            _ => return self.save(),
        }
        FormAction::Stay
    }

    /// The time the schedule is built from: the answer, or the default on the row.
    fn time(&self) -> &str {
        if self.at.trim().is_empty() {
            self.at_placeholder()
        } else {
            &self.at
        }
    }

    /// The name the job is written under: the answer, or the one the task suggests.
    fn title(&self) -> String {
        if self.name.trim().is_empty() {
            slug(&self.prompt)
        } else {
            self.name.trim().to_owned()
        }
    }

    fn set(&self, name: &str) -> &str {
        let JobRow::Set(i) = run_row(name) else {
            unreachable!("run_row is a settings row")
        };
        self.values[i].trim()
    }

    fn inherited(&self, i: usize) -> String {
        inherited(run_field(i), &self.defaults)
    }

    fn save(&mut self) -> FormAction {
        match self.job() {
            Ok(job) => FormAction::Save(
                self.original.as_ref().map(|j| j.name.clone()),
                Box::new(job),
            ),
            Err((row, e)) => {
                self.go(row);
                self.error = Some(e);
                FormAction::Stay
            }
        }
    }

    /// The job the rows describe, or the row a validation error belongs on. Every row left
    /// empty takes its default, so only a bad answer stops the save.
    fn job(&self) -> Result<config::Job, (JobRow, String)> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err((
                JobRow::Ask(Step::What),
                "the task cannot be empty".to_owned(),
            ));
        }
        let dir = launch_dir(&self.dir, &self.base, &self.fallback)
            .map_err(|e| (JobRow::Ask(Step::Where), e))?;
        let schedule = to_cron(self.when, self.time()).map_err(|e| (JobRow::Ask(Step::At), e))?;
        let name = self.title();
        let named = !name.is_empty()
            && name.len() <= 80
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !named {
            return Err((
                JobRow::Ask(Step::Name),
                "1-80 letters, digits, - or _".to_owned(),
            ));
        }
        let dir = fleet::tilde(&dir);
        let mut job = self
            .original
            .clone()
            .unwrap_or_else(|| config::Job::new(&name, &schedule, Path::new(&dir), ""));
        job.name = name;
        job.cwd = PathBuf::from(&dir);
        job.schedule = schedule;
        job.prompt = prompt.to_owned();
        let flag = |f: &str| match self.set(f) {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let text = |f: &str| Some(self.set(f).to_owned()).filter(|t| !t.is_empty());
        let num = |f: &str, what: &str| -> Result<Option<f64>, (JobRow, String)> {
            match self.set(f) {
                "" => Ok(None),
                t => t
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| (run_row(f), format!("{f}: {what}, not {t:?}"))),
            }
        };
        job.enabled = self.set("enabled") != "false";
        job.harness = harness::KNOWN
            .into_iter()
            .find(|k| k.to_string() == self.set("harness"));
        job.model = text("model");
        job.timeout_min = num("timeout_min", "a number of minutes, as in 30")?;
        job.write = flag("write");
        job.overlap = match self.set("overlap") {
            "skip" => Some(config::Overlap::Skip),
            "allow" => Some(config::Overlap::Allow),
            "replace" => Some(config::Overlap::Replace),
            _ => None,
        };
        job.catch_up = match self.set("catch_up") {
            "skip" => Some(config::CatchUp::Skip),
            "once" => Some(config::CatchUp::Once),
            _ => None,
        };
        job.notify = flag("notify");
        job.archive_transcript = flag("archive_transcript");
        job.env = self
            .set("env")
            .split(',')
            .map(|e| e.trim().to_owned())
            .filter(|e| !e.is_empty())
            .collect();
        // Refuse a name on its row: written into the file's flow sequence, one carrying YAML
        // punctuation would be read back as something other than a string.
        for key in &job.env {
            config::env_name(key).map_err(|e| (run_row("env"), format!("env: {e:#}")))?;
        }
        job.bedrock = flag("bedrock");
        job.aws_profile = text("aws_profile");
        job.aws_region = text("aws_region");
        job.codex_full_access = flag("codex_full_access");
        Ok(job)
    }

    /// Render the wizard's rows and return the selected row's line offset.
    fn lines(&self, columns: u16) -> (Vec<Line<'static>>, usize) {
        /// Columns an answer's label keeps, so what wraps hangs under the same one.
        const LABEL_W: usize = 9;
        let title = match &self.original {
            Some(j) => format!("edit {}", j.name),
            None => "new job".to_owned(),
        };
        let mut lines = vec![
            Line::default(),
            Line::from(Span::styled(title, Style::default().fg(ORANGE))),
            Line::default(),
        ];
        // The settings rows are labelled by the key each one writes, in a label column of their
        // own, so the answers above stay tight whichever way the section is folded.
        let set_w = RUN_FIELDS
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        let fallback = fleet::tilde(&self.fallback);
        let suggested = slug(&self.prompt);
        let mut at = 0;
        for row in self.rows() {
            let selected = row == self.row;
            let error = selected.then_some(self.error.as_deref()).flatten();
            match row {
                JobRow::Ask(step) => {
                    let (label, value, placeholder): (&str, &str, &str) = match step {
                        Step::What => ("what", &self.prompt, "the task"),
                        Step::Where => ("where", &self.dir, &fallback),
                        Step::When => ("when", WHEN[self.when], ""),
                        Step::At => ("at", &self.at, self.at_placeholder()),
                        Step::Name if suggested.is_empty() => ("name", &self.name, "from the task"),
                        Step::Name => ("name", &self.name, &suggested),
                    };
                    if selected {
                        at = lines.len();
                    }
                    let style = if selected { lit() } else { bold() };
                    let mut spans = vec![Span::styled(format!("  {label:<6} "), style)];
                    if step == Step::When {
                        picks(&mut spans, &WHEN, self.when);
                    } else if selected {
                        spans.extend(typed(value, self.cursor, placeholder));
                    } else if value.is_empty() {
                        spans.push(Span::styled(placeholder.to_owned(), dim()));
                    } else {
                        spans.push(Span::raw(value.to_owned()));
                    }
                    if let Some(e) = error {
                        spans.push(Span::styled(
                            format!("  {e}"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                    lines.extend(hang(spans, LABEL_W, columns as usize));
                }
                JobRow::Head => {
                    lines.push(Line::default());
                    if selected {
                        at = lines.len();
                    }
                    let mark = if self.shut {
                        "  ▸  enter opens it"
                    } else if selected {
                        "  ▾  ← shuts it"
                    } else {
                        "  ▾"
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  runs".to_owned(),
                            if selected {
                                lit()
                            } else {
                                Style::default().fg(ORANGE)
                            },
                        ),
                        Span::styled("  this job's own, over the defaults".to_owned(), dim()),
                        Span::styled(
                            mark.to_owned(),
                            if selected {
                                Style::default().fg(ORANGE)
                            } else {
                                dim()
                            },
                        ),
                    ]));
                }
                JobRow::Set(i) => {
                    let f = run_field(i);
                    let value = &self.values[i];
                    if selected {
                        at = lines.len();
                    }
                    let mut spans = vec![Span::styled(
                        format!("    {:<set_w$}  ", f.name),
                        if selected { lit() } else { bold() },
                    )];
                    // The cursor belongs in the slot only where typing is what fills it.
                    let open = selected
                        && match f.input {
                            Answer::PickOrType(..) => !f.picked(value),
                            _ => f.typed(),
                        };
                    spans.extend(control(
                        f,
                        &self.inherited(i),
                        value,
                        open,
                        self.cursor,
                        selected,
                    ));
                    if let Some(e) = error {
                        spans.push(Span::styled(
                            format!("  {e}"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                    lines.extend(flow(spans, 4 + set_w + 2, columns as usize));
                }
            }
            // `at` is where the selected row starts, so the rest of it shades as one line.
            if selected {
                on_row(&mut lines[at..], columns);
            }
        }
        (lines, at)
    }

    fn paragraph(&self, body: Rect) -> Paragraph<'static> {
        let (lines, at) = self.lines(body.width);
        let height = body.height as usize;
        let top = at
            .saturating_sub(height / 2)
            .min(lines.len().saturating_sub(height));
        Paragraph::new(lines).scroll((top as u16, 0))
    }

    fn line(&self) -> Line<'static> {
        let (what, help): (&str, String) = match self.row {
            JobRow::Ask(Step::What) => (
                "what",
                "the task, as you would type it to the harness".to_owned(),
            ),
            JobRow::Ask(Step::Where) => (
                "where",
                "a folder; empty takes the one shown, tab completes".to_owned(),
            ),
            JobRow::Ask(Step::When) => (
                "when",
                "once runs it now, supervised and in the ledger; the rest schedule a job"
                    .to_owned(),
            ),
            JobRow::Ask(Step::At) => (
                "at",
                match WHEN[self.when] {
                    "weekly" => "a day and a local time, as in mon 09:00",
                    "cron" => "minute hour day month weekday, as in 0 9 * * 1-5",
                    _ => "a local time, as in 09:00",
                }
                .to_owned(),
            ),
            JobRow::Ask(Step::Name) => (
                "name",
                "the job's name in jobs.yaml and launchd: letters, digits, - or _".to_owned(),
            ),
            JobRow::Head => (
                "runs",
                if self.shut {
                    "enter or → opens the section"
                } else {
                    "← shuts the section · → goes into it"
                }
                .to_owned(),
            ),
            JobRow::Set(i) => {
                let f = run_field(i);
                let d = self.inherited(i);
                let default = if d.is_empty() || d == SYSTEM {
                    "default passes nothing".to_owned()
                } else {
                    format!("default: {d}")
                };
                (
                    f.name,
                    match f.input {
                        Answer::PickOrType(_, w) => {
                            format!("{}, or type {w} · {default}", f.short)
                        }
                        _ => format!("{} · {default}", f.short),
                    },
                )
            }
        };
        Line::from(vec![
            Span::styled(format!("{what} › "), Style::default().fg(ORANGE)),
            Span::styled(help, dim()),
        ])
    }
}

/// A config field's grouping, display text, default and input control.
struct Field {
    group: &'static str,
    /// Shared subheading; empty means directly under the group.
    sub: &'static str,
    name: &'static str,
    short: &'static str,
    long: &'static str,
    builtin: &'static str,
    input: Answer,
}

enum Answer {
    Typed,
    Number(f64),
    Pick(&'static [&'static str]),
    PickOrType(&'static [&'static str], &'static str),
    Columns,
}

impl Field {
    fn picks(&self) -> Option<&'static [&'static str]> {
        match self.input {
            Answer::Typed | Answer::Number(_) | Answer::Columns => None,
            Answer::Pick(o) | Answer::PickOrType(o, _) => Some(o),
        }
    }

    fn typed(&self) -> bool {
        !matches!(self.input, Answer::Pick(_) | Answer::Columns)
    }

    fn step(&self) -> Option<f64> {
        match self.input {
            Answer::Number(s) => Some(s),
            _ => None,
        }
    }

    /// The option ring. Empty means built-in, and the built-in's own word is not offered a
    /// second time. A pick-only field keeps a value from the file it does not offer; a field
    /// that also types shows it in the slot past the ring instead.
    fn ring(&self, value: &str) -> Vec<String> {
        self.ring_from(self.builtin, value)
    }

    /// The ring stop a value sits on; a value on no stop is the typed slot past them.
    fn stop(&self, ring: &[String], value: &str) -> usize {
        self.stop_from(self.builtin, ring, value)
    }

    fn label<'a>(&self, o: &'a str) -> &'a str {
        self.label_from(self.builtin, o)
    }

    /// The same ring against a built-in of the caller's own. The wizard's rows inherit the
    /// file's defaults rather than the built-in here, so their first stop stands for whatever
    /// `defaults` says and every word the field offers stays on the ring.
    fn ring_from(&self, builtin: &str, value: &str) -> Vec<String> {
        let mut ring: Vec<String> = self
            .picks()
            .unwrap_or_default()
            .iter()
            .filter(|o| **o != builtin)
            .map(|o| if *o == "-" { "" } else { *o }.to_owned())
            .collect();
        if !self.typed()
            && !value.is_empty()
            && value != builtin
            && !ring.iter().any(|o| o == value)
        {
            ring.push(value.to_owned());
        }
        ring
    }

    fn stop_from(&self, builtin: &str, ring: &[String], value: &str) -> usize {
        let value = if value == builtin { "" } else { value };
        ring.iter()
            .position(|o| o == value)
            .unwrap_or(if self.typed() { ring.len() } else { 0 })
    }

    fn label_from<'a>(&self, builtin: &'a str, o: &'a str) -> &'a str {
        if o != "-" {
            o
        } else if builtin == SYSTEM {
            SYSTEM
        } else {
            self.word_from(builtin)
        }
    }

    /// What a row shows for an unset value: the built-in's own word when it is one of the
    /// options, so the answer is on the row and not in the help line under it.
    fn word_from<'a>(&self, builtin: &'a str) -> &'a str {
        if self.picks().is_some_and(|o| o.contains(&builtin)) {
            builtin
        } else {
            "default"
        }
    }

    fn picked(&self, value: &str) -> bool {
        self.picks()
            .is_some_and(|o| value.is_empty() || o.contains(&value))
    }
}

const BOOL: &[&str] = &["-", "false", "true"];

/// Empty harness-owned fields pass no override to the harness.
const SYSTEM: &str = "system default";

const GROUPS: [(&str, &str); 3] = [
    ("cones", "the dashboard itself"),
    ("harnesses", "how claude and codex are run"),
    ("runs", "what a run starts with"),
];

/// The group a fresh editor keeps shut. A run inherits these settings and rarely changes them,
/// while the rows above are the dashboard's own; `enter` or `→` on the head opens the section
/// and `←` shuts it again.
const SHUT: &str = "runs";

/// The row the folding group's head stands on: the first field the group holds.
fn fold_row() -> usize {
    FIELDS
        .iter()
        .position(|f| f.group == SHUT)
        .expect("SHUT names a group")
}

/// What the shut group's head explains in the place of a field's own text.
const SHUT_LONG: &str = "The value every run starts with, for each field a run has, unless the job's own line says otherwise. Scheduled runs and a `once` run take them; a session the composer starts is the harness's own and takes only the model and provider above.";

/// `start.harness` controls the composer; `defaults.harness` supplies the default for jobs.
const FIELDS: [Field; 25] = [
    Field {
        group: "cones",
        sub: "",
        name: "confirm_secs",
        short: "ctrl+x armed (s)",
        long: "Seconds an armed ctrl+x waits for its second press with no key pressed, up to 600. 0 keeps the mark until the next key.",
        builtin: "2",
        input: Answer::Number(1.0),
    },
    Field {
        group: "cones",
        sub: "",
        name: "columns",
        short: "session columns",
        long: "The columns the table draws after the harness and title, in their order. The row is the arranger: left and right pick a column, space shows or hides it, [ ] move it, and the table redraws under each key.",
        builtin: "harness, state, context, activity, model, age, last",
        input: Answer::Columns,
    },
    Field {
        group: "cones",
        sub: "",
        name: "whole_columns",
        short: "whole columns only",
        long: "true leaves out a column the list's right edge would cut through, so the table ends on a column that fits. false draws as much of it as there is room for. The mark, harness, state and title are always drawn, so a row names itself however narrow the list is.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "cones",
        sub: "start",
        name: "start.harness",
        short: "composer starts on",
        long: "The harness the composer is on in a new cones terminal; shift+tab changes it from there and cones writes nothing back. Codex and pi sessions start, their jobs are still unavailable. A pi runs in the dashboard's own viewer and ends with it, and is started with the instruction alone: the model and provider defaults name claude and codex.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex", "pi"]),
    },
    Field {
        group: "cones",
        sub: "start",
        name: "start.pane",
        short: "open with the pane",
        long: "Whether a new cones terminal opens with the viewer pane beside the list; ctrl+\\ toggles it from there and cones writes nothing back.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "cones",
        sub: "pane",
        name: "pane.at",
        short: "pane side",
        long: "right puts the pane beside the list, bottom under it.",
        builtin: "right",
        input: Answer::Pick(&["-", "right", "bottom"]),
    },
    Field {
        group: "cones",
        sub: "pane",
        name: "pane.ratio",
        short: "pane share (%)",
        long: "Percent of the frame the pane takes, 30 to 70 in tens. The list keeps the rest, less the divider between them; a taller or wider terminal gives both more.",
        builtin: "50",
        input: Answer::Pick(&["-", "30", "40", "50", "60", "70"]),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bars",
        short: "bar count",
        long: "Number of bars, 1 to 64, oldest first. 16 bars at 1m show the last 16 minutes.",
        builtin: "16",
        input: Answer::Number(1.0),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bucket",
        short: "time per bar",
        long: "Time per bar, such as 30s, 1m or 5m. Maximum 24h.",
        builtin: "1m",
        input: Answer::PickOrType(&["-", "30s", "1m", "5m", "15m", "1h"], "a duration"),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.metric",
        short: "count per bar",
        long: "lines: all transcript lines. messages: assistant replies. tools: tool calls. tokens: output tokens.",
        builtin: "lines",
        input: Answer::Pick(&["-", "lines", "messages", "tools", "tokens"]),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bound",
        short: "chart scale",
        long: "fleet: busiest bucket on screen. row: each row's busiest bucket. log: fleet on a log scale. A number in jobs.yaml sets the count for a full bar.",
        builtin: "fleet",
        input: Answer::Pick(&["-", "fleet", "row", "log"]),
    },
    Field {
        group: "harnesses",
        sub: "",
        name: "bedrock",
        short: "run on Bedrock",
        long: "true sends Claude to Amazon Bedrock, false to its own endpoint; system default passes nothing and the harness's own configuration decides. Claude is the only harness it reaches: a Codex job is refused outright, the Codex daemon keeps the provider it started with, and a composer pi starts with no provider switch. true is refused without the profile and region below, since the switch alone reaches Bedrock with nothing to authenticate it.",
        builtin: SYSTEM,
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "",
        name: "aws_profile",
        short: "AWS profile",
        long: "The profile every Bedrock run is given as AWS_PROFILE, as named in ~/.aws/config. Required by bedrock: true and unused without it; the run still inherits every other AWS_ variable for the credentials themselves.",
        builtin: SYSTEM,
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "",
        name: "aws_region",
        short: "AWS region",
        long: "The region every Bedrock run is given as AWS_REGION, as in us-east-1. Required by bedrock: true and unused without it; a model id is answered only by the regions that carry it.",
        builtin: SYSTEM,
        input: Answer::PickOrType(
            &[
                "-",
                "us-east-1",
                "us-west-2",
                "eu-central-1",
                "ap-northeast-1",
            ],
            "a region",
        ),
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "model",
        short: "alias or model id",
        long: "The alias or model id passed to Claude as --model, for jobs and for sessions the composer starts. A [1m] suffix asks for the million-token window, which the bare alias does not: opus starts on 200k. system default passes nothing and Claude's own settings decide.",
        builtin: SYSTEM,
        input: Answer::PickOrType(
            &[
                "-",
                "fable",
                "opus",
                "opus[1m]",
                "sonnet",
                "sonnet[1m]",
                "haiku",
            ],
            "a model id",
        ),
    },
    Field {
        group: "harnesses",
        sub: "codex",
        name: "codex_model",
        short: "model id",
        long: "Passed to Codex as -m for sessions the composer starts. The words are the ids as OpenAI names them; a Bedrock daemon takes the same id with the openai. prefix, typed in the slot. Empty passes nothing and Codex's own config decides. Codex jobs are still unavailable.",
        builtin: SYSTEM,
        input: Answer::PickOrType(
            &[
                "-",
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-luna",
                "gpt-5.6-terra",
            ],
            "a model id",
        ),
    },
    Field {
        group: "harnesses",
        sub: "codex",
        name: "codex_full_access",
        short: "no sandbox",
        long: "true allows all paths and network access without a sandbox. false uses the workspace sandbox; write controls file changes. Codex jobs are currently unavailable.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "harness",
        short: "harness",
        long: "The harness a run starts under, unless the job names one of its own. What the composer comes up on is start.harness above. Codex and pi jobs are still unavailable.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex", "pi"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "timeout_min",
        short: "time limit (min)",
        long: "Positive minutes, up to 10080 (one week). cones stops overdue runs and records a timeout.",
        builtin: "30",
        input: Answer::Number(5.0),
    },
    Field {
        group: "runs",
        sub: "",
        name: "write",
        short: "allow file changes",
        long: "false lets a job Read, Grep and Glob only. true adds Edit, Write and sandboxed Bash; a Codex job becomes workspace-write.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "overlap",
        short: "already running",
        long: "When a job is already running: skip the next run, allow both, or replace the active run.",
        builtin: "skip",
        input: Answer::Pick(&["-", "skip", "allow", "replace"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "catch_up",
        short: "missed ticks",
        long: "launchd loses a tick that passes while the Mac is powered off or logged out. once starts one run at the next login when any tick was missed, however many passed; skip leaves them lost. A slept-through tick already fires on wake and needs neither.",
        builtin: "skip",
        input: Answer::Pick(&["-", "skip", "once"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "notify",
        short: "failure alerts",
        long: "Notify on failures and timeouts.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "archive_transcript",
        short: "keep the transcript",
        long: "true copies Claude's transcript into ~/.cones/transcripts/<run id>/ when the run ends.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "env",
        short: "import env vars",
        long: "Shell variables every run imports, by name, separated by commas. A job's own list replaces this one. Values are read when the schedule is installed. Names that could change execution policy are refused, and Bedrock credentials come from the switch above instead.",
        builtin: "none",
        input: Answer::Typed,
    },
];

fn field_at(name: &str) -> usize {
    FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap_or_else(|| panic!("no config field {name}"))
}

/// The one field a job has that `defaults` does not, so the config editor never carries it.
const ENABLED: Field = Field {
    group: "runs",
    sub: "",
    name: "enabled",
    short: "on its schedule",
    long: "false keeps the job in the file and off the schedule; cones still starts it by hand.",
    builtin: "true",
    input: Answer::Pick(&["-", "true", "false"]),
};

/// What the wizard's settings section holds: the job's own field, then every field a default
/// covers, in the order a job line carries them. An empty row inherits `defaults`.
const RUN_FIELDS: [&str; 14] = [
    "enabled",
    "harness",
    "model",
    "timeout_min",
    "write",
    "overlap",
    "catch_up",
    "notify",
    "archive_transcript",
    "env",
    "bedrock",
    "aws_profile",
    "aws_region",
    "codex_full_access",
];

fn run_field(i: usize) -> &'static Field {
    match RUN_FIELDS[i] {
        "enabled" => &ENABLED,
        name => &FIELDS[field_at(name)],
    }
}

fn run_row(name: &str) -> JobRow {
    JobRow::Set(
        RUN_FIELDS
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("no run field {name}")),
    )
}

/// What an empty settings row falls back to: the file's own default, else the built-in.
fn inherited(f: &Field, d: &config::Policy) -> String {
    let num = |v: Option<f64>| v.map(ConfigForm::trim_num);
    let flag = |v: Option<bool>| v.map(|b| b.to_string());
    let text = match f.name {
        "harness" => d.harness.map(|h| h.to_string()),
        "model" => d.model.clone(),
        "timeout_min" => num(d.timeout_min),
        "write" => flag(d.write),
        "overlap" => d.overlap.map(|o| overlap_word(o).to_owned()),
        "catch_up" => d.catch_up.map(|c| catch_up_word(c).to_owned()),
        "notify" => flag(d.notify),
        "archive_transcript" => flag(d.archive_transcript),
        "env" => d.env.as_ref().map(|e| e.join(", ")),
        "bedrock" => flag(d.bedrock),
        "aws_profile" => d.aws_profile.clone(),
        "aws_region" => d.aws_region.clone(),
        "codex_full_access" => flag(d.codex_full_access),
        _ => None,
    };
    text.unwrap_or_else(|| f.builtin.to_owned())
}

fn overlap_word(o: config::Overlap) -> &'static str {
    match o {
        config::Overlap::Skip => "skip",
        config::Overlap::Allow => "allow",
        config::Overlap::Replace => "replace",
    }
}

fn catch_up_word(c: config::CatchUp) -> &'static str {
    match c {
        config::CatchUp::Skip => "skip",
        config::CatchUp::Once => "once",
    }
}

/// A field's control and current value, against the built-in the caller stands behind: the
/// config editor's own, or what a wizard row inherits from `defaults`. `open` draws the typed
/// slot with the cursor in it.
fn control(
    f: &Field,
    builtin: &str,
    value: &str,
    open: bool,
    cursor: usize,
    selected: bool,
) -> Vec<Span<'static>> {
    /// Columns the box of a typed value keeps, whatever is in it.
    const BOX_W: usize = 18;
    let mut spans = vec![];
    // A field that also types keeps a slot past its words; typing or a value the words
    // do not offer sits there.
    let mut slot: Option<&str> = None;
    if f.picks().is_some() {
        let ring = f.ring_from(builtin, value);
        let labels: Vec<&str> = ring
            .iter()
            .map(|o| {
                if o.is_empty() {
                    f.word_from(builtin)
                } else {
                    o.as_str()
                }
            })
            .collect();
        let at = if open {
            ring.len()
        } else {
            f.stop_from(builtin, &ring, value)
        };
        picks(&mut spans, &labels, at);
        match f.input {
            Answer::PickOrType(_, what) => slot = Some(what),
            _ => return spans,
        }
        spans.push(Span::raw(" "));
        if at < ring.len() {
            // On a word the slot stands empty, saying what it takes, and wraps whole.
            spans.push(Span::styled(
                format!("[ {what:<BOX_W$} ]", what = slot.unwrap_or_default()),
                dim(),
            ));
            return spans;
        }
    }
    if f.step().is_some() {
        let shown = match (value.is_empty(), builtin.parse::<f64>().is_ok()) {
            (false, _) => value,
            (true, true) => builtin,
            (true, false) => "default",
        };
        let arrows = if open { lit() } else { dim() };
        return vec![
            Span::styled("‹ ", arrows),
            if open {
                Span::styled(shown.to_owned(), pressed())
            } else if value.is_empty() {
                Span::styled(shown.to_owned(), dim())
            } else {
                Span::styled(shown.to_owned(), bold())
            },
            Span::styled(" ›", arrows),
        ];
    }
    let box_at = spans.len();
    let edge = if slot.is_some() && selected {
        lit()
    } else {
        dim()
    };
    spans.push(Span::styled("[ ", edge));
    if open {
        spans.extend(typed(value, cursor, slot.unwrap_or(builtin)));
    } else if value.is_empty() {
        let builtin = if builtin == SYSTEM {
            "default"
        } else {
            builtin
        };
        spans.push(Span::styled(builtin.to_owned(), dim()));
    } else {
        spans.push(Span::styled(value.to_owned(), bold()));
    }
    let used: usize = spans.iter().skip(box_at + 1).map(Span::width).sum();
    spans.push(Span::styled(
        format!("{} ]", " ".repeat(BOX_W.saturating_sub(used))),
        edge,
    ));
    spans
}

/// What a job's own line says for a field, empty where it leaves the field to `defaults`.
fn job_value(f: &Field, j: &config::Job) -> String {
    let num = |v: Option<f64>| v.map(ConfigForm::trim_num);
    let flag = |v: Option<bool>| v.map(|b| b.to_string());
    let text = match f.name {
        "enabled" => (!j.enabled).then(|| "false".to_owned()),
        "harness" => j.harness.map(|h| h.to_string()),
        "model" => j.model.clone(),
        "timeout_min" => num(j.timeout_min),
        "write" => flag(j.write),
        "overlap" => j.overlap.map(|o| overlap_word(o).to_owned()),
        "catch_up" => j.catch_up.map(|c| catch_up_word(c).to_owned()),
        "notify" => flag(j.notify),
        "archive_transcript" => flag(j.archive_transcript),
        "env" => Some(j.env.join(", ")).filter(|e| !e.is_empty()),
        "bedrock" => flag(j.bedrock),
        "aws_profile" => j.aws_profile.clone(),
        "aws_region" => j.aws_region.clone(),
        "codex_full_access" => flag(j.codex_full_access),
        _ => None,
    };
    text.unwrap_or_default()
}

#[derive(Debug, PartialEq)]
pub enum ConfigAction {
    Stay,
    Cancel,
    /// Validated values for the caller to persist. Empty columns and absent blocks use built-ins.
    Save(
        Box<config::Policy>,
        Vec<String>,
        Option<config::Activity>,
        Option<config::Pane>,
        Option<config::Start>,
        Option<f64>,
        Option<bool>,
    ),
}

/// Config editor state. Changes emit `Save` immediately; empty values use built-ins.
/// The dashboard owns file I/O, and session overrides stay in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigForm {
    pub row: usize,
    /// Values in `FIELDS` order; empty selects the built-in.
    pub values: Vec<String>,
    pub error: Option<String>,
    /// `before` restores an open field on escape.
    pub open: bool,
    before: String,
    /// Byte offset in the selected value.
    cursor: usize,
    /// The `SHUT` group is folded into its head until `enter` or `→` opens it.
    shut: bool,
    /// The selection sits on that head rather than on the field it stands for, open or shut.
    on_head: bool,
    arrange: ColumnForm,
}

impl ConfigForm {
    pub fn new(
        d: &config::Policy,
        columns: Option<&[String]>,
        spark: Option<&config::Activity>,
        pane: Option<&config::Pane>,
        start: Option<&config::Start>,
        confirm_secs: Option<f64>,
        whole_columns: Option<bool>,
    ) -> Self {
        let num = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
        let flag = |v: Option<bool>| v.map(|v| v.to_string()).unwrap_or_default();
        let spark = |f: fn(&config::Activity) -> String| spark.map(f).unwrap_or_default();
        let pane = |f: fn(&config::Pane) -> String| pane.map(f).unwrap_or_default();
        let values = FIELDS
            .iter()
            .map(|f| match f.name {
                "timeout_min" => num(d.timeout_min),
                "write" => flag(d.write),
                "overlap" => d
                    .overlap
                    .map(|o| match o {
                        config::Overlap::Skip => "skip",
                        config::Overlap::Allow => "allow",
                        config::Overlap::Replace => "replace",
                    })
                    .unwrap_or_default()
                    .to_owned(),
                "catch_up" => d.catch_up.map(catch_up_word).unwrap_or_default().to_owned(),
                "model" => d.model.clone().unwrap_or_default(),
                "harness" => d.harness.map(|h| h.to_string()).unwrap_or_default(),
                "codex_model" => d.codex_model.clone().unwrap_or_default(),
                "codex_full_access" => flag(d.codex_full_access),
                "notify" => flag(d.notify),
                "archive_transcript" => flag(d.archive_transcript),
                "env" => d.env.as_ref().map(|e| e.join(", ")).unwrap_or_default(),
                "bedrock" => flag(d.bedrock),
                "aws_profile" => d.aws_profile.clone().unwrap_or_default(),
                "aws_region" => d.aws_region.clone().unwrap_or_default(),
                "start.harness" => start.map(|s| s.harness.to_string()).unwrap_or_default(),
                "start.pane" => start.map(|s| s.pane.to_string()).unwrap_or_default(),
                "pane.at" => pane(|p| p.at.clone()),
                "pane.ratio" => pane(|p| p.ratio.to_string()),
                "activity.bars" => spark(|s| s.bars.to_string()),
                "activity.bucket" => spark(|s| s.bucket.clone()),
                "activity.metric" => spark(|s| s.metric.clone()),
                "confirm_secs" => num(confirm_secs),
                "whole_columns" => flag(whole_columns),
                "columns" => columns.map(|c| c.join(", ")).unwrap_or_default(),
                _ => spark(|s| s.bound.clone()),
            })
            .collect();
        Self {
            row: 0,
            values,
            error: None,
            open: false,
            before: String::new(),
            cursor: usize::MAX,
            shut: true,
            on_head: false,
            arrange: ColumnForm::new(columns.unwrap_or(&built_columns())),
        }
    }

    /// The selected row is the folding group's head, not a field of its own.
    fn head(&self) -> bool {
        self.on_head
    }

    /// A jump asks for the field itself, so it opens the group holding it.
    fn go(&mut self, row: usize) {
        if FIELDS[row].group == SHUT {
            self.shut = false;
        }
        self.step(row);
    }

    fn step(&mut self, row: usize) {
        self.row = row;
        self.cursor = usize::MAX;
        self.on_head = false;
    }

    fn enter(&mut self) {
        self.open = true;
        self.before = self.values[self.row].clone();
        self.cursor = usize::MAX;
    }

    fn field(&self) -> &'static Field {
        &FIELDS[self.row]
    }

    #[allow(clippy::type_complexity)]
    fn config(
        &self,
    ) -> Result<
        (
            config::Policy,
            Vec<String>,
            Option<config::Activity>,
            Option<config::Pane>,
            Option<config::Start>,
            Option<f64>,
            Option<bool>,
        ),
        String,
    > {
        let v = |name: &str| self.values[field_at(name)].trim();
        let num = |name: &str, what: &str| -> Result<Option<f64>, String> {
            match v(name) {
                "" => Ok(None),
                t => t
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{name}: {what}, not {t:?}")),
            }
        };
        let flag = |name: &str| match v(name) {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let text = |name: &str| Some(v(name).to_owned()).filter(|t| !t.is_empty());
        let names = |name: &str| -> Vec<String> {
            v(name)
                .split(',')
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .collect()
        };
        let columns = names("columns");
        let env = names("env");
        // Refuse a name on the row: written into the file's flow sequence, one carrying YAML
        // punctuation would be read back as something other than a string.
        for key in &env {
            config::env_name(key).map_err(|e| format!("env: {e:#}"))?;
        }
        let policy = config::Policy {
            timeout_min: num("timeout_min", "a number of minutes, as in 30")?,
            write: flag("write"),
            codex_full_access: flag("codex_full_access"),
            overlap: match v("overlap") {
                "skip" => Some(config::Overlap::Skip),
                "allow" => Some(config::Overlap::Allow),
                "replace" => Some(config::Overlap::Replace),
                _ => None,
            },
            catch_up: match v("catch_up") {
                "skip" => Some(config::CatchUp::Skip),
                "once" => Some(config::CatchUp::Once),
                _ => None,
            },
            notify: flag("notify"),
            archive_transcript: flag("archive_transcript"),
            env: Some(env).filter(|e: &Vec<String>| !e.is_empty()),
            model: text("model"),
            codex_model: text("codex_model"),
            bedrock: flag("bedrock"),
            aws_profile: text("aws_profile"),
            aws_region: text("aws_region"),
            harness: harness::KNOWN
                .into_iter()
                .find(|k| k.to_string() == v("harness")),
        };
        // Session overrides bypass file resolution, so validate Bedrock credentials here too.
        config::bedrock_aws(
            policy.bedrock,
            policy.aws_profile.as_deref(),
            policy.aws_region.as_deref(),
        )
        .map_err(|e| format!("{e:#}"))?;
        let spark = if ["bars", "bucket", "metric", "bound"]
            .iter()
            .all(|f| v(&format!("activity.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Activity::default();
            let s = config::Activity {
                bars: match v("activity.bars") {
                    "" => built.bars,
                    t => t.parse().map_err(|_| {
                        format!("activity.bars: a whole number, as in 16, not {t:?}")
                    })?,
                },
                bucket: text("activity.bucket").unwrap_or(built.bucket),
                metric: text("activity.metric").unwrap_or(built.metric),
                bound: text("activity.bound").unwrap_or(built.bound),
            };
            // Name the field the message is about, so the error lands on it.
            s.check().map_err(|e| {
                let e = format!("{e:#}");
                let field = ["bars", "bucket", "metric", "bound"]
                    .into_iter()
                    .find(|f| e.starts_with(&format!("activity {f}")))
                    .unwrap_or("bars");
                format!(
                    "activity.{field}: {}",
                    e.trim_start_matches(&format!("activity {field} "))
                )
            })?;
            Some(s)
        };
        let pane = if ["at", "ratio"]
            .iter()
            .all(|f| v(&format!("pane.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Pane::default();
            let p = config::Pane {
                at: text("pane.at").unwrap_or(built.at),
                ratio: match v("pane.ratio") {
                    "" => built.ratio,
                    t => t
                        .parse()
                        .map_err(|_| format!("pane.ratio: a whole percent, as in 50, not {t:?}"))?,
                },
            };
            // Name the field the message is about, so the error lands on it.
            p.check().map_err(|e| {
                let e = format!("{e:#}");
                let field = ["at", "ratio"]
                    .into_iter()
                    .find(|f| e.starts_with(&format!("pane {f} ")))
                    .unwrap_or("at");
                format!(
                    "pane.{field}: {}",
                    e.trim_start_matches(&format!("pane {field} "))
                )
            })?;
            Some(p)
        };
        let start = if ["harness", "pane"]
            .iter()
            .all(|f| v(&format!("start.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Start::default();
            Some(config::Start {
                harness: harness::KNOWN
                    .into_iter()
                    .find(|k| k.to_string() == v("start.harness"))
                    .unwrap_or(built.harness),
                pane: flag("start.pane").unwrap_or(built.pane),
            })
        };
        let mark = num("confirm_secs", "seconds, as in 2")?;
        if let Some(m) = mark {
            config::check_confirm_secs(m).map_err(|e| {
                let e = format!("{e:#}");
                format!(
                    "confirm_secs: {}",
                    e.trim_start_matches(&format!("confirm_secs {m}: "))
                )
            })?;
        }
        Ok((
            policy,
            columns,
            spark,
            pane,
            start,
            mark,
            flag("whole_columns"),
        ))
    }

    fn trim_num(v: f64) -> String {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }

    /// Step choices cyclically or numbers on their step grid, floored at zero.
    /// Non-numeric built-ins step from zero; return whether the value changed.
    fn turn(&mut self, back: bool) -> bool {
        let f = self.field();
        let value = self.values[self.row].clone();
        if let Some(step) = f.step() {
            let base = if value.is_empty() { f.builtin } else { &value };
            let now: f64 = base.parse().unwrap_or(0.0);
            let next = (now + if back { -step } else { step }).max(0.0);
            self.values[self.row] = Self::trim_num((next / step).round() * step);
            return self.values[self.row] != value;
        }
        let ring = f.ring(&value);
        if ring.is_empty() {
            return false;
        }
        // A field that also types has one more stop past the words: the slot typed into.
        let stops = ring.len() + f.typed() as usize;
        let at = if self.open {
            ring.len()
        } else {
            f.stop(&ring, &value)
        };
        let next = (at + if back { stops - 1 } else { 1 }) % stops;
        if next == ring.len() {
            self.enter();
            self.values[self.row].clear();
            return false;
        }
        self.open = false;
        self.values[self.row] = ring[next].clone();
        next != at
    }

    /// The folding group's head is a stop of its own on the walk, above its first field.
    /// A shut head keeps the walk; only `enter` or `→` goes past it.
    fn down(&mut self) {
        if self.on_head {
            self.on_head = self.shut;
        } else if self.row + 1 < FIELDS.len() {
            self.step(self.row + 1);
            self.on_head = self.row == fold_row();
        }
    }

    fn up(&mut self) {
        if !self.on_head && self.row == fold_row() {
            self.on_head = true;
        } else if self.row > 0 {
            self.step(self.row - 1);
        }
    }

    /// Validate changed values and focus the field named by a validation error.
    fn commit(&mut self) -> ConfigAction {
        let changed = self.values[self.row] != self.before;
        match self.config() {
            Err(e) if e.starts_with(&format!("{}:", self.field().name)) => {
                self.error = Some(e);
                ConfigAction::Stay
            }
            Err(e) => {
                self.open = false;
                self.go(FIELDS
                    .iter()
                    .position(|f| e.starts_with(&format!("{}:", f.name)))
                    .unwrap_or(self.row));
                self.error = Some(e);
                ConfigAction::Stay
            }
            Ok((p, c, s, pn, st, m, w)) => {
                self.open = false;
                if changed {
                    ConfigAction::Save(Box::new(p), c, s, pn, st, m, w)
                } else {
                    ConfigAction::Stay
                }
            }
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> ConfigAction {
        self.error = None;
        // The head takes no value: it opens or shuts the section, or it moves off itself.
        if self.head() {
            match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                KeyCode::Enter | KeyCode::Right => {
                    self.shut = false;
                    self.on_head = false;
                }
                KeyCode::Left => self.shut = true,
                KeyCode::Up => self.up(),
                KeyCode::Down => self.down(),
                _ => {}
            }
            return ConfigAction::Stay;
        }
        if !self.open {
            match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ' | '[' | ']')
                    if matches!(self.field().input, Answer::Columns) =>
                {
                    self.before = self.values[self.row].clone();
                    if let Arranged::Shown(cols) = self.arrange.key(code) {
                        self.values[self.row] = cols.join(", ");
                        return self.commit();
                    }
                }
                KeyCode::Left | KeyCode::Right => {
                    self.before = self.values[self.row].clone();
                    if self.turn(code == KeyCode::Left) {
                        return self.commit();
                    }
                }
                KeyCode::Backspace if !self.values[self.row].is_empty() => {
                    self.before = self.values[self.row].clone();
                    self.values[self.row].clear();
                    if matches!(self.field().input, Answer::Columns) {
                        self.arrange = ColumnForm::new(&built_columns());
                    }
                    return self.commit();
                }
                KeyCode::Enter
                    if matches!(self.field().input, Answer::Typed | Answer::Number(_)) =>
                {
                    self.enter()
                }
                // The words are already picked by the arrows; enter goes on to the next setting.
                KeyCode::Enter | KeyCode::Down => self.down(),
                KeyCode::Up => self.up(),
                // Typing on a field that also types goes into its slot.
                KeyCode::Char(_)
                    if matches!(self.field().input, Answer::PickOrType(..))
                        && !mods.contains(KeyModifiers::CONTROL) =>
                {
                    self.enter();
                    return self.key(code, mods);
                }
                KeyCode::Char(c) if !self.field().typed() => {
                    let f = self.field();
                    let opts = f.picks().unwrap_or_default();
                    if let Some(o) = opts
                        .iter()
                        .find(|o| (**o == "-" && c == '-') || f.label(o).starts_with(c))
                    {
                        self.before = self.values[self.row].clone();
                        self.values[self.row] = if *o == "-" {
                            String::new()
                        } else {
                            (*o).to_owned()
                        };
                        return self.commit();
                    }
                }
                _ => {}
            }
            return ConfigAction::Stay;
        }
        match code {
            KeyCode::Esc => {
                self.values[self.row] = std::mem::take(&mut self.before);
                self.open = false;
            }
            KeyCode::Enter => {
                let action = self.commit();
                if self.error.is_none() {
                    self.down();
                }
                return action;
            }
            // Arrows past either end of the slot step back onto the words.
            KeyCode::Left | KeyCode::Right
                if mods.is_empty() && matches!(self.field().input, Answer::PickOrType(..)) && {
                    let v = &self.values[self.row];
                    let at = snap(v, self.cursor);
                    if code == KeyCode::Left {
                        at == 0
                    } else {
                        at == v.len()
                    }
                } =>
            {
                if self.turn(code == KeyCode::Left) {
                    return self.commit();
                }
            }
            _ => {
                // Typing over a word the field offers starts from empty rather than
                // appending to it.
                if matches!(self.field().input, Answer::PickOrType(..))
                    && self.field().picked(&self.values[self.row])
                {
                    self.values[self.row].clear();
                }
                if let Some(at) = edit(&mut self.values[self.row], self.cursor, code, mods) {
                    self.cursor = at;
                }
            }
        }
        ConfigAction::Stay
    }

    /// Render editor rows and return the selected row's line offset.
    fn lines(&self, columns: u16) -> (Vec<Line<'static>>, usize) {
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                Span::styled("config", Style::default().fg(ORANGE)),
                Span::styled("  jobs.yaml", dim()),
            ]),
        ];
        let label_w = FIELDS
            .iter()
            .map(|f| f.short.chars().count())
            .max()
            .unwrap_or(0);
        let indent = 4 + label_w + 2;
        let mut head: Option<(&str, &str)> = None;
        let mut at = 0;
        for (i, f) in FIELDS.iter().enumerate() {
            if head.map(|(g, _)| g) != Some(f.group) {
                let (name, what) = GROUPS
                    .iter()
                    .find(|(g, _)| *g == f.group)
                    .copied()
                    .unwrap_or((f.group, ""));
                lines.push(Line::default());
                // The folding group's head is a row of its own, so it takes the selection.
                let folds = f.group == SHUT;
                let picked = folds && self.on_head && i == self.row;
                if picked {
                    at = lines.len();
                }
                let mut spans = vec![Span::styled(
                    name.to_owned(),
                    if picked {
                        lit()
                    } else {
                        Style::default().fg(ORANGE)
                    },
                )];
                spans.push(Span::styled(format!("  {what}"), dim()));
                // The mark says the section folds, and stays whichever way it is folded.
                if folds {
                    spans.push(Span::styled(
                        if self.shut {
                            "  ▸  enter opens it"
                        } else if picked {
                            "  ▾  ← shuts it"
                        } else {
                            "  ▾"
                        }
                        .to_owned(),
                        if picked {
                            Style::default().fg(ORANGE)
                        } else {
                            dim()
                        },
                    ));
                }
                lines.push(Line::from(spans));
                if picked {
                    let last = lines.len() - 1;
                    on_row(&mut lines[last..], columns);
                }
            }
            if self.shut && f.group == SHUT {
                head = Some((f.group, f.sub));
                continue;
            }
            if !f.sub.is_empty() && head.map(|(_, b)| b) != Some(f.sub) {
                lines.push(Line::from(Span::styled(format!("  {}", f.sub), dim())));
            }
            head = Some((f.group, f.sub));
            let selected = i == self.row;
            let row = |open| {
                let mut spans = vec![Span::styled(
                    format!("    {:<label_w$}  ", f.short),
                    if selected { lit() } else { bold() },
                )];
                spans.extend(self.control(i, open));
                if selected && let Some(e) = &self.error {
                    spans.push(Span::styled(
                        format!("  {e}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                // Wrap controls between words at a stable indent, independent of selection.
                flow(spans, indent, columns as usize)
            };
            let open = selected && self.open;
            if selected {
                at = lines.len();
            }
            let mut drawn = row(open);
            if selected {
                on_row(&mut drawn, columns);
            }
            // Keep the closed control's height while editing so later rows stay put.
            if open {
                let shut = row(false).len();
                drawn.resize_with(drawn.len().max(shut), Line::default);
            }
            lines.extend(drawn);
        }
        lines.push(Line::default());
        // Wrap explanations at the row indent and reserve the tallest explanation's height.
        let f = self.field();
        let (name, long) = if self.head() {
            (SHUT, SHUT_LONG)
        } else {
            (f.name, f.long)
        };
        let explain = format!("    {name:<label_w$}  ");
        let room = (columns as usize)
            .saturating_sub(explain.chars().count())
            .max(20);
        let tall = FIELDS
            .iter()
            .map(|f| f.long)
            .chain([SHUT_LONG])
            .map(|l| wrap(l, room).len())
            .max()
            .unwrap_or(1);
        let mut rest = wrap(long, room).into_iter();
        lines.push(Line::from(vec![
            Span::styled(explain, bold()),
            Span::raw(rest.next().unwrap_or_default()),
        ]));
        let mut n = 1;
        for l in rest {
            lines.push(Line::from(format!("{:indent$}{l}", "")));
            n += 1;
        }
        lines.extend((n..tall).map(|_| Line::default()));
        (lines, at)
    }

    fn paragraph(&self, body: Rect) -> Paragraph<'static> {
        let (lines, at) = self.lines(body.width);
        let height = body.height as usize;
        let top = at
            .saturating_sub(height / 2)
            .min(lines.len().saturating_sub(height));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top as u16, 0))
    }

    fn control(&self, i: usize, open: bool) -> Vec<Span<'static>> {
        let (f, value) = (&FIELDS[i], &self.values[i]);
        if matches!(f.input, Answer::Columns) {
            let built = value.is_empty();
            let mut spans = vec![];
            for (n, c) in self.arrange.order.iter().enumerate() {
                if n == self.arrange.shown {
                    spans.push(Span::styled("· ", dim()));
                }
                // Pad inside the span so the cursor's block sits even around the
                // name, and keep the gap to the next name outside it.
                spans.push(Span::styled(
                    format!(" {c} "),
                    if n == self.arrange.at && i == self.row {
                        pressed()
                    } else if n < self.arrange.shown && !built {
                        bold()
                    } else {
                        dim()
                    },
                ));
                spans.push(Span::raw(" "));
            }
            return spans;
        }
        control(f, f.builtin, value, open, self.cursor, i == self.row)
    }

    fn line(&self) -> Line<'static> {
        let f = self.field();
        if self.head() {
            return Line::from(vec![
                Span::styled(format!("{SHUT} › "), Style::default().fg(ORANGE)),
                Span::styled(
                    if self.shut {
                        "enter or → opens the section"
                    } else {
                        "← shuts the section · → goes into it"
                    },
                    dim(),
                ),
            ]);
        }
        let mut spans = vec![Span::styled(
            format!("{} › ", f.name),
            Style::default().fg(ORANGE),
        )];
        // Keep this line short enough to avoid wrapping and shifting the list.
        let default = if f.builtin == SYSTEM {
            "default passes nothing".to_owned()
        } else {
            format!("default: {}", f.builtin)
        };
        let help = if self.open {
            "enter keeps it · esc reverts".to_owned()
        } else {
            match f.input {
                Answer::Columns => {
                    "space shows or hides · [ ] move it · bksp the built-in set".to_owned()
                }
                Answer::Pick(_) => default,
                Answer::PickOrType(_, what) => format!("or type {what} · enter next · {default}"),
                _ => format!("enter types it · {default}"),
            }
        };
        spans.push(Span::styled(help, dim()));
        Line::from(spans)
    }
}

/// Wrap between spans with hanging indentation; keep the first two spans together.
/// The widget clips any span wider than the available space.
fn flow(spans: Vec<Span<'static>>, indent: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![];
    let mut row: Vec<Span<'static>> = vec![];
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w > width && used > indent {
            lines.push(Line::from(std::mem::take(&mut row)));
            row.push(Span::raw(" ".repeat(indent)));
            used = indent;
        }
        used += w;
        row.push(span);
    }
    if !row.is_empty() {
        lines.push(Line::from(row));
    }
    lines
}

/// Wrap at spaces; oversized words occupy a line of their own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + word.chars().count() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

fn built_columns() -> Vec<String> {
    config::DEFAULT_COLUMNS
        .iter()
        .map(|c| (*c).to_owned())
        .collect()
}

enum Arranged {
    Stay,
    Shown(Vec<String>),
}

/// `order[..shown]` is the visible column set.
#[derive(Debug, Clone, PartialEq)]
struct ColumnForm {
    order: Vec<String>,
    shown: usize,
    at: usize,
}

impl ColumnForm {
    fn new(columns: &[String]) -> Self {
        let mut order = columns.to_vec();
        order.extend(
            config::COLUMNS
                .iter()
                .filter(|c| !columns.iter().any(|h| h == *c))
                .map(|c| (*c).to_owned()),
        );
        Self {
            shown: columns.len(),
            at: 0,
            order,
        }
    }

    fn chosen(&self) -> Vec<String> {
        self.order[..self.shown].to_vec()
    }

    /// Hiding a column keeps the cursor on it, so space can immediately restore it.
    fn key(&mut self, code: KeyCode) -> Arranged {
        match code {
            KeyCode::Left | KeyCode::Right => {
                let n = self.order.len();
                self.at = (self.at + if code == KeyCode::Left { n - 1 } else { 1 }) % n;
                Arranged::Stay
            }
            KeyCode::Char(' ') if self.at < self.shown => {
                let c = self.order.remove(self.at);
                self.shown -= 1;
                self.order.insert(self.shown, c);
                self.at = self.shown;
                Arranged::Shown(self.chosen())
            }
            KeyCode::Char(' ') => {
                let c = self.order.remove(self.at);
                self.order.insert(self.shown, c);
                self.shown += 1;
                self.at = self.shown - 1;
                Arranged::Shown(self.chosen())
            }
            KeyCode::Char('[') if self.at < self.shown && self.at > 0 => {
                self.order.swap(self.at, self.at - 1);
                self.at -= 1;
                Arranged::Shown(self.chosen())
            }
            KeyCode::Char(']') if self.at + 1 < self.shown => {
                self.order.swap(self.at, self.at + 1);
                self.at += 1;
                Arranged::Shown(self.chosen())
            }
            _ => Arranged::Stay,
        }
    }
}

enum Mode {
    Normal,
    Filter,
    Job(Box<JobForm>),
    Config(Box<ConfigForm>),
    Folder(Input),
    Rename(Input),
    /// Wrapped line offset in the usage guide.
    Guide(usize),
}

/// Guide entries with an empty key are headings; tests check keys against docs/dashboard.md.
const GUIDE: &[(&str, &str)] = &[
    ("", "Rows"),
    (
        "↑ ↓",
        "move between rows; ↑ past the first table lands on the menu, where ← → pick a button",
    ),
    (
        "enter",
        "start the job, follow the running run, open the session or finished run as a viewer, return to a viewer that is alive; on the menu row, give the picked button's screen the keys in the pane: add folder, jobs, defaults, help; on the jobs screen's last row, the wizard on a new job",
    ),
    (
        "shift+enter",
        "the same over the whole frame; ctrl+z or esc come back to the pane; with an instruction typed, a line break",
    ),
    (
        "ctrl+x twice",
        "stop the run or session; delete a job with no run in flight; hide a finished run; forget a Codex daemon thread; remove a pinned folder",
    ),
    (
        "ctrl+e",
        "edit the selected job in the wizard, on the jobs screen",
    ),
    (
        "ctrl+p",
        "pin the selected row's folder: it keeps a row after the last session there leaves",
    ),
    ("ctrl+s", "regroup sessions by state or by directory"),
    (
        "ctrl+f",
        "filter rows by text; enter keeps the filter, esc clears it",
    ),
    (
        "ctrl+n",
        "rename the selected Claude session; the title is written where claude --resume reads it",
    ),
    (
        "ctrl+r",
        "reload now; the dashboard reloads every second on its own",
    ),
    ("", "Composer"),
    (
        "any key",
        "types an instruction; enter starts a session with it in the selected row's directory",
    ),
    (
        "shift+tab",
        "the harness the next session starts under; the composer's prefix shows it, and a pi ends with the viewer it runs in",
    ),
    (
        "ctrl+v",
        "paste the clipboard's image; its path is typed into the instruction",
    ),
    (
        "← →",
        "move a character in the instruction, and in every prompt that takes text; alt+← alt+→ a word; ctrl+a ctrl+e to the ends",
    ),
    (
        "backspace",
        "delete a character; ctrl+w alt+d a word; ctrl+u ctrl+k everything before or after the cursor",
    ),
    ("", "Viewers"),
    (
        "tab",
        "into the pane's viewer or a button's screen and back out to the list, as does ← with the client's composer empty; a form that uses tab itself, the folder prompt or an open field, is left with ctrl+z or esc; shift+tab inside a viewer is the client's",
    ),
    (
        "ctrl+z",
        "back to the list from a viewer or a button's screen; the viewer stays alive and enter on its row gives it the keys again",
    ),
    (
        "ctrl+\\",
        "from the list, the pane on or off; inside a viewer, beside the list or over the whole frame",
    ),
    ("wheel", "scrolls the pane's viewer back, focused or not"),
    ("", "Leaving"),
    (
        "esc",
        "backs out one thing at a time: an armed ctrl+x, the instruction, the jobs screen, the dashboard",
    ),
    ("ctrl+c twice", "quit"),
    ("ctrl+g", "this guide; ↑ ↓ scroll it, esc closes it"),
];

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
    claude: PathBuf,
    /// Fallback launch directory and base for relative folder input.
    cwd: PathBuf,
    /// Index into `MENU`.
    menu: usize,
    data: Data,
    rows: Vec<Row>,
    /// Inactive list: jobs for menu previews, or main rows beside the jobs pane.
    other: Vec<Row>,
    /// Indexes into `rows` that pass the filter; the cursor indexes this list.
    visible: Vec<usize>,
    cursor: usize,
    scroll: usize,
    by_state: bool,
    jobs_view: bool,
    widths: Widths,
    filter: Input,
    mode: Mode,
    status: String,
    /// Composer text; `caret` is a byte offset.
    text: String,
    caret: usize,
    /// The PNGs pasted into the instruction, in the order their markers were typed.
    images: Vec<PathBuf>,
    /// Index into `harness::KNOWN` for the next launch.
    harness: usize,
    /// Background launches keyed by placeholder row id.
    started: Vec<(String, mpsc::Receiver<Launched>)>,
    /// Placeholder rows until the registry reports the launched sessions.
    pending: Vec<Pending>,
    opening: Option<Opening>,
    tick: usize,
    refreshed: Instant,
    loading: Option<mpsc::Receiver<Result<Data>>>,
    loading_started: Option<Instant>,
    /// The last read failed, so every count and row on screen is the read before it.
    stale: bool,
    /// A transition happened after the current read started. Discard that read and run again.
    reload_pending: bool,
    /// Harness commands can take seconds. Keep input and drawing alive while they finish.
    stopping: Vec<PendingStop>,
    /// Successful delete/forget commands take effect here before the registry catches up.
    removed_sessions: HashSet<String>,
    feedback: Option<(&'static str, Instant)>,
    /// Row key awaiting a second ctrl+x, until another key or `confirm_secs` expires.
    armed: Option<String>,
    armed_at: Instant,
    /// Require a second ctrl+c so an interrupt aimed at a closing viewer cannot quit the dashboard.
    quit_armed: Option<Instant>,
    log: Option<PathBuf>,
    viewers: Vec<Open>,
    /// The viewer that has the pane and the keys; an index into `viewers`.
    focus: Option<usize>,
    /// The real terminal's default colors, probed once at start, for viewers that ask.
    colors: viewer::Colors,
    mouse_capture: bool,
    needs_clear: bool,
    /// The last frame's rows and columns.
    size: (u16, u16),
    /// Last drawn viewer rectangle, also used for sizing and mouse coordinates.
    pane: Rect,
    /// Persistent layout preference, initialized from `start.pane`.
    split: bool,
    /// Temporary full-frame override from shift+enter; cleared on exit or split toggle.
    full: bool,
    /// Selected viewer key and the time its cursor rest began.
    rest: Option<(String, Instant)>,
    /// Avoid retrying a refused speculative attach until the cursor moves.
    prespawned: Option<String>,
    /// Where the list rows were drawn last, so a click finds its row.
    list_area: Rect,
}

const REST: Duration = Duration::from_millis(400);

/// Shorter rest in split view reduces visible blank time while allowing held arrows through.
const REST_SPLIT: Duration = Duration::from_millis(50);

const WHEEL_LINES: i32 = 3;

const AGENT_VIEW_TITLE: &str = "claude agents";

/// Focused viewers only, so this is not the number of harness clients a dashboard runs: the
/// prespawned pool sits outside it and the ceiling is `MAX_FOCUSED_VIEWERS + SPECULATIVE_VIEWERS`,
/// each client its own process at roughly 165MB. Five is deliberate rather than a bug to fix: the
/// two prespawned clients are what make peek instant, and that is worth their 330MB on a machine
/// with memory to spare. Evict only listed-session Claude attaches, which can be reopened
/// speculatively; other clients stay alive and may exceed this cap too.
const MAX_FOCUSED_VIEWERS: usize = 3;

/// Two speculative slots avoid reattaching when moving between adjacent rows. They are held on top
/// of `MAX_FOCUSED_VIEWERS` rather than inside it, so raising either constant costs a whole client.
const SPECULATIVE_VIEWERS: usize = 2;

/// Launch status and, on failure, the prompt to restore.
type Launched = (String, Option<String>);

/// Match a placeholder to the registry using the short id returned by `claude --bg`.
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

// Expire placeholders if the launch never appears in the registry.
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
        coordinator: false,
        activity: Vec::new(),
    }
}

struct Open {
    /// The row key it opened from.
    key: String,
    /// `attach`, `codex`, `claude agents`, `logs`: the word in the status line.
    what: String,
    viewer: Viewer,
    /// A Codex thread to record from its rollout once the viewer is left or ends.
    record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    recorded: bool,
    first_paint_logged: bool,
    /// For a speculative viewer, which was never focused, this is when it was spawned.
    last_focused: Instant,
    /// Unfocused attaches use the separate speculative pool until first focus.
    speculative: bool,
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
        let data = Data::load(jobs_path, state, claude)?;
        let start = data.start;
        Ok(Self {
            exe: exe.to_owned(),
            jobs_path: jobs_path.to_owned(),
            state: state.to_owned(),
            claude: claude.to_owned(),
            cwd: std::env::current_dir().context("dashboard working directory")?,
            menu: 0,
            split: start.pane,
            data,
            rows: vec![],
            visible: vec![],
            cursor: 0,
            scroll: 0,
            by_state: false,
            jobs_view: false,
            other: vec![],
            widths: Widths::new(),
            filter: Input::default(),
            mode: Mode::Normal,
            status: String::new(),
            text: String::new(),
            caret: 0,
            images: Vec::new(),
            harness: Self::harness_at(Some(start.harness)),
            started: Vec::new(),
            pending: Vec::new(),
            opening: None,
            tick: 0,
            refreshed: Instant::now(),
            loading: None,
            loading_started: None,
            stale: false,
            reload_pending: false,
            stopping: Vec::new(),
            removed_sessions: HashSet::new(),
            feedback: None,
            armed: None,
            armed_at: Instant::now(),
            quit_armed: None,
            log: None,
            viewers: Vec::new(),
            focus: None,
            colors: viewer::Colors::default(),
            mouse_capture: false,
            needs_clear: false,
            size: (24, 80),
            pane: Rect::new(0, 0, 80, 23),
            full: false,
            rest: None,
            prespawned: None,
            list_area: Rect::default(),
        })
    }

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

    #[cfg(test)]
    fn refresh(&mut self) -> Result<()> {
        let data = Data::load(&self.jobs_path, &self.state, &self.claude)?;
        self.apply(data);
        Ok(())
    }

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

    /// Invalidate an in-flight read without starting a second reader.
    fn invalidate(&mut self) {
        if self.loading.is_some() {
            self.reload_pending = true;
        } else {
            self.reload();
        }
    }

    fn poll(&mut self) {
        self.poll_stops();
        let mut launched = false;
        for (id, rx) in std::mem::take(&mut self.started) {
            match rx.try_recv() {
                Ok((message, retry)) => {
                    match retry {
                        Some(prompt) => {
                            self.pending.retain(|p| p.session.session_id != id);
                            if self.text.is_empty() {
                                self.fill(prompt);
                            }
                            self.status = message;
                        }
                        None => {
                            if let Some(p) =
                                self.pending.iter_mut().find(|p| p.session.session_id == id)
                            {
                                p.short = short_id(&message);
                            }
                            // The new row is the report; only a failure needs words.
                            self.status.clear();
                        }
                    }
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
                self.stale = true;
                self.refreshed = Instant::now();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status = "reload failed: worker disconnected".into();
                self.stale = true;
                self.refreshed = Instant::now();
            }
        }
    }

    /// The counts, marked when they are the last good read rather than a current one. The status
    /// line carries the reason and the next action overwrites it; this stays until a read succeeds.
    fn header_summary(&self) -> Line<'static> {
        let summary = self.data.summary(spinner_frame(self.tick));
        if !self.stale {
            return summary;
        }
        // First, not last: a narrow header clips its tail, and the warning outranks the counts.
        let mut spans = vec![Span::styled(
            "! stale  ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(summary.spans);
        Line::from(spans)
    }

    fn apply(&mut self, mut data: Data) {
        if self.status.starts_with("reload failed:") {
            self.status.clear();
        }
        self.stale = false;
        self.removed_sessions
            .retain(|id| data.sessions.iter().any(|s| &s.session_id == id));
        data.sessions
            .retain(|s| !self.removed_sessions.contains(&s.session_id));
        // Follow a placeholder into its registry row only if the cursor is still on it.
        let on = self
            .selected()
            .and_then(|r| r.kind.key().map(str::to_owned));
        let mut follow = true;
        self.pending.retain(|p| {
            let listed = data.sessions.iter().any(|s| p.matches(s));
            if listed && on.as_deref() != Some(p.session.session_id.as_str()) {
                follow = false;
            }
            !listed && p.at.elapsed() < PENDING_TTL
        });
        data.sessions
            .extend(self.pending.iter().map(|p| p.session.clone()));
        let arrived = data
            .sessions
            .iter()
            .filter(|s| {
                !self
                    .data
                    .sessions
                    .iter()
                    .any(|o| o.session_id == s.session_id)
            })
            .max_by_key(|s| s.started)
            .map(|s| s.session_id.clone());
        self.data = data;
        self.rebuild();
        if let Some(id) = arrived
            && follow
        {
            self.select_new(&id);
        }
        self.refreshed = Instant::now();
    }

    /// Do not change the launch target while typing or move selection away from a focused viewer.
    fn select_new(&mut self, id: &str) {
        if self.focus.is_some() || !self.text.is_empty() {
            return;
        }
        if let Some(i) = self
            .visible
            .iter()
            .position(|&i| self.rows[i].kind.key() == Some(id))
        {
            self.cursor = i;
            self.settle();
        }
    }

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
        let in_pane = self.jobs_view && self.split_active();
        self.rows = if in_pane { vec![] } else { menu_rows() };
        self.rows.extend(self.data.rows_excluding(
            self.by_state,
            self.jobs_view,
            &deleting,
            &mut self.widths,
        ));
        self.other = if self.jobs_view { menu_rows() } else { vec![] };
        self.other.extend(self.data.rows_excluding(
            self.by_state,
            !self.jobs_view,
            &deleting,
            &mut self.widths,
        ));
        self.apply_filter();
        if let Some(k) = &keep
            && let Some(i) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].kind.key() == Some(k.as_str()))
        {
            self.cursor = i;
        } else if keep.is_none() {
            let below = |i: &usize| {
                let k = &self.rows[*i].kind;
                k.selectable() && *k != Kind::Menu
            };
            self.cursor = self.visible.iter().position(below).unwrap_or(0);
        }
        self.settle();
        self.timing("rebuild", started);
    }

    /// Keep matching rows and their group headers. Exclude other unselectable kinds
    /// when filtering, or they can hide the header above them.
    fn apply_filter(&mut self) {
        let needle = self.filter.text.to_lowercase();
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
        // Leaving the menu row forgets the picked button, so coming back lands on the first.
        if !self.on_menu() {
            self.menu = 0;
        }
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
            Kind::Folder(dir) => self
                .data
                .folders
                .iter()
                .find(|p| &fleet::tilde(p) == dir)
                .cloned(),
            _ => None,
        }
    }

    /// `dir` is the child's cwd, used by `cones run --prompt`.
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

    /// The viewer a row is already open in. A pi never reports a session id the composer
    /// could have used as a key, and reports a new one once it writes its session file, so
    /// its viewer is paired with the process it holds instead. The launch key carries the
    /// harness word so a pid the kernel has recycled cannot pair a row with another
    /// harness's viewer, the way `fleet::terminate` refuses to signal a reused pid.
    fn viewer_of(&self, kind: &Kind) -> Option<usize> {
        if let Some(i) = Self::viewer_key(kind).and_then(|k| self.viewer_index(&k)) {
            return Some(i);
        }
        let Kind::Session(id, _) = kind else {
            return None;
        };
        let s = self.data.sessions.iter().find(|s| &s.session_id == id)?;
        let (pid, launched) = (s.pid?, format!("{}:start:", s.harness));
        self.viewers
            .iter()
            .position(|o| o.viewer.pid() == pid && o.key.starts_with(&launched))
    }

    fn frame(&self) -> Rect {
        Rect::new(0, 0, self.size.1, self.size.0)
    }

    fn split_active(&self) -> bool {
        self.split && !(self.full && self.pane_focused())
    }

    fn rest_for(&self) -> Duration {
        if self.split_active() {
            REST_SPLIT
        } else {
            REST
        }
    }

    /// The pane takes `pane.ratio` percent of the frame and the list keeps the rest,
    /// less the divider between them.
    fn split_areas(&self, frame: Rect) -> [Rect; 3] {
        let list = |total: u16| {
            let share = u32::from(100u16.saturating_sub(self.data.pane.ratio));
            (u32::from(total) * share / 100) as u16
        };
        if self.data.pane.at == "bottom" {
            return Layout::vertical([
                Constraint::Length(list(frame.height)),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .areas(frame);
        }
        Layout::horizontal([
            Constraint::Length(list(frame.width)),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(frame)
    }

    /// Use the same viewer size for spawn, focus and draw. In split view, overlay hints
    /// on its last row so focus changes never resize the harness.
    fn pane(&self, frame: Rect) -> Rect {
        if self.split_active() {
            return self.split_areas(frame)[2];
        }
        let height = frame.height.saturating_sub(1).max(1);
        Rect { height, ..frame }
    }

    /// Session rows show only their own viewer. Non-session rows may keep the last focused viewer.
    fn shown(&self) -> Option<usize> {
        if self.focus.is_some() {
            return self.focus;
        }
        if self.panel().is_some() {
            return None;
        }
        if !self.split_active() {
            return None;
        }
        let own = self.selected().and_then(|r| self.viewer_of(&r.kind));
        if own.is_some() || matches!(self.selected().map(|r| &r.kind), Some(Kind::Session(..))) {
            return own;
        }
        self.most_recently_focused()
    }

    fn selected_session(&self) -> Option<&fleet::Session> {
        let Some(Kind::Session(id, _)) = self.selected().map(|r| &r.kind) else {
            return None;
        };
        self.data.sessions.iter().find(|s| &s.session_id == id)
    }

    fn rename_selected(&mut self) {
        match self.selected_session() {
            Some(s) if s.harness == "claude" && s.transcript_path.is_some() => {
                self.mode = Mode::Rename(Input::new(s.title.clone().unwrap_or_default()));
            }
            Some(s) if s.harness == "claude" => {
                self.status = "this session has no transcript yet".into();
            }
            Some(_) => self.status = "only Claude sessions can be renamed here".into(),
            None => self.status = "ctrl+n renames the selected session".into(),
        }
    }

    fn most_recently_focused(&self) -> Option<usize> {
        self.viewers
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.speculative)
            .max_by_key(|(_, o)| o.last_focused)
            .map(|(i, _)| i)
    }

    fn panel(&self) -> Option<&'static str> {
        if self.focus.is_some() {
            return None;
        }
        let open = match self.mode {
            Mode::Guide(_) => Some("help"),
            Mode::Config(_) => Some("config"),
            Mode::Job(_) => Some("jobs"),
            Mode::Folder(_) => Some("folder"),
            _ => self.jobs_view.then_some("jobs"),
        };
        open.or_else(|| {
            matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu)).then(|| MENU[self.menu].0)
        })
    }

    fn panel_shown(&self) -> bool {
        self.split_active() && !self.panel_focused() && self.panel().is_some()
    }

    fn panel_focused(&self) -> bool {
        self.jobs_view
            || matches!(
                self.mode,
                Mode::Guide(_) | Mode::Config(_) | Mode::Job(_) | Mode::Folder(_)
            )
    }

    fn pane_focused(&self) -> bool {
        self.focus.is_some() || self.panel_focused()
    }

    fn config_form(&self) -> Box<ConfigForm> {
        Box::new(ConfigForm::new(
            &config::defaults(&self.jobs_path),
            config::file_columns(&self.jobs_path).as_deref(),
            config::file_activity(&self.jobs_path).as_ref(),
            config::file_pane(&self.jobs_path).as_ref(),
            config::file_start(&self.jobs_path).as_ref(),
            config::file_confirm_secs(&self.jobs_path),
            config::file_whole_columns(&self.jobs_path),
        ))
    }

    fn leave_jobs(&mut self) {
        self.jobs_view = false;
        self.rebuild();
        if let Some(i) = self
            .visible
            .iter()
            .position(|&i| self.rows[i].kind == Kind::Menu)
        {
            self.cursor = i;
        }
    }

    /// Leaving a screen lands on the first session instead of the menu row it was opened from.
    fn select_first_session(&mut self) {
        if let Some(i) = self
            .visible
            .iter()
            .position(|&i| matches!(self.rows[i].kind, Kind::Session(..)))
        {
            self.cursor = i;
        }
    }

    /// A temporary full-frame override returns to split view before changing the persistent layout.
    fn toggle_split(&mut self) {
        let once = std::mem::take(&mut self.full) && self.pane_focused();
        self.split = once || !self.split;
        self.needs_clear = true;
        self.rebuild();
        self.debug(|| format!("split {}", self.split));
    }

    /// First focus promotes a speculative viewer into the live pool and enforces its cap.
    fn focus(&mut self, mut i: usize) {
        if std::mem::take(&mut self.viewers[i].speculative) {
            let spawned = self.viewers[i].last_focused;
            self.timing("viewer_prespawn_hit", spawned);
            while self.live_viewers() > MAX_FOCUSED_VIEWERS {
                let Some(oldest) = self.least_recently_focused(Some(i)) else {
                    break;
                };
                self.close(oldest);
                if oldest < i {
                    i -= 1;
                }
            }
        }
        self.focus = Some(i);
        let pane = self.pane;
        let open = &mut self.viewers[i];
        open.last_focused = Instant::now();
        open.viewer.resize(pane.height, pane.width);
        let line = format!("focus {} ({})", open.key, open.what);
        self.debug(|| line);
    }

    fn open(
        &mut self,
        terminal_size: (u16, u16),
        c: Command,
        what: &str,
        key: String,
        record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    ) {
        self.size = terminal_size;
        self.pane = self.pane(self.frame());
        if let Some(i) = self.viewer_index(&key) {
            self.focus(i);
            return;
        }
        self.debug(|| format!("open {what} as {key}: {c:?}"));
        let normal = SHELL_TTY.get().and_then(|t| t.as_ref());
        match Viewer::spawn(
            c,
            self.pane.height,
            self.pane.width,
            normal,
            self.colors.clone(),
        ) {
            Ok(viewer) => {
                // Evict only after the new viewer starts successfully.
                while self.live_viewers() >= MAX_FOCUSED_VIEWERS {
                    let Some(oldest) = self.least_recently_focused(None) else {
                        break;
                    };
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
                    speculative: false,
                });
                let pid = self.viewers.last().unwrap().viewer.pid();
                self.debug(|| format!("viewer pid {pid}; the dashboard keeps the terminal"));
                self.focus(self.viewers.len() - 1);
            }
            Err(e) => self.status = format!("{what} failed: {e}"),
        }
    }

    fn live_viewers(&self) -> usize {
        self.viewers.iter().filter(|o| !o.speculative).count()
    }

    /// Only listed-session Claude attaches can be reopened quietly; see `MAX_FOCUSED_VIEWERS`.
    fn least_recently_focused(&self, keep: Option<usize>) -> Option<usize> {
        self.viewers
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                !o.speculative
                    && Some(*i) != keep
                    && o.what == "attach"
                    && !o.key.starts_with("run:")
            })
            .min_by_key(|(_, o)| o.last_focused)
            .map(|(i, _)| i)
    }

    fn track_rest(&mut self) {
        let key = self.selected().and_then(|r| Self::viewer_key(&r.kind));
        match (&self.rest, key) {
            (Some((k, _)), Some(key)) if *k == key => {}
            (_, Some(key)) => {
                self.rest = Some((key, Instant::now()));
                self.prespawned = None;
            }
            (_, None) => {
                self.rest = None;
                self.prespawned = None;
            }
        }
    }

    /// Only pre-open Claude attaches: resuming a finished run or starting a Codex client
    /// changes the session or fleet. Done background jobs still have a joinable worker.
    fn prespawn_target(&self) -> Option<(String, PathBuf)> {
        if !matches!(self.mode, Mode::Normal)
            || self.focus.is_some()
            || self.opening.is_some()
            || !self.text.trim().is_empty()
        {
            return None;
        }
        let (rested, since) = self.rest.as_ref()?;
        if since.elapsed() < self.rest_for() || self.prespawned.as_deref() == Some(rested.as_str())
        {
            return None;
        }
        let Some(Kind::Session(id, _)) = self.selected().map(|r| &r.kind) else {
            return None;
        };
        if id != rested
            || id.starts_with("starting:")
            || self.viewer_index(id).is_some()
            || self.stopping.iter().any(|a| &a.id == id)
            || self.removed_sessions.contains(id)
        {
            return None;
        }
        let s = self.data.sessions.iter().find(|s| &s.session_id == id)?;
        if !Self::joinable(s) {
            return None;
        }
        Some((id.clone(), s.cwd.clone()))
    }

    /// Done Claude background jobs still have a worker; failed or stopped jobs do not.
    fn joinable(s: &Session) -> bool {
        s.harness == "claude"
            && !s.own_terminal()
            && !matches!(s.state.as_str(), "failed" | "stopped")
    }

    fn prespawn(&mut self, id: String, cwd: PathBuf) {
        self.prespawned = Some(id.clone());
        let normal = SHELL_TTY.get().and_then(|t| t.as_ref());
        let viewer = harness::adapter(HarnessKind::Claude)
            .and_then(|h| h.attach(&id, &cwd))
            .and_then(|c| {
                let line = format!("{c:?}");
                Viewer::spawn(
                    c,
                    self.pane.height,
                    self.pane.width,
                    normal,
                    self.colors.clone(),
                )
                .map(|v| (v, line))
                .map_err(Into::into)
            });
        let (viewer, command) = match viewer {
            Ok(viewer) => viewer,
            Err(e) => {
                self.debug(|| format!("prespawn {id} failed: {e:#}"));
                return;
            }
        };
        self.viewers.push(Open {
            key: id,
            what: "attach".into(),
            viewer,
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: true,
        });
        self.pool_speculative();
        let open = self.viewers.last().unwrap();
        let line = format!("prespawn {} pid {}: {command}", open.key, open.viewer.pid());
        self.debug(|| line);
    }

    /// Speculative viewers have their own cap, independent of the live pool.
    fn pool_speculative(&mut self) {
        while self.viewers.len() - self.live_viewers() > SPECULATIVE_VIEWERS {
            let oldest = self
                .viewers
                .iter()
                .enumerate()
                .filter(|(_, o)| o.speculative)
                .min_by_key(|(_, o)| o.last_focused)
                .map(|(i, _)| i)
                .unwrap();
            self.close(oldest);
        }
    }

    fn close_orphan_speculative(&mut self) {
        let gone = self.viewers.iter().position(|o| {
            o.speculative && !self.data.sessions.iter().any(|s| s.session_id == o.key)
        });
        if let Some(i) = gone {
            let key = self.viewers[i].key.clone();
            self.debug(|| format!("prespawn {key} dropped: its session left the list"));
            self.close(i);
        }
    }

    /// Once a loop turn, after the reload landed and the viewers were pumped.
    fn prespawn_tick(&mut self) {
        self.close_orphan_speculative();
        self.track_rest();
        if let Some((id, cwd)) = self.prespawn_target() {
            self.prespawn(id, cwd);
        }
    }

    /// Record new Codex threads before closing their viewers so their rows survive.
    fn close(&mut self, i: usize) {
        let had_frame = !self.split_active();
        let open = self.viewers.remove(i);
        match self.focus {
            Some(f) if f == i => {
                self.focus = None;
                self.needs_clear = had_frame;
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

    fn unfocus(&mut self) {
        if self.focus.is_none() {
            return;
        }
        // A full-frame viewer requires a clear; split-view changes use ratatui's diff.
        self.needs_clear = !self.split_active();
        self.full = false;
        let i = self.focus.take().unwrap();
        self.feedback = Some(("return_to_draw", Instant::now()));
        let open = &mut self.viewers[i];
        open.last_focused = Instant::now();
        self.status.clear();
        let record = (!open.recorded).then(|| open.record.clone()).flatten();
        if let Some((dir, since)) = record {
            self.viewers[i].recorded = true;
            // Replace the launch key with the recorded thread id to reuse this client on return.
            if let Some(id) = self.record_codex(&dir, since) {
                self.viewers[i].key = id;
            }
        }
        self.invalidate();
        let open = &self.viewers[i];
        let line = format!(
            "dashboard back from {}; viewer pid {} title {:?}",
            open.what,
            open.viewer.pid(),
            open.viewer.title()
        );
        self.debug(|| line);
        // Drop viewers left in Claude's agent list so the row cannot show or attach another session.
        if self.viewers[i].viewer.title() == Some(AGENT_VIEW_TITLE) {
            self.close(i);
        }
    }

    fn pump(&mut self) -> bool {
        let mut dirty = false;
        let mut i = 0;
        while i < self.viewers.len() {
            let focused = self.focus == Some(i);
            let on_view = self.shown() == Some(i);
            let open = &mut self.viewers[i];
            let mut lines = Vec::new();
            let mut failed = None;
            match open.viewer.pump() {
                Ok(changed) => dirty |= changed && on_view,
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
            let speculative = open.speculative;
            for line in lines {
                self.debug(|| line);
            }
            // Speculative failures go only to the debug log.
            if speculative && (failed.is_some() || exited.is_some()) {
                let open = &self.viewers[i];
                let key = open.key.clone();
                let error = String::from_utf8_lossy(open.viewer.stderr_tail());
                let last = error.lines().rev().find(|line| !line.trim().is_empty());
                let why = match (failed, last) {
                    (Some(failed), _) => failed,
                    (None, Some(last)) => {
                        format!("exited with {}: {}", exited.unwrap(), last.trim())
                    }
                    (None, None) => format!("exited with {}", exited.unwrap()),
                };
                self.debug(|| format!("prespawn {key} ended: {why}"));
                self.close(i);
                continue;
            }
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

    /// Give the strip's ends priority; show input alerts whole or omit them.
    fn strip(&self, i: usize, width: u16) -> Line<'static> {
        let open = &self.viewers[i];
        let width = width as usize;
        let left = Span::styled(format!("{CONE} cones"), Style::default().fg(ORANGE));
        let name = open
            .viewer
            .title()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                self.data
                    .sessions
                    .iter()
                    .find(|s| s.session_id == open.key)
                    .and_then(|s| s.title.clone())
            })
            .unwrap_or_else(|| open.what.clone());
        let mut middle = vec![
            Span::styled(" · ", dim()),
            Span::styled(name, plain()),
            Span::styled(" · ", dim()),
        ];
        middle.extend(self.data.summary(spinner_frame(self.tick)).spans);
        // Until the launched thread has an id, we cannot exclude its own row from input alerts.
        let has_id = open.record.is_none() || open.recorded;
        let alert = self
            .data
            .sessions
            .iter()
            .filter(|s| has_id && s.state == "blocked" && s.session_id != open.key)
            .max_by_key(|s| s.last_activity)
            .map(|s| {
                let title = s
                    .title
                    .clone()
                    .unwrap_or_else(|| s.session_id.chars().take(8).collect());
                Span::styled(
                    format!(" · {} needs input", clip(&title, 24)),
                    Style::default().fg(Color::Yellow),
                )
            });
        // The strip is the only row a full-frame viewer leaves the dashboard, so an armed
        // quit shows there; otherwise the first ctrl+c would look ignored.
        let mut keys = if self.quitting() {
            // Two spans, so a strip too narrow for the whole hint drops its tail the way it
            // drops a key instead of losing the warning altogether.
            let red = Style::default().fg(Color::Red);
            let cut = QUIT_HINT.find(" · ").unwrap_or(QUIT_HINT.len());
            vec![
                Span::styled(&QUIT_HINT[..cut], red),
                Span::styled(&QUIT_HINT[cut..], red),
            ]
        } else {
            vec![
                Span::styled("tab back", dim()),
                Span::styled(" · ctrl+\\ split", dim()),
            ]
        };
        let ends = |keys: &[Span]| left.width() + keys.iter().map(Span::width).sum::<usize>();
        while !keys.is_empty() && ends(&keys) > width {
            keys.pop();
        }
        let ends = ends(&keys);
        let room = width.saturating_sub(ends);
        let used: usize = middle.iter().map(Span::width).sum();
        if let Some(alert) = alert
            && used + alert.width() <= room
        {
            middle.push(alert);
        }
        let mut middle = fit(middle, room);
        while middle
            .last()
            .is_some_and(|s| matches!(s.content.trim(), "" | "·"))
        {
            middle.pop();
        }
        let used: usize = middle.iter().map(Span::width).sum();
        let mut spans = vec![left];
        spans.extend(middle);
        spans.push(Span::raw(" ".repeat(width.saturating_sub(ends + used))));
        spans.extend(keys);
        Line::from(spans)
    }

    /// Clients without mouse reporting leave wheel scrolling to our emulator.
    fn wants_mouse(&self) -> bool {
        self.split_active() || self.focus.is_some()
    }

    /// VS Code sends an empty bracketed paste for clipboard images. Forward it as ctrl+v
    /// to a viewer, or read the clipboard for the composer.
    fn paste(&mut self, text: &str) {
        if text.is_empty() {
            if let Some(open) = self.focused() {
                open.viewer.write(b"\x16");
            } else if matches!(self.mode, Mode::Normal) {
                self.attach_image();
            }
            return;
        }
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
            // A bracketed paste breaks lines with CR; the composer keeps one kind of break.
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let at = snap(&self.text, self.caret);
            self.text.insert_str(at, &text);
            self.caret = at + text.len();
        }
    }

    fn attach_image(&mut self) {
        match paste_image() {
            Ok(path) => {
                self.caret = attach(&mut self.text, self.caret, self.images.len());
                self.images.push(path);
            }
            Err(e) => self.status = e,
        }
    }

    fn take_prompt(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        let images = std::mem::take(&mut self.images);
        self.caret = 0;
        expand(&text, |n| {
            images
                .get(n)
                .map_or_else(String::new, |p| p.display().to_string())
        })
    }

    fn fill(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.len();
    }

    /// Clamp drags and releases outside the pane so the viewer sees buttons released.
    /// Shift-wheel or clients without mouse reporting scroll the emulator.
    fn mouse(&mut self, ev: MouseEvent) {
        if self.split_active() && !self.click(ev) {
            return;
        }
        let Some(ev) = self.pane_mouse(ev) else {
            return;
        };
        let pane = self.pane;
        let wheel = match ev.kind {
            MouseEventKind::ScrollUp => Some(WHEEL_LINES),
            MouseEventKind::ScrollDown => Some(-WHEEL_LINES),
            _ => None,
        };
        let Some(i) = self.focus.or_else(|| wheel.and(self.shown())) else {
            return;
        };
        let open = &mut self.viewers[i];
        let mode = open.viewer.screen().mouse_protocol_mode();
        if let Some(lines) = wheel
            && (mode == viewer::MouseProtocolMode::None
                || ev.modifiers.contains(KeyModifiers::SHIFT))
        {
            open.viewer.scroll(lines);
            return;
        }
        let bytes = viewer::encode_mouse(ev, (pane.x, pane.y), mode);
        if !bytes.is_empty() {
            open.viewer.write(&bytes);
        }
    }

    fn pane_mouse(&self, mut ev: MouseEvent) -> Option<MouseEvent> {
        let p = self.pane;
        let inside =
            (p.left()..p.right()).contains(&ev.column) && (p.top()..p.bottom()).contains(&ev.row);
        if !inside {
            if !matches!(ev.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
                return None;
            }
            ev.column = ev
                .column
                .clamp(p.left(), p.right().saturating_sub(1).max(p.left()));
            ev.row = ev
                .row
                .clamp(p.top(), p.bottom().saturating_sub(1).max(p.top()));
        }
        Some(ev)
    }

    /// Handle list clicks and focus changes; return whether the viewer should receive the event.
    fn click(&mut self, ev: MouseEvent) -> bool {
        if ev.kind != MouseEventKind::Down(MouseButton::Left) {
            return true;
        }
        let p = self.pane;
        let on_pane =
            (p.left()..p.right()).contains(&ev.column) && (p.top()..p.bottom()).contains(&ev.row);
        if on_pane {
            match self.panel() {
                None => {
                    if self.focus.is_none()
                        && let Some(i) = self.shown()
                    {
                        self.focus(i);
                    }
                    return self.focus.is_some();
                }
                Some(_) if !self.panel_focused() => {
                    self.full = false;
                    let _ = self.enter();
                    return false;
                }
                Some(_) => {}
            }
        }
        if self.focus.is_some() {
            self.unfocus();
        }
        // A click off the pane takes the keys back from a focused panel, as esc would.
        if !on_pane && self.panel_focused() {
            if self.jobs_view {
                self.leave_jobs();
            } else {
                self.mode = Mode::Normal;
            }
        }
        let l = self.list_area;
        if (l.top()..l.bottom()).contains(&ev.row) && ev.column < l.right() {
            let n = self.scroll + (ev.row - l.y) as usize;
            if n < self.visible.len() && self.rows[self.visible[n]].kind.selectable() {
                self.cursor = n;
                if self.rows[self.visible[n]].kind == Kind::Menu {
                    // Match `menu_cells`: the two-column selection mark, then buttons and gaps.
                    let mut x = l.x + 2;
                    for (i, (name, ..)) in MENU.iter().enumerate() {
                        let w = name.chars().count() as u16 + 2;
                        if (x..x + w).contains(&ev.column) {
                            self.menu = i;
                        }
                        x += w + 1;
                    }
                }
            }
        }
        false
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
                // A start that never opens leaves no row, so the log is the only record of why.
                let failed = format!("{} failed: {error:#}", opening.what);
                self.debug(|| failed.clone());
                self.status = failed;
                if self.text.is_empty()
                    && let Some(prompt) = opening.prompt
                {
                    self.fill(prompt);
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
            self.fill(prompt);
        }
        self.status = "opening cancelled".into();
        true
    }

    /// Record the launched thread for later resume; a client closed before its first turn has none.
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

    fn enter_label(&self) -> &'static str {
        let row = self.selected();
        if row
            .and_then(|r| self.viewer_of(&r.kind))
            .is_some_and(|i| !self.viewers[i].speculative)
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
        enter_verb(row.map(|r| &r.kind), self.menu)
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
        self.debug(|| {
            format!(
                "enter on {:?}: {}",
                kind.key(),
                enter_verb(Some(&kind), self.menu)
            )
        });
        if let Some(i) = self.viewer_of(&kind) {
            self.focus(i);
            return Ok(());
        }
        match kind {
            // `spawn` only forks: overlap decides admission inside the child, so the
            // run's own row reports whether it started or was skipped.
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("run {name} requested")),
            Kind::NewJob => self.new_job(),
            // Running headless jobs expose logs; completed runs can resume.
            Kind::Run(id, s) if s == "started" => {
                let mut c = self.me();
                c.args(["__logs", &id, "--follow"]);
                self.open(self.size, c, "logs", format!("run:{id}"), None);
            }
            Kind::Session(id, _) if id.starts_with("starting:") => {
                self.status = "still starting · its row fills in when Claude lists it".into();
            }
            Kind::Session(id, _) => {
                let Some(s) = self.data.sessions.iter().find(|s| s.session_id == id) else {
                    return Ok(());
                };
                let (harness, cwd, own_terminal) =
                    (s.harness.clone(), s.cwd.clone(), s.own_terminal());
                // Interactive clients in other terminals cannot be joined.
                if own_terminal {
                    self.status = format!(
                        "{harness} runs in its own terminal and cannot be joined from here"
                    );
                    return Ok(());
                }
                if harness == "codex" {
                    let key = id.clone();
                    let home = s
                        .transcript_path
                        .as_deref()
                        .and_then(codex::home_of)
                        .map_or_else(|| codex::home(&self.claude), Path::to_path_buf);
                    self.prepare_viewer("codex".into(), key, None, None, move || {
                        harness::codex_resume(&home, &id, &cwd)
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
                c.args(["__attach", &id]);
                self.open(self.size, c, "attach", format!("run:{id}"), None);
            }
            Kind::Menu => self.open_menu(),
            Kind::Folder(dir) => {
                self.status = format!("type an instruction · enter starts a session in {dir}");
            }
            _ => {}
        }
        Ok(())
    }

    fn open_menu(&mut self) {
        match MENU[self.menu].0 {
            "folder" => self.mode = Mode::Folder(Input::default()),
            "jobs" => self.show_jobs(),
            "config" => self.mode = Mode::Config(self.config_form()),
            _ => self.mode = Mode::Guide(0),
        }
    }

    fn install(&mut self, done: &str) {
        let r = self.me().arg("__install").output();
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

    fn harness_at(kind: Option<HarnessKind>) -> usize {
        harness::KNOWN
            .iter()
            .position(|k| Some(*k) == kind)
            .unwrap_or(0)
    }

    fn session_policy(&self) -> config::Policy {
        config::defaults(&self.jobs_path)
    }

    fn start(&mut self) {
        if self.menu_is("jobs") || self.on_new_job() {
            self.new_job();
            return;
        }
        let dir = self.target_dir();
        let kind = harness::KNOWN[self.harness];
        let policy = self.session_policy();
        let prompt = self.take_prompt();
        let what = format!("{kind} in {}", fleet::tilde(&dir));
        // Rollout timestamps are the thread's own clock; a little slack covers it.
        let since = chrono::Utc::now() - chrono::Duration::seconds(5);
        self.debug(|| format!("start {what}: {prompt:?}"));
        // Codex and pi run as the dashboard's own client; only Claude is launched and left.
        if kind != HarnessKind::Claude {
            let record = (kind == HarnessKind::Codex).then(|| (dir.clone(), since));
            let retry = Some(prompt.clone());
            // Use a temporary launch key until the harness reports the session's own id.
            let key = format!("{kind}:start:{}", since.timestamp_millis());
            self.prepare_viewer(what, key, record, retry, move || {
                match harness::start(kind, &dir, prompt.trim(), &policy)? {
                    Start::Foreground(command) => Ok(command),
                    Start::Background(_) => anyhow::bail!("expected a {kind} viewer"),
                }
            });
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.status = format!("starting {what}");
        let id = format!("starting:{}", since.timestamp_millis());
        let session = placeholder(&id, &dir, &prompt);
        self.data.sessions.push(session.clone());
        self.pending.push(Pending {
            session,
            short: None,
            at: Instant::now(),
        });
        self.rebuild();
        self.select_new(&id);
        std::thread::spawn(move || {
            // Capability checks and the command both run off the input thread.
            let result = (|| -> Result<String> {
                let Start::Background(mut command) =
                    harness::start(kind, &dir, prompt.trim(), &policy)?
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

    fn arm(&mut self, key: String) {
        self.armed = Some(key);
        self.armed_at = Instant::now();
    }

    fn expire(&mut self) {
        let mark = self.data.confirm_secs;
        if self.armed.is_some()
            && mark > 0.0
            && self.armed_at.elapsed() >= Duration::from_secs_f64(mark)
        {
            self.armed = None;
            if self.status.starts_with("ctrl+x again") {
                self.status = "kept".into();
            }
        }
        if self
            .quit_armed
            .is_some_and(|at| at.elapsed() >= QUIT_CONFIRM)
        {
            self.quit_armed = None;
            if self.status == QUIT_HINT {
                self.status.clear();
            }
        }
    }

    fn quitting(&self) -> bool {
        self.quit_armed
            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM)
    }

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
                self.arm(name);
            }
        }
    }

    /// Hide the row without removing its ledger record or resumable session.
    fn hide_run(&mut self, id: String) {
        match self.armed.take() {
            Some(armed) if armed == id => {
                self.status = match Ledger::new(&self.state).and_then(|l| l.hide(&id)) {
                    Ok(()) => "run hidden · the ledger still has it".into(),
                    Err(e) => format!("hide failed: {e:#}"),
                };
                self.invalidate();
            }
            _ => {
                self.status = "ctrl+x again to hide this run · any other key keeps it".into();
                self.arm(id);
            }
        }
    }

    fn remove_folder(&mut self, dir: String) {
        match self.armed.take() {
            Some(armed) if armed == dir => {
                self.data.folders.retain(|p| fleet::tilde(p) != dir);
                self.status = match self.save_folders() {
                    Ok(()) => format!("{dir} removed · the folder itself is untouched"),
                    Err(e) => format!("remove failed: {e:#}"),
                };
                self.rebuild();
            }
            _ => {
                self.status = "ctrl+x again to remove this folder · any other key keeps it".into();
                self.arm(dir);
            }
        }
    }

    /// Select a newly pinned empty folder; preserve selection if it already has session rows.
    fn pin_folder(&mut self, dir: PathBuf) -> Result<()> {
        if !self.data.folders.contains(&dir) {
            self.data.folders.push(dir.clone());
            self.save_folders()?;
        }
        self.rebuild();
        self.select_new(&fleet::tilde(&dir));
        Ok(())
    }

    fn pin_selected(&mut self) {
        let dir = self.target_dir();
        let name = fleet::tilde(&dir);
        if self.data.folders.contains(&dir) {
            self.status = format!("{name} is pinned already");
            return;
        }
        self.status = match self.pin_folder(dir) {
            Ok(()) => format!("{name} pinned · its row stays after the last session there leaves"),
            Err(e) => format!("folder not saved: {e:#}"),
        };
    }

    fn save_folders(&self) -> Result<()> {
        Ledger::new(&self.state).and_then(|l| l.write_folders(&self.data.folders))
    }

    fn on_new_job(&self) -> bool {
        matches!(self.selected().map(|r| &r.kind), Some(Kind::NewJob))
    }

    fn show_jobs(&mut self) {
        self.jobs_view = true;
        self.rebuild();
        let first = self
            .visible
            .iter()
            .position(|&i| matches!(self.rows[i].kind, Kind::Job(_) | Kind::NewJob));
        self.cursor = first.unwrap_or(0);
        self.settle();
    }

    fn new_job(&mut self) {
        let (base, fallback) = (self.jobs_dir(), self.target_dir());
        let seed = self.take_prompt();
        self.mode = Mode::Job(Box::new(JobForm::new(
            &base,
            &fallback,
            None,
            &seed,
            &config::defaults(&self.jobs_path),
        )));
    }

    fn edit_job(&mut self) {
        let Some(Kind::Job(name)) = self.selected().map(|r| r.kind.clone()) else {
            self.status = "select a job to edit · the menu's jobs button lists them".into();
            return;
        };
        match config::raw_jobs(&self.jobs_path) {
            Ok(jobs) => match jobs.into_iter().find(|j| j.name == name) {
                Some(j) => {
                    self.mode = Mode::Job(Box::new(JobForm::new(
                        &self.jobs_dir(),
                        &self.cwd,
                        Some(j),
                        "",
                        &config::defaults(&self.jobs_path),
                    )));
                }
                None => {
                    self.status = format!("{name} is not in {}", fleet::tilde(&self.jobs_path));
                }
            },
            Err(e) => self.status = format!("{e:#}"),
        }
    }

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
            Kind::Folder(_) => Some("remove"),
            _ => None,
        }
    }

    fn composer(&self) -> Line<'static> {
        if self.on_button() {
            return Line::default();
        }
        let kind = harness::KNOWN[self.harness].to_string();
        let mut spans = vec![Span::styled(
            format!("{} › ", logo(&kind)),
            brand(&kind).add_modifier(Modifier::BOLD),
        )];
        let label = |n: usize| format!("[Image #{}]", n + 1);
        // The composer is one line; a break shows as its glyph and stays a break in the prompt.
        let show = |t: &str| expand(t, label).replace('\n', "⏎");
        let shown = show(&self.text);
        let caret = show(&self.text[..snap(&self.text, self.caret)]).len();
        spans.extend(typed(&shown, caret, "Type an instruction…"));
        Line::from(spans)
    }

    fn hint_line(&self) -> Line<'static> {
        if !self.status.is_empty() {
            let style = if self.quitting() {
                Style::default().fg(Color::Red)
            } else {
                dim()
            };
            return Line::styled(self.status.clone(), style);
        }
        if let Some(action) = self
            .stopping
            .iter()
            .find(|a| self.selected().and_then(|r| r.kind.key()) == Some(a.id.as_str()))
        {
            return Line::styled(action.message(), dim());
        }
        let prefix = (!self.filter.text.is_empty())
            .then(|| Span::styled(format!("filter: {}  ", self.filter.text), dim()));
        let mut line = if self.focus.is_some() {
            hints(&[("tab", "back"), ("ctrl+\\", "full screen")])
        } else {
            self.mode_hints(prefix.as_ref().map_or(0, Span::width))
        };
        if let Some(prefix) = prefix {
            line.spans.insert(0, prefix);
        }
        line
    }

    /// Drop global hints from the end until they fit; keep the selected row's action and exit key.
    fn mode_hints(&self, taken: usize) -> Line<'static> {
        let start = if self.menu_is("jobs") || self.on_new_job() {
            "new job with it".to_owned()
        } else {
            format!(
                "start {} in {}",
                harness::KNOWN[self.harness],
                fleet::tilde(&self.target_dir())
            )
        };
        match &self.mode {
            Mode::Filter => hints(&[("enter", "keep the filter"), ("esc", "clear it")]),
            Mode::Job(form) if form.row == JobRow::Head => {
                let mut keys = vec![("↑ ↓", "field"), ("enter →", "open")];
                if !form.shut {
                    keys.push(("←", "shut"));
                }
                keys.push(("esc", "cancel"));
                hints(&keys)
            }
            Mode::Job(form) => {
                let mut keys = vec![];
                if form.turns() {
                    keys.push(("← →", "change"));
                }
                keys.push(("enter", form.enter_does()));
                keys.push(("↑ ↓", "field"));
                if form.row == JobRow::Ask(Step::Where) {
                    keys.push(("tab", "complete"));
                }
                keys.push(("esc", "cancel"));
                hints(&keys)
            }
            Mode::Config(form) if form.open => hints(&[("enter", "keep"), ("esc", "back")]),
            Mode::Config(form) if form.head() => {
                hints(&[("↑", "field"), ("enter →", "open"), ("esc", "done")])
            }
            Mode::Config(form) => {
                let f = form.field();
                let mut keys = vec![("↑ ↓", "field")];
                if matches!(f.input, Answer::Columns) {
                    let a = &form.arrange;
                    keys.push(("← →", "column"));
                    keys.push(("space", if a.at < a.shown { "hide" } else { "show" }));
                    keys.push(("[ ]", "move"));
                } else if f.picks().is_some() {
                    keys.push(("← →", "change"));
                } else if f.step().is_some() {
                    keys.push(("← →", "step"));
                }
                if f.typed() {
                    keys.push(("enter", "type"));
                }
                if !form.values[form.row].is_empty() {
                    keys.push(("bksp", "reset"));
                }
                keys.push(("esc", "done"));
                hints(&keys)
            }
            Mode::Guide(_) => hints(&[("↑ ↓", "scroll"), ("esc", "back")]),
            Mode::Folder(_) => hints(&[
                ("enter", "add"),
                ("tab", "complete"),
                ("↑ ↓", "recent"),
                ("esc", "cancel"),
            ]),
            Mode::Rename(_) => hints(&[("enter", "rename"), ("esc", "cancel")]),
            Mode::Normal if !self.text.is_empty() => {
                hints(&[("enter", &start), ("shift+tab", "harness")])
            }
            Mode::Normal => {
                let mut keys = vec![];
                if self.selected().is_some() {
                    keys.push(("enter", self.enter_label()));
                }
                if self.menu_is(MENU[self.menu].0) {
                    keys.push(("← →", "pick"));
                }
                if let Some(verb) = self.stop_verb() {
                    keys.push(("ctrl+x", verb));
                }
                if let Some(Kind::Job(_)) = self.selected().map(|r| &r.kind) {
                    keys.push(("ctrl+e", "edit"));
                }
                if self.shown().is_some() || self.panel_shown() {
                    keys.push(("tab", "pane"));
                }
                keys.push(("shift+tab", "harness"));
                keys.push(("esc", if self.jobs_view { "back" } else { "quit" }));
                let room = (self.hint_width() as usize).saturating_sub(taken);
                let mut line = hints(&keys);
                while keys.len() > 2 && line.width() > room {
                    keys.remove(keys.len() - 2);
                    line = hints(&keys);
                }
                line
            }
        }
    }

    fn hint_width(&self) -> u16 {
        if self.split_active() {
            self.split_areas(self.frame())[0].width
        } else {
            self.size.1
        }
    }

    fn stop(&mut self) {
        let id = match self.selected().map(|r| r.kind.clone()) {
            Some(Kind::Run(id, s)) if s != "started" => return self.hide_run(id),
            Some(Kind::Folder(dir)) => return self.remove_folder(dir),
            Some(Kind::Session(id, _)) if id.starts_with("starting:") => {
                self.status = "still starting · nothing to stop yet".into();
                return;
            }
            Some(Kind::Session(id, _) | Kind::Run(id, _)) => id,
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
        // Forgetting a thread or removing a background job preserves its resumable conversation.
        let verb = self.session_verb(&id);
        if let Some(action) = self.stopping.iter().find(|a| a.id == id) {
            self.status = action.message();
            self.armed = None;
            return;
        }
        match self.armed.take() {
            Some(armed) if armed == id => {
                for key in [id.clone(), format!("run:{id}")] {
                    if let Some(i) = self.viewer_index(&key) {
                        self.close(i);
                    }
                }
                let (state, claude, target) = (self.state.clone(), self.claude.clone(), id.clone());
                // Terminate the attached client to release the daemon-held thread; it remains resumable.
                // No registry lists a Codex client, so signal its pid rather than looking it up.
                let client = self
                    .data
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .and_then(|s| s.pid);
                self.queue_stop(id, verb, move || {
                    if verb == "forget" {
                        codex::forget(&state, &target)?;
                        if let Some(pid) = client {
                            fleet::terminate(pid, "codex")?;
                        }
                        Ok(true)
                    } else {
                        Ledger::new(&state).and_then(|l| runner::stop(&l, &claude, &target))
                    }
                });
            }
            _ => {
                self.arm(id);
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
                // The row leaving the list says it; the hint line goes back to the keys.
                Ok(true) if matches!(action.verb, "delete" | "forget") => {
                    self.removed_sessions.insert(action.id.clone());
                    self.data.sessions.retain(|s| s.session_id != action.id);
                    String::new()
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

    /// A ctrl+c press: true when it is the second within the confirmation window.
    fn quit_press(&mut self) -> bool {
        if self
            .quit_armed
            .replace(Instant::now())
            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM)
        {
            return true;
        }
        self.status = QUIT_HINT.into();
        false
    }

    /// Route input to the active mode or viewer; return true to quit the dashboard.
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        if let Some(open) = self.focused() {
            if ctrl && code == KeyCode::Char('z') {
                self.unfocus();
                return Ok(false);
            }
            // Plain tab returns to the list; shift+tab remains the client's mode switch.
            if code == KeyCode::Tab && mods.is_empty() {
                self.unfocus();
                return Ok(false);
            }
            // Left at an empty composer has nowhere to go in the client, so it returns
            // to the list the way tab does.
            if code == KeyCode::Left
                && mods.is_empty()
                && viewer::at_empty_prompt(open.viewer.screen())
            {
                self.unfocus();
                return Ok(false);
            }
            // ctrl+\ arrives as the byte 0x1c, which crossterm reports as ctrl+4.
            if ctrl && matches!(code, KeyCode::Char('\\' | '4')) {
                self.toggle_split();
                return Ok(false);
            }
            // ctrl+c never reaches the client: Claude Code, Codex and pi all quit on two of
            // them, and Claude Code's first one drops to the agents list. It is the
            // dashboard's quit key here as it is from the list; esc interrupts the client.
            if ctrl && code == KeyCode::Char('c') {
                return Ok(self.quit_press());
            }
            if mods.contains(KeyModifiers::SHIFT)
                && matches!(code, KeyCode::PageUp | KeyCode::PageDown)
            {
                let page = i32::from(open.viewer.screen().size().0.saturating_sub(1));
                open.viewer
                    .scroll(if code == KeyCode::PageUp { page } else { -page });
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
        if self.full && !self.pane_focused() && self.opening.is_none() {
            self.full = false;
        }
        // Forms keep tab for completion or editing; otherwise it returns to the list.
        let tab_out = code == KeyCode::Tab
            && mods.is_empty()
            && match &self.mode {
                Mode::Config(form) => !form.open,
                Mode::Guide(_) => true,
                _ => false,
            };
        if (tab_out || (ctrl && code == KeyCode::Char('z'))) && self.panel_focused() {
            if matches!(self.mode, Mode::Normal) {
                self.leave_jobs();
            } else {
                self.mode = Mode::Normal;
                self.status = "back to the list".into();
            }
            self.needs_clear = true;
            return Ok(false);
        }
        if ctrl && matches!(code, KeyCode::Char('\\' | '4')) && self.panel_focused() {
            self.toggle_split();
            return Ok(false);
        }
        match &mut self.mode {
            Mode::Filter => {
                match code {
                    KeyCode::Esc => {
                        self.filter = Input::default();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Enter => self.mode = Mode::Normal,
                    _ => {
                        self.filter.key(code, mods);
                    }
                }
                self.apply_filter();
                self.settle();
            }
            Mode::Guide(top) => {
                let top = *top;
                match code {
                    KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Char('g') if ctrl => self.mode = Mode::Normal,
                    KeyCode::Up => self.mode = Mode::Guide(top.saturating_sub(1)),
                    // Scroll is clamped to entry count, not wrapped line count.
                    KeyCode::Down => self.mode = Mode::Guide((top + 1).min(GUIDE.len() - 1)),
                    _ => {}
                }
            }
            Mode::Folder(input) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up | KeyCode::Down if !self.data.recent.is_empty() => {
                    let recent: Vec<String> =
                        self.data.recent.iter().map(|p| fleet::tilde(p)).collect();
                    let at = recent.iter().position(|r| *r == input.text);
                    let n = recent.len();
                    let next = match (code, at) {
                        (KeyCode::Up, None) => 0,
                        (KeyCode::Up, Some(i)) => (i + 1) % n,
                        (_, None) => n - 1,
                        (_, Some(i)) => (i + n - 1) % n,
                    };
                    *input = Input::new(recent[next].clone());
                }
                KeyCode::Tab => self.status = input.complete(&self.cwd).join("  "),
                KeyCode::Enter => {
                    let text = input.text.clone();
                    match launch_dir(&text, &self.cwd, &self.cwd) {
                        Ok(dir) => {
                            self.mode = Mode::Normal;
                            let name = fleet::tilde(&dir);
                            self.status = match self.pin_folder(dir) {
                                Ok(()) => format!(
                                    "{name} added · type an instruction and enter starts a session there"
                                ),
                                Err(e) => format!("folder not saved: {e:#}"),
                            };
                        }
                        Err(e) => self.status = e,
                    }
                }
                _ => {
                    input.key(code, mods);
                }
            },
            Mode::Rename(input) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let name = input.text.trim().to_owned();
                    let Some(session) = self.selected_session() else {
                        self.mode = Mode::Normal;
                        return Ok(false);
                    };
                    self.status = match fleet::rename(session, &name) {
                        Ok(()) => format!("renamed to {name}"),
                        Err(e) => format!("not renamed: {e:#}"),
                    };
                    self.mode = Mode::Normal;
                    self.invalidate();
                }
                _ => {
                    input.key(code, mods);
                }
            },
            Mode::Job(form) if code == KeyCode::Tab && form.row == JobRow::Ask(Step::Where) => {
                self.status = form.complete().join("  ");
            }
            Mode::Job(form) => match form.key(code, mods) {
                FormAction::Stay => {}
                FormAction::Cancel => self.mode = Mode::Normal,
                FormAction::RunOnce(prompt, dir) => {
                    self.mode = Mode::Normal;
                    let what = format!("started a run in {}", fleet::tilde(&dir));
                    self.spawn(&["run", "--prompt", &prompt], Some(&dir), &what);
                }
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
            Mode::Config(form) => match form.key(code, mods) {
                ConfigAction::Stay => {}
                ConfigAction::Cancel => {
                    self.mode = Mode::Normal;
                    self.select_first_session();
                }
                ConfigAction::Save(policy, columns, spark, pane, start, mark, whole) => {
                    self.data.columns = if columns.is_empty() {
                        built_columns()
                    } else {
                        columns.clone()
                    };
                    self.data.whole_columns = whole.unwrap_or(config::WHOLE_COLUMNS);
                    self.rebuild();
                    match config::write_config(
                        &self.jobs_path,
                        &policy,
                        Some(&columns),
                        spark.as_ref(),
                        pane.as_ref(),
                        start.as_ref(),
                        mark,
                        whole,
                    ) {
                        Ok(()) => {
                            self.status =
                                format!("config saved to {}", fleet::tilde(&self.jobs_path));
                            self.invalidate();
                        }
                        Err(e) => {
                            if let Mode::Config(form) = &mut self.mode {
                                form.error = Some(format!("{e:#}"));
                            }
                        }
                    }
                }
            },
            Mode::Normal => {
                let armed = self.armed.take();
                if self.on_button() && matches!(code, KeyCode::Left | KeyCode::Right) {
                    let n = MENU.len();
                    self.menu = (self.menu + if code == KeyCode::Right { 1 } else { n - 1 }) % n;
                    return Ok(false);
                }
                // Right on a row with nothing typed reaches for the pane: open it, then focus it.
                if !self.on_button()
                    && code == KeyCode::Right
                    && mods.is_empty()
                    && self.text.is_empty()
                {
                    match self.shown() {
                        _ if !self.split => self.toggle_split(),
                        Some(i) => self.focus(i),
                        None if self.panel_shown() => self.open_menu(),
                        None => self.status = "nothing in the pane".into(),
                    }
                    return Ok(false);
                }
                if !self.on_button()
                    && let Some(at) = edit(&mut self.text, self.caret, code, mods)
                {
                    self.caret = at;
                    return Ok(false);
                }
                match code {
                    KeyCode::Char('c') if ctrl => {
                        if self.quit_press() {
                            return Ok(true);
                        }
                    }
                    KeyCode::Char('x') if ctrl => {
                        self.armed = armed;
                        self.stop();
                    }
                    KeyCode::Esc => {
                        if armed.is_some() {
                            self.status = "kept".into();
                        } else if !self.text.is_empty() {
                            self.text.clear();
                            self.images.clear();
                        } else if self.jobs_view {
                            self.leave_jobs();
                        } else {
                            return Ok(true);
                        }
                    }
                    KeyCode::Up => self.step(-1),
                    KeyCode::Down => self.step(1),
                    KeyCode::Tab => match self.shown() {
                        Some(i) => self.focus(i),
                        None if self.jobs_view => self.leave_jobs(),
                        None if self.panel_shown() => self.open_menu(),
                        None => self.status = "nothing in the pane".into(),
                    },
                    KeyCode::BackTab => {
                        self.harness = (self.harness + 1) % harness::KNOWN.len();
                    }
                    // Terminals may encode shift+enter as ESC CR, which crossterm reports as alt+enter.
                    KeyCode::Enter
                        if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && self.text.trim().is_empty() =>
                    {
                        // Set this before asynchronous viewer startup; it takes effect only while focused.
                        self.full = true;
                        self.enter()?;
                    }
                    KeyCode::Enter if self.text.trim().is_empty() => {
                        self.full = false;
                        self.enter()?;
                    }
                    // With something typed, the same chord breaks the line instead of launching.
                    KeyCode::Enter if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                        let at = snap(&self.text, self.caret);
                        self.text.insert(at, '\n');
                        self.caret = at + 1;
                    }
                    KeyCode::Enter => self.start(),
                    KeyCode::Char('s') if ctrl => {
                        self.by_state = !self.by_state;
                        self.rebuild();
                    }
                    KeyCode::Char('p') if ctrl => self.pin_selected(),
                    KeyCode::Char('\\' | '4') if ctrl => self.toggle_split(),
                    KeyCode::Char('e') if ctrl => self.edit_job(),
                    KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
                    KeyCode::Char('g') if ctrl => self.mode = Mode::Guide(0),
                    KeyCode::Char('n') if ctrl => self.rename_selected(),
                    KeyCode::Char('r') if ctrl => {
                        self.invalidate();
                        self.status = "refresh requested".into();
                    }
                    KeyCode::Char('v') if ctrl => self.attach_image(),
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.size = (area.height, area.width);
        self.pane = self.pane(area);
        if self.split_active() {
            let [list, rule, pane] = self.split_areas(area);
            self.draw_dashboard(frame, list);
            let style = if self.pane_focused() {
                Style::default().fg(ORANGE)
            } else {
                dim()
            };
            let symbol = if rule.width == 1 { "│" } else { "─" };
            let buf = frame.buffer_mut();
            for y in rule.top()..rule.bottom() {
                for x in rule.left()..rule.right() {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_symbol(symbol);
                        cell.set_style(style);
                    }
                }
            }
            if let Some(name) = self.panel() {
                self.draw_panel(frame, name, pane);
                return;
            }
            let inner = self.pane;
            match self.shown() {
                Some(i) if self.viewers[i].viewer.first_paint().is_some() => {
                    self.draw_viewer(frame, i, inner)
                }
                Some(i) => self.viewers[i].viewer.resize(inner.height, inner.width),
                None => {}
            }
            return;
        }
        if let Some(i) = self.focus {
            if area.height >= 2 {
                let strip = Rect {
                    y: area.bottom() - 1,
                    height: 1,
                    ..area
                };
                frame.render_widget(Paragraph::new(self.strip(i, area.width)), strip);
            }
            let pane = self.pane;
            self.draw_viewer(frame, i, pane);
            return;
        }
        self.draw_dashboard(frame, area);
    }

    /// Draw the emulator cursor only when focused and at the live scroll position.
    fn draw_viewer(&mut self, frame: &mut Frame, i: usize, pane: Rect) {
        let focused = self.focus == Some(i);
        let open = &mut self.viewers[i];
        open.viewer.resize(pane.height, pane.width);
        let screen = open.viewer.screen();
        viewer::render(screen, pane, frame.buffer_mut());
        if focused && !screen.hide_cursor() && screen.scrollback() == 0 {
            let (row, mut col) = screen.cursor_position();
            col = col.min(pane.width.saturating_sub(1));
            if col > 0
                && screen
                    .cell(row, col)
                    .is_some_and(|c| c.is_wide_continuation())
            {
                col -= 1;
            }
            if row < pane.height {
                frame.set_cursor_position((pane.x + col, pane.y + row));
            }
        }
    }

    fn mode_line(&self) -> Line<'static> {
        match &self.mode {
            Mode::Filter => {
                let mut spans = vec![Span::styled("/ ", bold())];
                spans.extend(self.filter.spans("text a row must contain"));
                Line::from(spans)
            }
            Mode::Job(f) => f.line(),
            Mode::Config(f) => f.line(),
            Mode::Folder(input) => {
                let mut spans = vec![Span::styled("folder › ", Style::default().fg(ORANGE))];
                spans.extend(input.spans(&fleet::tilde(&self.cwd)));
                Line::from(spans)
            }
            Mode::Rename(input) => {
                let mut spans = vec![Span::styled("rename › ", Style::default().fg(ORANGE))];
                spans.extend(input.spans("a title for the session"));
                Line::from(spans)
            }
            Mode::Guide(_) => Line::from(vec![
                Span::styled("guide › ", Style::default().fg(ORANGE)),
                Span::styled("the keys and what they do", dim()),
            ]),
            Mode::Normal => self.composer(),
        }
    }

    fn framed(&self, line: Line<'static>, width: u16) -> (Paragraph<'static>, u16) {
        let rules = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(if self.quitting() {
                Style::default().fg(Color::Red)
            } else {
                dim()
            });
        let input = Paragraph::new(line).wrap(Wrap { trim: false }).block(rules);
        // line_count already counts the two rules, so this is the whole framed box.
        let rows = input.line_count(width).clamp(3, 10) as u16;
        (input, rows)
    }

    fn draw_panel(&mut self, frame: &mut Frame, name: &str, pane: Rect) {
        // A column of air beside the vertical rule, so the text is not against it.
        let pane = if self.data.pane.at == "bottom" {
            pane
        } else {
            Rect {
                x: pane.x + 1,
                width: pane.width.saturating_sub(1),
                ..pane
            }
        };
        let (_, verb, what) = MENU.iter().find(|(n, ..)| *n == name).unwrap_or(&MENU[0]);
        let line = if self.panel_focused() {
            self.mode_line()
        } else {
            Line::from(vec![
                Span::styled(format!("{verb} › "), Style::default().fg(ORANGE)),
                Span::styled(*what, dim()),
            ])
        };
        let (prompt, rows) = self.framed(line, pane.width);
        // Reserve matching hint rows so the pane and composer prompts align.
        let [body, foot, hint] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(rows),
            Constraint::Length(1),
        ])
        .areas(pane);
        match (&self.mode, name) {
            (Mode::Job(form), _) => frame.render_widget(form.paragraph(body), body),
            (Mode::Config(form), _) => frame.render_widget(form.paragraph(body), body),
            (Mode::Guide(top), _) => frame.render_widget(guide(*top, body.width), body),
            (_, "help") => frame.render_widget(guide(0, body.width), body),
            // Config previews reread jobs.yaml every frame. Cache the form in rebuild
            // if profiling shows this cost.
            (_, "config") => frame.render_widget(self.config_form().paragraph(body), body),
            (_, "jobs") if self.jobs_view => self.draw_list(frame, body),
            (_, "jobs") => {
                let all: Vec<usize> = (0..self.other.len()).collect();
                let lines =
                    self.row_lines(&self.other, &all, None, 0, body.height as usize, body.width);
                frame.render_widget(Paragraph::new(lines), body);
            }
            _ => frame.render_widget(Paragraph::new(self.recent_lines()), body),
        }
        frame.render_widget(prompt, foot);
        if self.panel_focused() {
            frame.render_widget(Paragraph::new(self.hint_line()), hint);
        }
    }

    fn recent_lines(&self) -> Vec<Line<'static>> {
        let held = match &self.mode {
            Mode::Folder(input) => input.text.as_str(),
            _ => "",
        };
        let mut lines = vec![
            Line::default(),
            Line::from(Span::styled("recent folders", Style::default().fg(ORANGE))),
        ];
        lines.extend(self.data.recent.iter().map(|p| {
            let name = fleet::tilde(p);
            if name == held {
                Line::from(vec![
                    Span::styled("▌ ", Style::default().fg(ORANGE)),
                    Span::styled(name, bold()),
                ])
            } else {
                Line::from(vec![Span::raw("  "), Span::styled(name, dim())])
            }
        }));
        lines
    }

    /// Align the composer's lower rule with the harness's live input, even while viewing history.
    /// Without a visible rule, reserve only the hint row.
    fn foot_rows(&self) -> u16 {
        if self.data.pane.at == "bottom" {
            return 1;
        }
        let Some(i) = self.shown() else { return 1 };
        let screen = self.viewers[i].viewer.screen();
        let (rows, cols) = screen.size();
        let ruled = |y: u16| {
            (0..cols)
                .filter(|&x| {
                    screen
                        .cell_unscrolled(y, x)
                        .is_some_and(|c| c.contents() == "\u{2500}")
                })
                .count() as u16
                * 2
                > cols
        };
        (0..rows)
            .rev()
            .find(|&y| ruled(y))
            .map_or(1, |y| rows - 1 - y)
            .clamp(1, 6)
    }

    fn draw_dashboard(&mut self, frame: &mut Frame, area: Rect) {
        let in_pane = self.split_active() && self.panel().is_some();
        let mut line = if in_pane {
            self.composer()
        } else {
            self.mode_line()
        };
        // Hide the composer cursor while the pane has focus.
        if self.focus.is_some() || (in_pane && self.panel_focused()) {
            for span in &mut line.spans {
                span.style = span.style.remove_modifier(Modifier::REVERSED);
            }
        }
        let (input, rows) = self.framed(line, area.width);
        let [head, list, prompt, foot] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(rows),
            Constraint::Length(self.foot_rows()),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(header_lines(self.header_summary(), head.width as usize)),
            head,
        );
        if in_pane && self.jobs_view {
            let all: Vec<usize> = (0..self.other.len()).collect();
            let lines =
                self.row_lines(&self.other, &all, None, 0, list.height as usize, list.width);
            frame.render_widget(Paragraph::new(lines), list);
        } else if in_pane {
            self.draw_list(frame, list);
        } else if let Mode::Guide(top) = self.mode {
            frame.render_widget(guide(top, list.width), list);
        } else if let Mode::Job(form) = &self.mode {
            frame.render_widget(form.paragraph(list), list);
        } else if let Mode::Config(form) = &self.mode {
            frame.render_widget(form.paragraph(list), list);
        } else {
            self.draw_list(frame, list);
        }
        frame.render_widget(input, prompt);
        // A focused viewer's keys go in the list's own hint row: the pane's last row is the
        // harness's status line, and drawing over it hid the permission mode it ends with.
        if !(self.split_active() && self.panel_focused()) {
            frame.render_widget(Paragraph::new(self.hint_line()), foot);
        }
    }

    fn menu_is(&self, name: &str) -> bool {
        self.on_menu() && MENU[self.menu].0 == name
    }

    fn on_menu(&self) -> bool {
        matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu))
    }

    /// The list is sitting on a button, which takes no instruction.
    fn on_button(&self) -> bool {
        self.on_menu() && self.text.is_empty()
    }

    fn menu_cells(&self, selected: bool) -> Vec<(String, Style)> {
        let mut cells = vec![];
        for (i, (name, ..)) in MENU.iter().enumerate() {
            let style = if selected && i == self.menu {
                pressed()
            } else {
                button()
            };
            cells.push((format!(" {name} "), style));
            cells.push((" ".to_owned(), Style::default()));
        }
        if selected {
            cells.push((format!(" {}", MENU[self.menu].2), dim()));
        }
        cells
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        self.list_area = area;
        let height = area.height as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if height > 0 && self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        let lines = self.row_lines(
            &self.rows,
            &self.visible,
            Some(self.cursor),
            self.scroll,
            height,
            area.width,
        );
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// A missing cursor selects only the menu row, as used beside the jobs pane.
    #[allow(clippy::too_many_arguments)]
    fn row_lines(
        &self,
        rows: &[Row],
        visible: &[usize],
        cursor: Option<usize>,
        scroll: usize,
        height: usize,
        width: u16,
    ) -> Vec<Line<'static>> {
        visible
            .iter()
            .enumerate()
            .skip(scroll)
            .take(height)
            .map(|(n, &i)| {
                let row = &rows[i];
                let selected = cursor.map_or(row.kind == Kind::Menu, |c| c == n);
                let armed = self
                    .armed
                    .as_deref()
                    .is_some_and(|a| row.kind.key() == Some(a));
                let mut spans = Vec::with_capacity(row.cells.len() + 1);
                let mut mark = 0;
                if row.kind.selectable() {
                    spans.push(Span::styled(
                        if selected { "▌ " } else { "  " },
                        Style::default().fg(if armed { Color::Red } else { ORANGE }),
                    ));
                    mark = 2;
                }
                let menu;
                let cells = if row.kind == Kind::Menu {
                    menu = self.menu_cells(selected);
                    &menu
                } else {
                    &row.cells
                };
                let mut drawn = Vec::with_capacity(cells.len());
                for (c, (text, style)) in cells.iter().enumerate() {
                    let (text, style) = if c == 0 && row.working() {
                        (
                            text.replacen('▁', SPINNER[spinner_frame(self.tick)], 1),
                            *style,
                        )
                    } else {
                        (text.clone(), *style)
                    };
                    drawn.push(Span::styled(
                        text,
                        if armed { style.fg(Color::Red) } else { style },
                    ));
                }
                // Only a table has columns; a menu or hint row keeps every cell it has.
                let tabular = matches!(
                    row.kind,
                    Kind::Session(..) | Kind::Job(_) | Kind::Run(..) | Kind::Columns
                );
                spans.extend(if self.data.whole_columns && tabular {
                    whole_cells(
                        drawn,
                        (width as usize).saturating_sub(mark),
                        named_cell(rows, i),
                    )
                } else {
                    drawn
                });
                let line = Line::from(spans);
                if selected { line.style(bold()) } else { line }
            })
            .collect()
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
    // Ctrl+Z changes dashboard focus instead of suspending it. Viewer children restore
    // their default signal handlers in `pre_exec`.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let signalled = quit_on_signals()?;
    SHELL_TTY.get_or_init(|| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(0, &mut t) == 0).then_some(t)
    });
    let mut terminal = ratatui::init();
    // Ratatui restores raw mode; also disable the reporting modes cones enabled.
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
            // A closed terminal or a `kill` leaves through the same teardown as `q`, so the shell
            // gets its line discipline back and the viewers are reaped rather than hung up on.
            if signalled.load(Ordering::Relaxed) {
                return Ok(());
            }
            app.tick = (animation.elapsed().as_millis() / 100) as usize;
            if app.refreshed.elapsed() >= Duration::from_secs(1) {
                app.reload();
            }
            app.poll();
            let dirty = app.pump();
            app.prespawn_tick();
            app.expire();
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
                std::io::stdout().sync_update(|_| -> std::io::Result<()> {
                    if app.needs_clear {
                        // Avoid `Terminal::clear`: its cursor query fails on terminals that do not answer.
                        // Clear the backend and forget the previous buffer instead.
                        terminal.backend_mut().clear()?;
                        terminal.swap_buffers();
                        app.needs_clear = false;
                    }
                    terminal.draw(|f| app.draw(f))?;
                    Ok(())
                })??;
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
            // Poll between animation frames for responsive input and reloads without idle repaints.
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
    // Hand the terminal back before reaping. Viewers draw on their own ptys, so a slow
    // reap has nothing left to say to this screen, and quitting feels immediate.
    ratatui::restore();
    hand_back_tty();
    // Viewers die with the dashboard: their process groups, never the agents behind them.
    let reaping = Instant::now();
    app.viewers.clear();
    app.timing("reap", reaping);
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
    #[test]
    fn the_shut_section_takes_no_value_until_enter_opens_it() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let none = KeyModifiers::NONE;
        let head = FIELDS.iter().position(|f| f.group == SHUT).unwrap();
        for _ in 0..FIELDS.len() {
            c.key(KeyCode::Down, none);
        }
        assert_eq!(c.row, head, "walking down stops on the shut head");
        for code in [KeyCode::Left, KeyCode::Backspace, KeyCode::Char('t')] {
            c.key(code, none);
        }
        assert!(
            c.values.iter().all(|v| v.is_empty()) && !c.open,
            "the head answers no key that changes a value"
        );
        c.key(KeyCode::Up, none);
        assert_eq!(c.row, head - 1, "up leaves the head for the row above");
        c.key(KeyCode::Down, none);
        c.key(KeyCode::Right, none);
        assert_eq!(c.row, head, "→ opens the section on its first field");
        c.key(KeyCode::Down, none);
        assert_eq!(c.row, head + 1, "and the walk goes on through it");
        let mark = |c: &ConfigForm| {
            c.lines(120)
                .0
                .iter()
                .map(ToString::to_string)
                .find(|l| l.starts_with(SHUT))
                .expect("the section has a head")
        };
        assert!(mark(&c).contains('▾'), "an open head says so: {}", mark(&c));
        for _ in 0..2 {
            c.key(KeyCode::Up, none);
        }
        assert!(
            c.head() && !c.shut,
            "walking back up stops on the open head"
        );
        c.key(KeyCode::Left, none);
        assert!(
            c.shut && c.head(),
            "← shuts the section and stays on its head"
        );
        assert!(mark(&c).contains('▸'), "a shut head says so: {}", mark(&c));
        c.key(KeyCode::Down, none);
        assert!(c.head(), "and the shut head keeps the walk again");

        let p = config::Policy {
            env: Some(vec!["FOO".to_owned(), "BAR".to_owned()]),
            archive_transcript: Some(true),
            ..Default::default()
        };
        let c = ConfigForm::new(&p, None, None, None, None, None, None);
        assert_eq!(c.values[field_at("env")], "FOO, BAR");
        assert_eq!(c.values[field_at("archive_transcript")], "true");
        let saved = c.config().unwrap().0;
        assert_eq!(
            (saved.env, saved.archive_transcript),
            (p.env.clone(), p.archive_transcript),
            "a run field the file names comes back from its row unchanged"
        );

        let mut c = ConfigForm::new(&p, None, None, None, None, None, None);
        c.go(field_at("env"));
        for bad in ["A: B", "1FOO", "PATH"] {
            c.values[c.row] = bad.to_owned();
            let e = c.config().unwrap_err();
            assert!(
                e.starts_with("env:"),
                "{bad} is refused on the row, before the line is written: {e}"
            );
        }
    }

    #[test]
    fn arrows_step_a_number_field_on_its_own_grid() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let none = KeyModifiers::NONE;
        let value = |c: &ConfigForm| c.values[c.row].clone();

        c.go(field_at("confirm_secs"));
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "3");
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "4");
        for _ in 0..2 {
            c.key(KeyCode::Left, none);
        }
        assert_eq!(
            value(&c),
            "2",
            "the grid keeps the step's precision, not the float's"
        );
        c.key(KeyCode::Backspace, none);
        assert!(
            value(&c).is_empty(),
            "backspace is the way back to the built-in"
        );

        c.go(field_at("timeout_min"));
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "35");
        for _ in 0..8 {
            c.key(KeyCode::Left, none);
        }
        assert_eq!(value(&c), "0", "a step never goes below zero");

        c.go(field_at("overlap"));
        for want in ["allow", "replace", ""] {
            c.key(KeyCode::Right, none);
            assert_eq!(value(&c), want, "the built-in is one stop, not two");
        }
        c.go(field_at("model"));
        c.values[c.row] = "claude-opus-5".to_owned();
        c.key(KeyCode::Right, none);
        assert!(value(&c).is_empty(), "past the typed value is the built-in");
        c.key(KeyCode::Left, none);
        assert!(
            c.open && value(&c).is_empty(),
            "back is the slot, empty and typed into"
        );
        c.key(KeyCode::Left, none);
        assert!(!c.open, "and past the slot the words again");
        assert_eq!(value(&c), "haiku");
        c.key(KeyCode::Enter, none);
        assert_eq!(
            c.row,
            field_at("model") + 1,
            "enter on a word goes to the next setting"
        );

        c.go(field_at("columns"));
        assert!(
            matches!(c.key(KeyCode::Right, none), ConfigAction::Stay),
            "moving the cursor along the row writes nothing"
        );
        let shown = match c.key(KeyCode::Char(' '), none) {
            ConfigAction::Save(_, cols, ..) => cols,
            other => panic!("space on the columns row writes the line, got {other:?}"),
        };
        assert_eq!(
            shown.len(),
            config::DEFAULT_COLUMNS.len() - 1,
            "the column under the cursor left the set"
        );
        assert_eq!(value(&c), shown.join(", "), "and the row reads the line");
        assert!(
            matches!(c.key(KeyCode::Backspace, none), ConfigAction::Save(_, c, ..) if c.is_empty()),
            "backspace leaves the line out, so the built-in set applies"
        );

        let span = |t: &str| Span::raw(t.to_owned());
        let wide = flow(vec![span("ab"), span("cd"), span("ef")], 2, 4);
        assert_eq!(
            wide.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["abcd", "  ef"],
            "a control too wide hangs under the column it started in"
        );
        assert_eq!(
            flow(vec![span("abcdef"), span("gh")], 2, 4).len(),
            2,
            "the first span keeps one span beside it whatever the width"
        );
    }

    #[test]
    fn a_wrapped_answer_hangs_under_its_label() {
        let hung = hang(
            vec![
                Span::styled("  what  ".to_owned(), bold()),
                Span::raw("one two three four".to_owned()),
            ],
            8,
            22,
        );
        assert_eq!(
            hung.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["  what  one two three ", "        four"],
            "the tail starts on the label's column"
        );
        assert_eq!(
            hang(vec![Span::raw("a".repeat(60))], 8, 28).len(),
            3,
            "a word wider than the line breaks at the edge instead of running off it"
        );
        // The cursor splits a word into three spans, so the break has to fall inside one.
        let cursor = hang(
            [
                vec![Span::raw("  what  ".to_owned())],
                super::typed("aaa bbb ccc", 5, ""),
            ]
            .concat(),
            8,
            16,
        );
        assert_eq!(
            cursor.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["  what  aaa bbb ", "        ccc"],
            "a wrapped answer keeps its cursor"
        );
    }

    #[test]
    fn config_explanation_keeps_the_rows_indent() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        let c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let (lines, _) = c.lines(48);
        let shown: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(
            shown.iter().any(|l| l.starts_with("runs  ")),
            "group headers sit on the margin"
        );
        assert!(
            shown.iter().any(|l| l.as_str() == "  activity"),
            "a block's sub-head is indented by two"
        );
        assert!(
            shown.iter().any(|l| l.starts_with("    alias or model id")),
            "rows are indented by four and led by their label, under their sub-head"
        );
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with("runs  ") && l.contains("enter opens it"))
                && !shown.iter().any(|l| l.starts_with("    time limit (min)")),
            "the runs section starts shut, with its head saying how to open it"
        );
        let mut tail: Vec<&String> = shown
            .iter()
            .skip_while(|l| !l.starts_with("    alias or model id  "))
            .skip(1)
            .collect();
        while tail.last().is_some_and(|l| l.is_empty()) {
            tail.pop();
        }
        let long = tail.iter().rev().take_while(|l| !l.is_empty()).count();
        assert!(long > 1, "the explanation wraps at the width given");
        assert!(
            tail.iter()
                .rev()
                .take(long)
                .all(|l| l.starts_with("    ") && l.chars().count() <= 48),
            "every wrapped line keeps the indent and fits"
        );
    }

    #[test]
    fn the_row_the_cursor_is_on_is_shaded_across_the_pane() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        c.row = 2;
        let (lines, at) = c.lines(48);
        let shaded: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.style.bg.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            shaded,
            (at..at + shaded.len()).collect::<Vec<_>>(),
            "the shading is the selected row and nothing else"
        );
        assert!(
            !shaded.is_empty()
                && shaded.iter().all(|&i| lines[i]
                    .spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
                    == 48),
            "every line of the row is padded out to the pane"
        );
    }

    #[test]
    fn guide_keys_are_documented() {
        let docs = include_str!("../docs/dashboard.md");
        for (key, _) in super::GUIDE {
            for word in key.split_whitespace() {
                assert!(docs.contains(word), "{word} is not in docs/dashboard.md");
            }
        }
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
        assert!(!app.key(KeyCode::Char('g'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Guide(0)));
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let text = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            text.contains("Viewers") && text.contains("this guide"),
            "{text}"
        );
        assert!(text.contains("↑ ↓ scroll · esc back"), "{text}");
        assert!(!app.key(KeyCode::Down, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(1)));
        assert!(!app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Normal));
    }

    use super::*;

    #[test]
    fn header_fits_the_counts_and_keeps_its_border_at_narrow_widths() {
        let counts = Line::raw("123 working  4 input  5 idle  6 done  ·  7 jobs  8 runs");
        for summary in [counts.clone(), Line::raw("1 idle"), Line::default()] {
            // 14 for the mascot, its gap and the borders, 2 for the padding inside them.
            let fitted = (summary.width() + 16).max(24);
            for width in [0, 1, 7, 23, 24, 40, 60, 80, 120] {
                let lines = header_lines(summary.clone(), width);
                assert_eq!(lines.len(), 3);
                assert!(lines.iter().all(|line| line.width() <= width));
                if width >= 24 {
                    let want = width.min(fitted);
                    assert!(lines.iter().all(|line| line.width() == want), "{width}");
                    for (line, border) in lines.iter().zip(['┐', '│', '┘']) {
                        let line = line.to_string();
                        assert!(line.ends_with(border), "{line}");
                    }
                    assert!(lines[0].to_string().contains("cones ─"), "{width}");
                }
            }
        }
        let wide = header_lines(counts, 120);
        let middle = wide[1].to_string();
        assert!(middle.contains("123 working") && middle.contains("8 runs"));
    }
    use ratatui::Terminal;
    use std::fs;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn an_image_is_one_character_a_label_on_screen_and_a_path_at_launch() {
        let (a, b) = (image_marker(0), image_marker(1));
        let mut text = "look at".to_owned();
        let end = text.len();
        let at = attach(&mut text, end, 0);
        assert_eq!(
            text,
            format!("look at {a} "),
            "spaced from the text before it"
        );
        assert_eq!(at, text.len(), "the cursor lands after it");
        let at = attach(&mut text, 0, 1);
        assert_eq!(
            text,
            format!("{b} look at {a} "),
            "at the cursor, spaced from what follows"
        );
        assert_eq!(at, b.len_utf8() + 1);
        let label = |n: usize| format!("[Image #{}]", n + 1);
        assert_eq!(expand(&text, label), "[Image #2] look at [Image #1] ");
        let end = text.len();
        let at = edit(&mut text, end, KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        let at = edit(&mut text, at, KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert_eq!(
            text,
            format!("{b} look at "),
            "backspace takes the whole image"
        );
        assert_eq!(at, text.len());
        let d = dir();
        let mut app = app(d.path());
        app.text = format!("see {a}");
        app.images.push(PathBuf::from("/tmp/cones/pasted-1.png"));
        app.caret = app.text.len();
        let line = app.composer();
        let shown: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(shown.contains("see [Image #1]"), "{shown}");
        assert_eq!(app.take_prompt(), "see /tmp/cones/pasted-1.png");
        assert!(app.text.is_empty() && app.images.is_empty() && app.caret == 0);
    }

    fn typed(f: &mut JobForm, text: &str) {
        for c in text.chars() {
            assert_eq!(
                f.key(KeyCode::Char(c), KeyModifiers::NONE),
                FormAction::Stay
            );
        }
    }

    fn enter(f: &mut JobForm) -> FormAction {
        f.key(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn job_form(base: &Path, original: Option<config::Job>, seed: &str) -> JobForm {
        JobForm::new(base, base, original, seed, &config::Policy::default())
    }

    #[test]
    fn every_prompt_edits_where_the_cursor_is() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("src")).unwrap();
        let mut f = job_form(base.path(), None, "fix the tests");
        f.key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        f.key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        typed(&mut f, "please ");
        assert_eq!(
            f.prompt, "please fix the ",
            "ctrl+w, then cmd+left and typing"
        );
        f.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(
            f.prompt, "fix the ",
            "cmd+delete takes everything before the cursor"
        );
        assert_eq!(enter(&mut f), FormAction::Stay);
        typed(&mut f, "sr");
        f.key(KeyCode::Left, KeyModifiers::NONE);
        f.key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            f.dir, "r",
            "backspace mid-answer takes the character before the cursor"
        );
        f.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(f.dir, "r");
        f.key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        typed(&mut f, "s");
        assert!(f.complete().is_empty());
        assert_eq!(
            f.dir, "src/",
            "tab completes and leaves the cursor after the path"
        );
        typed(&mut f, "x");
        assert_eq!(f.dir, "src/x");
        f.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        f.key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            f.row,
            JobRow::Ask(Step::What),
            "backspace on an empty answer still steps back"
        );
        assert_eq!(f.prompt, "fix the ");
        typed(&mut f, "x");
        assert_eq!(
            f.prompt, "fix the x",
            "the cursor is after the answer stepped back to"
        );

        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        c.go(field_at("timeout_min"));
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        for ch in "15".chars() {
            c.key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        c.key(KeyCode::Left, KeyModifiers::NONE);
        c.key(KeyCode::Char('0'), KeyModifiers::NONE);
        assert_eq!(
            c.values[field_at("timeout_min")],
            "105",
            "the defaults editor types where the cursor is"
        );
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.go(field_at("confirm_secs"));
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.key(KeyCode::Char('2'), KeyModifiers::NONE);
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.go(field_at("timeout_min"));
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.key(KeyCode::Char('7'), KeyModifiers::NONE);
        assert_eq!(
            c.values[field_at("timeout_min")],
            "1057",
            "another row puts the cursor after its value"
        );
        assert_eq!(c.values[field_at("confirm_secs")], "2");

        let mut i = Input::new("ab");
        assert!(i.key(KeyCode::Left, KeyModifiers::NONE));
        assert!(i.key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            !i.key(KeyCode::Enter, KeyModifiers::NONE),
            "enter is not an edit"
        );
        assert_eq!(
            i,
            Input {
                text: "axb".into(),
                at: 2
            }
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
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        app.filter = Input::new("209aa1a4");
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
            coordinator: false,
            activity: Vec::new(),
        };
        data.sessions
            .push(session("aaaa-interactive", "interactive"));
        data.sessions.push(session("bbbb-background", "bg"));
        let marker = |data: &Data, id: &str| {
            data.rows(false)
                .into_iter()
                .find(|r| matches!(&r.kind, Kind::Session(s, _) if s == id))
                .map(|r| r.cells[2].0.trim().to_owned())
                .unwrap()
        };
        assert_eq!(marker(&data, "aaaa-interactive"), "working");
        assert_eq!(marker(&data, "bbbb-background"), "working");
        data.columns = vec!["model".into()];
        let width = |data: &Data, id: &str| {
            data.rows(false)
                .into_iter()
                .find(|r| matches!(&r.kind, Kind::Session(s, _) if s == id))
                .map(|r| r.cells.len())
                .unwrap()
        };
        assert_eq!(
            width(&data, "aaaa-interactive"),
            4,
            "without a state column there is no slot before the title; the footer says own terminal"
        );
        assert_eq!(width(&data, "bbbb-background"), 4);
    }

    #[test]
    fn the_list_names_the_folders_orchestrator_in_orange() {
        let d = dir();
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        let session = |id: &str, kind: &str, coordinator: bool| Session {
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
            title: Some("sweep".into()),
            last: None,
            coordinator,
            activity: Vec::new(),
        };
        data.sessions.push(session("aaaa-worker", "bg", false));
        data.sessions.push(session("bbbb-orchestrator", "bg", true));
        data.sessions
            .push(session("cccc-typed", "interactive", true));
        let row = |data: &Data, id: &str| {
            data.rows(false)
                .into_iter()
                .find(|r| matches!(&r.kind, Kind::Session(s, _) if s == id))
                .unwrap()
        };
        assert_eq!(row(&data, "aaaa-worker").cells[2].0.trim(), "working");
        assert_eq!(row(&data, "aaaa-worker").cells[3].0.trim(), "sweep");
        assert_eq!(row(&data, "aaaa-worker").cells[3].1, plain());
        let marked = row(&data, "bbbb-orchestrator");
        assert_eq!(marked.cells[2].0.trim(), "working");
        assert_eq!(marked.cells[3].0.trim(), "★ sweep");
        assert_eq!(marked.cells[3].1, lit());
        data.columns = vec!["model".into()];
        let marked = row(&data, "bbbb-orchestrator");
        assert_eq!(
            marked.cells[2].0.trim(),
            "★ sweep",
            "no state, no word: the title carries the mark"
        );
        assert_eq!(marked.cells[2].1, lit());
        assert_eq!(row(&data, "cccc-typed").cells[2].0.trim(), "★ sweep");
        assert_eq!(
            row(&data, "aaaa-worker").cells[1].0.trim(),
            "✻",
            "without the harness column the mark stays and the name goes"
        );
        data.columns = vec!["harness".into()];
        assert_eq!(row(&data, "aaaa-worker").cells[1].0.trim(), "✻ claude");
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
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        app.filter = Input::new("codex-77");
        app.apply_filter();
        app.settle();
        assert!(matches!(&app.selected().unwrap().kind, Kind::Session(id, _) if id == "codex-77"));
        assert_eq!(app.enter_label(), "own terminal");
        app.filter = Input::default();
        app.apply_filter();
        assert_ne!(
            app.enter_label(),
            "own terminal",
            "the verb follows the selected row"
        );
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
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        app.filter = Input::new("dddd-dae");
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

    fn shown(f: &JobForm, columns: u16) -> Vec<String> {
        f.lines(columns).0.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn the_wizard_runs_once_or_schedules_a_claude_job() {
        let base = dir();
        let mut f = job_form(base.path(), None, "");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(
            (f.row, f.error.is_some()),
            (JobRow::Ask(Step::What), true),
            "an empty task stays"
        );
        typed(&mut f, "triage the TODOs");
        assert_eq!(f.error, None, "the next key clears the error");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.row, JobRow::Ask(Step::Where));
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.row, JobRow::Ask(Step::When));
        let canon = base.path().canonicalize().unwrap();
        assert_eq!(f.dir, fleet::tilde(&canon));
        assert_eq!(
            enter(&mut f),
            FormAction::RunOnce("triage the TODOs".into(), canon.clone()),
            "once runs now; no name is asked"
        );
        let rows = shown(&f, 120);
        assert!(rows[1].starts_with("new job"), "{rows:?}");
        assert!(rows[3].contains("what   triage the TODOs"), "{rows:?}");
        assert!(rows[5].contains("[once] hourly"), "{rows:?}");
        assert_eq!(
            rows.len(),
            6,
            "once takes the defaults, so it asks nothing more: {rows:?}"
        );
        for _ in 0..3 {
            f.key(KeyCode::Right, KeyModifiers::NONE);
        }
        assert_eq!(WHEN[f.when], "weekdays");
        let rows = shown(&f, 120);
        assert!(
            rows[7].contains("name   triage-the-todos"),
            "the name follows the task until it is typed: {rows:?}"
        );
        assert!(
            rows[9].contains("runs") && rows[9].contains("enter opens it"),
            "{rows:?}"
        );
        assert_eq!(
            rows.len(),
            10,
            "weekdays asks a time and a name, the settings stay shut: {rows:?}"
        );
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.row, JobRow::Ask(Step::At));
        typed(&mut f, "25:00");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert!(f.error.is_some(), "a bad time stays");
        f.at.clear();
        typed(&mut f, "8:30");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.row, JobRow::Ask(Step::Name));
        f.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(f.row, JobRow::Ask(Step::At), "the rows walk both ways");
        assert_eq!(enter(&mut f), FormAction::Stay);
        typed(&mut f, "bad name");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert!(f.error.is_some(), "a name with a space stays on its row");
        f.name = "nightly".into();
        match enter(&mut f) {
            FormAction::Save(None, job) => {
                assert_eq!(job.name, "nightly");
                assert_eq!(job.harness, None, "the file's default harness applies");
                assert_eq!(job.schedule, "30 8 * * 1-5");
                assert_eq!(job.cwd, PathBuf::from(&f.dir));
                assert_eq!(job.prompt, "triage the TODOs");
                assert_eq!(job.model, None);
                assert!(job.enabled);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_settings_section_writes_a_jobs_own_fields_and_defaults_the_rest() {
        let base = dir();
        let defaults = config::Policy {
            timeout_min: Some(45.0),
            write: Some(true),
            ..Default::default()
        };
        let mut f = JobForm::new(base.path(), base.path(), None, "sweep", &defaults);
        f.when = 2;
        f.go(JobRow::Head);
        assert!(f.shut, "a job with no settings of its own comes up folded");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(
            f.row,
            run_row("enabled"),
            "enter opens the section and goes in"
        );
        let rows = shown(&f, 200);
        let row = |key: &str| {
            rows.iter()
                .find(|l| l.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("no {key} row in {rows:?}"))
                .to_owned()
        };
        assert!(
            row("timeout_min").contains("45"),
            "the file's default is on the row"
        );
        assert!(
            row("write").contains("[true]"),
            "an empty row shows what it inherits: {}",
            row("write")
        );
        assert!(row("enabled").contains("[true]"), "{}", row("enabled"));
        f.go(run_row("write"));
        f.key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            f.set("write"),
            "false",
            "the arrows turn a row against what it inherits"
        );
        f.go(run_row("timeout_min"));
        f.key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            f.set("timeout_min"),
            "50",
            "a number steps from what it inherits"
        );
        f.go(run_row("env"));
        typed(&mut f, "LANG, BAD NAME");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.row, run_row("env"), "a refused name lands on its own row");
        assert!(f.error.is_some());
        for _ in 0..10 {
            f.key(KeyCode::Backspace, KeyModifiers::NONE);
        }
        assert_eq!(f.set("env"), "LANG");
        f.go(run_row("model"));
        typed(&mut f, "opus[1m]");
        assert_eq!(
            f.set("model"),
            "opus[1m]",
            "typing a word the ring offers keeps typing"
        );
        match enter(&mut f) {
            FormAction::Save(None, job) => {
                assert_eq!((job.write, job.timeout_min), (Some(false), Some(50.0)));
                assert_eq!(job.model.as_deref(), Some("opus[1m]"));
                assert_eq!(job.env, vec!["LANG".to_owned()]);
                assert_eq!(job.name, "sweep", "the name follows the task");
                assert_eq!(
                    job.schedule, "0 9 * * *",
                    "the time takes the row's default"
                );
                assert_eq!(
                    (job.notify, job.harness, job.overlap),
                    (None, None, None),
                    "a row left alone writes nothing and inherits"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn schedules_round_trip_between_the_picks_and_cron() {
        assert_eq!(to_cron(1, ""), Ok("0 * * * *".into()));
        assert_eq!(to_cron(2, "09:00"), Ok("0 9 * * *".into()));
        assert_eq!(to_cron(4, "Mon 7:15"), Ok("15 7 * * 1".into()));
        assert!(to_cron(4, "someday 7:15").is_err());
        assert!(to_cron(3, "9").is_err());
        assert_eq!(to_cron(5, "0 2 * * *"), Ok("0 2 * * *".into()));
        assert!(to_cron(5, "not cron").is_err());
        for cron in [
            "0 * * * *",
            "0 9 * * *",
            "30 8 * * 1-5",
            "15 7 * * 1",
            "*/5 * * * *",
        ] {
            let (when, at) = from_cron(cron);
            assert_eq!(to_cron(when, &at), Ok(cron.to_owned()), "{cron}");
        }
        assert_eq!(from_cron("15 7 * * 1"), (4, "mon 07:15".into()));
        assert_eq!(from_cron("*/5 * * * *").0, 5);
        assert_eq!(
            slug("  Read the TODOs!! and draft TRIAGE.md"),
            "read-the-todos-and-draft-triage-md"
        );
    }

    #[test]
    fn editing_opens_on_the_fields_the_job_carries() {
        let base = dir();
        let mut j = config::Job::new("one", "0 9 * * *", Path::new("."), "first");
        j.model = Some("sonnet".into());
        j.timeout_min = Some(45.0);
        let mut f = JobForm::new(
            base.path(),
            base.path(),
            Some(j),
            "ignored seed",
            &config::Policy::default(),
        );
        assert_eq!(
            (f.name.as_str(), f.dir.as_str(), WHEN[f.when], f.at.as_str()),
            ("one", ".", "daily", "09:00"),
            "the schedule opens on the picks that made it"
        );
        let rows = shown(&f, 120);
        assert!(rows[1].starts_with("edit one"));
        assert!(!f.shut, "a job that already sets fields opens on them");
        assert!(
            rows.iter().any(|l| l.contains("sonnet")),
            "the job's own model is on its row: {rows:?}"
        );
        typed(&mut f, ", revised");
        for _ in 0..4 {
            assert_eq!(enter(&mut f), FormAction::Stay);
        }
        assert_eq!(f.row, JobRow::Ask(Step::Name));
        match enter(&mut f) {
            FormAction::Save(Some(old), job) => {
                assert_eq!(old, "one");
                assert_eq!(job.prompt, "first, revised");
                assert_eq!(job.schedule, "0 9 * * *");
                assert_eq!(job.model.as_deref(), Some("sonnet"));
                assert_eq!(job.timeout_min, Some(45.0));
                assert_eq!(
                    job.cwd,
                    PathBuf::from(fleet::tilde(&base.path().canonicalize().unwrap()))
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.key(KeyCode::Esc, KeyModifiers::NONE), FormAction::Cancel);
    }

    fn registry(claude: &Path, id: &str, cwd: &str, status: &str, started: i64) {
        registry_kind(claude, id, cwd, status, started, "interactive");
    }

    fn registry_bg(claude: &Path, id: &str, cwd: &str, status: &str, started: i64) {
        registry_kind(claude, id, cwd, status, started, "bg");
    }

    fn registry_kind(claude: &Path, id: &str, cwd: &str, status: &str, started: i64, kind: &str) {
        fs::create_dir_all(claude.join("sessions")).unwrap();
        let mut entry = serde_json::json!({"pid": std::process::id(), "sessionId": id, "cwd": cwd,
            "kind": kind, "status": status, "startedAt": started, "updatedAt": started});
        if kind == "bg" {
            entry["jobId"] = serde_json::Value::String(id[..8].to_owned());
        }
        fs::write(
            claude.join("sessions").join(format!("{id}.json")),
            entry.to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_dashboard_launched_codex_thread_sorts_by_start_among_the_other_rows() {
        let d = tempfile::tempdir().unwrap();
        let (claude, state) = (d.path().join(".claude"), d.path().join("state"));
        let cwd = d.path().to_str().unwrap();
        registry(&claude, A, cwd, "idle", 1_789_000_000_000);
        let rollout = d.path().join("rollout.jsonl");
        fs::write(
            &rollout,
            format!(
                r#"{{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{{"id":"dddd","timestamp":"2026-09-01T00:00:00Z","cwd":{}}}}}"#,
                serde_json::to_string(cwd).unwrap()
            ) + "\n",
        )
        .unwrap();
        codex::remember(
            &state,
            codex::Thread {
                id: "dddd".into(),
                cwd: d.path().to_path_buf(),
                started: "2026-09-01T00:00:00Z".parse().unwrap(),
                rollout,
            },
        )
        .unwrap();
        let ids: Vec<String> = fleet_rows(&claude, &state, &[])
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, ["dddd", A], "the older thread comes first");
        // Forgetting drops the record, and the row goes with it; a thread the daemon still
        // holds keeps its row, because a live thread is worth seeing.
        codex::forget(&state, "dddd").unwrap();
        let ids: Vec<String> = fleet_rows(&claude, &state, &[])
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, [A], "a forgotten thread has no row");
    }

    #[test]
    fn folders_sort_by_name_with_pinned_ones_among_them() {
        let d = dir();
        let claude = d.path();
        let (alpha, beta, gamma) = (
            claude.join("alpha"),
            claude.join("Beta"),
            claude.join("gamma"),
        );
        for p in [&alpha, &beta, &gamma] {
            fs::create_dir(p).unwrap();
        }
        registry(claude, A, beta.to_str().unwrap(), "idle", 1_757_682_871_000);
        registry(
            claude,
            B,
            gamma.to_str().unwrap(),
            "blocked",
            1_757_682_872_000,
        );
        let mut app = app(claude);
        app.refresh().unwrap();
        app.pin_folder(alpha.clone()).unwrap();
        let headers = |app: &App| {
            app.rows
                .iter()
                .filter(|r| r.kind == Kind::Header)
                .map(Row::text)
                .collect::<Vec<_>>()
        };
        let names = [&alpha, &beta, &gamma].map(|p| fleet::tilde(p));
        assert_eq!(
            headers(&app),
            names,
            "alpha before Beta, the pinned one in place"
        );
        app.by_state = true;
        app.rebuild();
        assert_eq!(
            headers(&app),
            ["input", "idle", names[0].as_str()],
            "by state the pinned folder trails"
        );
    }

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
    fn a_session_that_just_appeared_takes_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        let cwd = claude.to_str().unwrap();
        registry(claude, A, cwd, "idle", 1_757_682_871_000);
        let mut app = app(claude);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        registry(claude, B, cwd, "idle", 1_757_682_872_000);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(B), "the new row is selected");
        app.text = "fix it".into();
        registry(claude, C, cwd, "idle", 1_757_682_873_000);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(B), "typing holds the cursor");
        app.text.clear();
        app.refresh().unwrap();
        assert_eq!(
            key(&app).as_deref(),
            Some(B),
            "a row seen once is not new again"
        );
        let session = placeholder("starting:1", claude, "again");
        app.data.sessions.push(session.clone());
        app.pending.push(Pending {
            session,
            short: Some("dddddddd".into()),
            at: Instant::now(),
        });
        app.rebuild();
        app.select_new("starting:1");
        assert_eq!(key(&app).as_deref(), Some("starting:1"));
        let d = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        registry(claude, d, cwd, "idle", 1_757_682_874_000);
        app.refresh().unwrap();
        assert_eq!(
            key(&app).as_deref(),
            Some(d),
            "the listed row took the cursor"
        );
        let session = placeholder("starting:2", claude, "once more");
        app.data.sessions.push(session.clone());
        app.pending.push(Pending {
            session,
            short: Some("eeeeeeee".into()),
            at: Instant::now(),
        });
        app.rebuild();
        app.select_new("starting:2");
        app.select_new(B);
        assert_eq!(key(&app).as_deref(), Some(B));
        registry(
            claude,
            "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
            cwd,
            "idle",
            1_757_682_875_000,
        );
        app.refresh().unwrap();
        assert_eq!(
            key(&app).as_deref(),
            Some(B),
            "the handover does not pull the cursor back"
        );
    }

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
        assert_eq!(Kind::Menu.key(), Some("menu"));
        assert_eq!(Kind::Header.key(), None);
        assert_eq!(Kind::Columns.key(), None);
        assert_eq!(Kind::Blank.key(), None);
    }

    #[test]
    fn coming_back_lands_on_the_same_row_with_filter_and_grouping_kept() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        registry(claude, B, "/src/two", "idle", 1_757_682_872_000);
        registry(claude, C, "/src/two", "idle", 1_757_682_873_000);
        let mut app = app(claude);
        app.by_state = true;
        app.filter = Input::new("two");
        app.split = false;
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
        assert_eq!(app.filter.text, "two", "filter kept");
        assert!(matches!(app.mode, Mode::Normal));
        // The session ended while open: the cursor falls on a remaining row, not on nothing.
        fs::remove_file(claude.join("sessions").join(format!("{C}.json"))).unwrap();
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(B));
        assert!(app.by_state && app.filter.text == "two");
        app.filter = Input::default();
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
    fn a_start_that_never_opens_says_why_in_the_log() {
        let d = dir();
        let mut app = app(d.path());
        let log = d.path().join("tui-debug.log");
        app.log = Some(log.clone());
        app.prepare_viewer(
            "codex in ~/src".into(),
            "codex:test".into(),
            None,
            Some("fix the lag".into()),
            || anyhow::bail!("no daemon"),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.poll_opening() {
            assert!(Instant::now() < deadline, "preparation did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.text, "fix the lag");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("codex in ~/src failed: no daemon"), "{text}");
    }

    /// A read that fails leaves the previous rows on screen, so the header says they are the
    /// previous ones. The status line cannot carry that: the next keypress overwrites it.
    #[test]
    fn a_failed_read_marks_the_header_stale_until_one_succeeds() {
        let d = dir();
        let mut app = app(d.path());
        let good = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        let text = |app: &App| {
            app.header_summary()
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert!(!text(&app).contains("stale"), "a good read says nothing");
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        tx.send(Err(anyhow::anyhow!(
            "reading the process table with /bin/ps: no such file"
        )))
        .unwrap();
        app.poll();
        assert!(app.stale);
        assert!(text(&app).contains("! stale"), "{}", text(&app));
        assert!(app.status.contains("/bin/ps"), "{}", app.status);
        // A keypress takes the status line; the header keeps the mark.
        app.status = "opening codex".into();
        assert!(text(&app).contains("! stale"));
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        tx.send(Ok(good)).unwrap();
        app.poll();
        assert!(!app.stale, "a good read clears it");
        assert!(!text(&app).contains("stale"));
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
    fn a_deleted_row_leaves_no_note_behind() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.queue_stop(A.into(), "delete", || Ok(true));
        poll_until(&mut app, |a| a.stopping.is_empty());
        assert!(
            app.status.is_empty(),
            "the row leaving the list is the answer"
        );
        assert!(app.rows.iter().all(|r| r.kind.key() != Some(A)));
        poll_until(&mut app, |a| a.loading.is_none());
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
        app.split = false;
        app.refresh().unwrap();
        app.show_jobs();
        assert!(matches!(app.selected().unwrap().kind, Kind::Job(_)));
        assert_eq!(app.stop_verb(), Some("delete"));
        let hint: String = app
            .hint_line()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            hint.starts_with(
                "enter start job · ctrl+x delete · ctrl+e edit · shift+tab harness · esc back"
            ),
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
    fn ctrl_c_quits_only_when_pressed_twice_in_quick_succession() {
        let d = dir();
        let mut app = app(d.path());
        let c = |app: &mut App| app.key(KeyCode::Char('c'), KeyModifiers::CONTROL).unwrap();
        assert!(!c(&mut app), "one ctrl+c only arms");
        assert_eq!(app.status, QUIT_HINT);
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let red = |t: &Terminal<ratatui::backend::TestBackend>, sym: &str| {
            t.backend()
                .buffer()
                .content()
                .iter()
                .any(|c| c.symbol() == sym && c.fg == Color::Red)
        };
        assert!(red(&t, "─"), "the composer's rules are red");
        assert!(red(&t, "q"), "the hint is red");
        app.quit_armed = Some(Instant::now() - QUIT_CONFIRM);
        app.expire();
        assert!(app.quit_armed.is_none() && app.status.is_empty());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(!red(&t, "─"), "the rules are dim again");
        assert!(!c(&mut app), "one ctrl+c only arms");
        assert!(c(&mut app), "the second quits");
        app.quit_armed = Some(Instant::now() - QUIT_CONFIRM);
        assert!(!c(&mut app), "a stale arm is a first press again");
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
        app.armed_at = Instant::now() - Duration::from_secs(3);
        app.expire();
        assert_eq!((app.armed.as_deref(), app.status.as_str()), (None, "kept"));
        app.stop();
        assert_eq!(app.armed.as_deref(), Some(A), "armed again, not hidden");
        app.data.confirm_secs = 0.0;
        app.armed_at = Instant::now() - Duration::from_secs(3600);
        app.expire();
        assert_eq!(app.armed.as_deref(), Some(A), "no clock at 0");
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
    fn the_composer_edits_where_the_cursor_is() {
        let d = tempfile::tempdir().unwrap();
        let mut app = app(d.path());
        let k = |app: &mut App, code, mods| {
            assert!(!app.key(code, mods).unwrap());
        };
        for c in "fix the tests".chars() {
            k(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        k(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(app.text, "fix the ", "option+delete takes a word");
        k(&mut app, KeyCode::Left, KeyModifiers::ALT);
        k(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(app.text, "the ", "and so does alt+backspace, from mid-text");
        k(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        for c in "please ".chars() {
            k(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.text, "please the ", "cmd+left, then typing lands there");
        k(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        k(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            app.text, "please the",
            "cmd+right is end of line, not edit job"
        );
        k(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
        app.paste("whole ");
        assert_eq!(app.text, "please whole the", "a paste lands at the cursor");
        let under = app
            .composer()
            .spans
            .into_iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .unwrap();
        assert_eq!(
            under.content, "t",
            "the block cursor sits on the next character"
        );
        k(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(
            app.text, "the",
            "cmd+delete takes everything before the cursor"
        );
        k(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(app.text.is_empty());
        app.paste("");
        assert!(
            app.text.starts_with(image_marker(0)) || app.status == "no image on the clipboard",
            "an empty paste is an image paste: the clipboard's PNG, or the status says there is none"
        );
        for png in app.images.drain(..) {
            let _ = std::fs::remove_file(png);
        }
        app.text.clear();
        assert_eq!(
            snap("héllo", 2),
            1,
            "a stale offset lands on a character boundary"
        );
    }

    #[test]
    fn the_composer_wraps_a_long_instruction_instead_of_cutting_it() {
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
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
    fn the_bottom_lines_name_the_harness_and_what_ctrl_x_does() {
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
        app.refresh().unwrap();
        let text = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(
            text(app.composer()),
            "",
            "a button takes no instruction, so the menu row has no prompt"
        );
        let hint = text(app.hint_line());
        assert!(
            hint.starts_with("enter add folder · ← → pick · shift+tab harness · esc quit"),
            "an empty dashboard opens on the menu row, folder picked: {hint}"
        );
        app.text = "fix the tests".into();
        // The key names its own effect: with three harnesses it cannot name the next one.
        for (mark, name) in [(">_", "codex"), ("\u{3c0}", "pi"), ("\u{273b}", "claude")] {
            app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
            let (composer, hint) = (text(app.composer()), text(app.hint_line()));
            assert!(
                composer.starts_with(&format!("{mark} {name} \u{203a} ")),
                "{composer}"
            );
            assert!(
                hint.starts_with(&format!("enter start {name} in ")),
                "{hint}"
            );
            assert!(hint.contains("shift+tab harness"), "{hint}");
        }
        app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        assert!(text(app.composer()).starts_with(">_ codex \u{203a} "));
        app.menu = 1;
        assert!(text(app.hint_line()).starts_with("enter new job with it"));
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

    #[test]
    fn the_top_menu_is_reached_going_up_and_its_folder_prompt_adds_a_row() {
        let d = dir();
        let claude = d.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(claude);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A), "opens on the first table");
        app.step(-1);
        assert_eq!(key(&app).as_deref(), Some("menu"));
        assert!(app.menu_is("folder"), "folder is picked until ← → move it");
        let home = app.cwd.clone();
        assert_eq!(
            app.target_dir(),
            home,
            "the menu row launches into the dashboard's own directory"
        );
        let inside = claude.join("inside");
        fs::create_dir(&inside).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(app.mode, Mode::Folder(_)),
            "enter opens the prompt"
        );
        for c in "nowhere-such-dir".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(app.mode, Mode::Folder(_)),
            "a missing directory is refused"
        );
        assert!(app.status.contains("not a directory"), "{}", app.status);
        app.mode = Mode::Folder(Input::new(inside.display().to_string()));
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        let inside = inside.canonicalize().unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            key(&app).as_deref(),
            Some(fleet::tilde(&inside).as_str()),
            "the cursor moves onto the folder's row"
        );
        assert_eq!(app.target_dir(), inside, "so the composer starts there");
        assert_eq!(app.cwd, home, "the menu's own target did not move");
        while key(&app).as_deref() != Some("menu") {
            app.step(-1);
        }
        assert_eq!(app.target_dir(), home);
        app.refresh().unwrap();
        assert_eq!(
            key(&app).as_deref(),
            Some("menu"),
            "a reload keeps the menu row"
        );
        assert!(app.menu_is("folder"), "and the picked button");
        registry(claude, B, "/src/two", "idle", 1_757_682_872_000);
        app.refresh().unwrap();
        assert_eq!(
            fs::read_to_string(claude.join("recent")).unwrap(),
            "/src/two\n/src/one\n",
            "a folder seen for the first time goes to the front"
        );
        app.mode = Mode::Folder(Input::default());
        app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Folder(t) if t.text == "/src/two"),
            "{:?}",
            app.status
        );
        app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Folder(t) if t.text == "/src/one"));
        app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Folder(t) if t.text == "/src/two"),
            "wraps"
        );
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Folder(t) if t.text == "/src/one"));
        app.mode = Mode::Normal;
    }

    #[test]
    fn the_jobs_screen_lists_the_jobs_with_a_new_job_row() {
        let d = dir();
        let claude = d.path();
        let cwd = claude.canonicalize().unwrap();
        let jobs = claude.join("jobs.yaml");
        fs::write(
            &jobs,
            format!(
                "version: 1\njobs:\n  - name: nightly\n    schedule: \"0 2 * * *\"\n    harness: claude\n    cwd: {}\n    prompt: first\n    model: sonnet\n    enabled: false\n",
                cwd.display()
            ),
        )
        .unwrap();
        registry(claude, A, cwd.to_str().unwrap(), "idle", 1_757_682_871_000);
        registry(claude, B, "/src/other", "idle", 1_757_682_871_000);
        let mut app = App::new(Path::new("cones"), &jobs, claude, claude).unwrap();
        app.pin_folder(cwd.clone()).unwrap();
        app.split = false;
        app.refresh().unwrap();
        let keys: Vec<String> = app
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Header => Some(format!("# {}", r.text())),
                Kind::Folder(_) => Some("folder".into()),
                k => k.key().map(str::to_owned),
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                "menu".to_owned(),
                format!("# {}", fleet::tilde(&cwd)),
                A.into(),
                "# /src/other".into(),
                B.into(),
            ],
            "the dashboard has no job row and the pinned folder has no placeholder"
        );
        app.show_jobs();
        let keys: Vec<String> = app
            .rows
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Header => Some(format!("# {}", r.text())),
                k => k.key().map(str::to_owned),
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                "menu".to_owned(),
                "# jobs".into(),
                "nightly".into(),
                "new job".into()
            ]
        );
        assert_eq!(key(&app).as_deref(), Some("nightly"));
        assert_eq!(app.enter_label(), "start job");
        app.step(1);
        assert_eq!(app.enter_label(), "new job");
        app.enter().unwrap();
        assert!(matches!(app.mode, Mode::Job(_)));
        app.mode = Mode::Normal;
        let job = app
            .rows
            .iter()
            .find(|r| r.kind.key() == Some("nightly"))
            .unwrap();
        let text = job.text();
        assert!(
            text.contains("off · 0 2 * * *"),
            "the last run's status, or off, sits before the name with the schedule: {text}"
        );
        assert!(text.contains("nightly"), "{text}");
        assert!(
            text.contains(" off "),
            "a disabled job says so under state: {text}"
        );
        assert!(text.contains("sonnet"), "{text}");
        assert!(
            text.contains(&fleet::tilde(&cwd)),
            "the jobs screen shows each job's directory: {text}"
        );
        let names = app
            .rows
            .iter()
            .find(|r| r.kind == Kind::Columns)
            .map(Row::text)
            .unwrap_or_default();
        assert!(
            names.contains("model") && names.contains("dir") && !names.contains("context"),
            "the jobs screen has a job's columns, not a session's: {names}"
        );
        let hint: String = app
            .hint_line()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(hint.ends_with("esc back"), "{hint}");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.jobs_view, "esc leaves the jobs screen");
        assert!(app.rows.iter().all(|r| !matches!(r.kind, Kind::Job(_))));
        app.by_state = true;
        app.rebuild();
        let headers: Vec<String> = app
            .rows
            .iter()
            .filter(|r| r.kind == Kind::Header)
            .map(Row::text)
            .collect();
        assert_eq!(headers, vec!["idle".to_owned()]);
    }

    #[test]
    fn a_picked_folder_keeps_a_row_until_it_is_removed() {
        let d = dir();
        let claude = d.path();
        registry(claude, A, "/src/one", "idle", 1_757_682_871_000);
        let inside = claude.join("inside");
        fs::create_dir(&inside).unwrap();
        assert!(
            Command::new("git")
                .args(["-C", inside.to_str().unwrap(), "init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        fs::write(inside.join("new.txt"), "").unwrap();
        let mut app = app(claude);
        app.refresh().unwrap();
        let picked = launch_dir(&inside.display().to_string(), &app.cwd, &app.cwd).unwrap();
        let name = fleet::tilde(&picked);
        app.pin_folder(picked.clone()).unwrap();
        let folder_row = |app: &App| {
            app.visible
                .iter()
                .position(|&i| app.rows[i].kind == Kind::Folder(name.clone()))
        };
        assert!(folder_row(&app).is_some(), "the folder has a row at once");
        app.refresh().unwrap();
        let row = &app.rows[app.visible[folder_row(&app).unwrap()]];
        assert!(
            row.text().contains(" · 1 change · nothing runs here"),
            "the row leads with the branch and the tree state: {}",
            row.text()
        );
        assert_eq!(
            git_state(Path::new("/")),
            None,
            "outside a repository, nothing"
        );
        assert!(
            app.rows
                .iter()
                .any(|r| r.kind == Kind::Header && r.text() == name),
            "under its own group title"
        );
        app.refresh().unwrap();
        assert!(folder_row(&app).is_some(), "a reload keeps it");
        app.cursor = folder_row(&app).unwrap();
        assert_eq!(app.target_dir(), picked, "the composer starts there");
        assert_eq!(app.stop_verb(), Some("remove"));

        registry(
            claude,
            B,
            &picked.display().to_string(),
            "idle",
            1_757_682_871_000,
        );
        app.refresh().unwrap();
        assert!(
            folder_row(&app).is_none(),
            "a session in the folder takes the group"
        );
        fs::remove_file(claude.join("sessions").join(format!("{B}.json"))).unwrap();
        app.refresh().unwrap();
        assert!(
            folder_row(&app).is_some(),
            "and the row is back when it leaves"
        );

        app.select_new(A);
        assert_eq!(key(&app).as_deref(), Some(A));
        app.key(KeyCode::Char('p'), KeyModifiers::CONTROL).unwrap();
        assert!(app.status.contains("pinned"), "{}", app.status);
        assert_eq!(
            key(&app).as_deref(),
            Some(A),
            "the cursor stays on the session"
        );
        assert!(
            !app.rows
                .iter()
                .any(|r| r.kind == Kind::Folder("/src/one".into())),
            "no placeholder while the session runs"
        );
        app.key(KeyCode::Char('p'), KeyModifiers::CONTROL).unwrap();
        assert!(app.status.contains("already"), "{}", app.status);
        fs::remove_file(claude.join("sessions").join(format!("{A}.json"))).unwrap();
        app.refresh().unwrap();
        assert!(
            app.rows
                .iter()
                .any(|r| r.kind == Kind::Folder("/src/one".into())),
            "the folder keeps a row after the session leaves"
        );
        assert_eq!(
            fs::read_to_string(claude.join("folders")).unwrap(),
            format!("{}\n/src/one\n", picked.display())
        );

        app.cursor = folder_row(&app).unwrap();
        app.stop();
        assert!(app.status.starts_with("ctrl+x again"), "{}", app.status);
        app.stop();
        assert!(app.status.contains("removed"), "{}", app.status);
        assert!(folder_row(&app).is_none(), "gone at once");
        app.refresh().unwrap();
        assert!(folder_row(&app).is_none(), "and after a reload");
        assert_eq!(
            fs::read_to_string(claude.join("folders")).unwrap(),
            "/src/one\n",
            "the folder ctrl+p pinned stays"
        );
    }

    #[test]
    fn columns_never_shrink() {
        let row = |a: &str, b: &str| vec![(a.to_owned(), plain()), (b.to_owned(), plain())];
        let mut widths = Widths::new();
        let (_, wide) = columns(&["state", "age"], vec![row("input", "59s")], &mut widths);
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
            speculative: false,
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
    fn a_focused_viewer_keeps_ctrl_c_from_the_client() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.viewers.push(viewer_open("run:r1", "attach", "VIEW"));
        app.focus = Some(0);
        let before = app.viewers[0].viewer.pid();
        assert!(!app.key(KeyCode::Char('c'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, Some(0), "the viewer stays focused");
        assert_eq!(app.status, QUIT_HINT, "it arms quit as from the list");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            app.viewers[0].viewer.exited().is_none() && app.viewers[0].viewer.pid() == before,
            "the client never saw the byte"
        );
        app.full = true;
        let text = |w| {
            app.strip(0, w)
                .spans
                .iter()
                .map(|s| s.content.clone().into_owned())
                .collect::<String>()
        };
        assert!(
            text(120).contains(QUIT_HINT),
            "a full-frame viewer shows the armed quit in its strip: {:?}",
            text(120)
        );
        assert!(
            text(40).contains("ctrl+c again quits"),
            "a narrow strip keeps the press that quits: {:?}",
            text(40)
        );
        assert!(
            app.key(KeyCode::Char('c'), KeyModifiers::CONTROL).unwrap(),
            "the second press quits"
        );
    }

    #[test]
    fn a_focused_viewer_takes_the_frame_and_ctrl_z_brings_the_composer_back() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.split = false;
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
            !screen.iter().any(|r| r.contains("Type an instruction…")),
            "the composer is not drawn under a viewer: {screen:#?}"
        );
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, None);
        assert!(
            app.status.is_empty(),
            "leaving a viewer says nothing: {}",
            app.status
        );
        assert_eq!(app.viewers.len(), 1, "the viewer is alive off-screen");
        assert!(app.needs_clear);
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 80);
        assert!(
            screen.iter().any(|r| r.contains("Type an instruction…")),
            "{screen:#?}"
        );
        assert!(!screen[0].starts_with("VIEW"), "{screen:#?}");
    }

    #[test]
    fn a_focused_viewer_sits_on_a_pane_above_the_dashboards_strip() {
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
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
        let strip = &screen[11];
        assert!(strip.contains("cones"), "{strip:?}");
        assert!(
            strip.contains("· attach ·"),
            "with no title the viewer's what names it: {strip:?}"
        );
        assert!(
            strip.contains("0 working"),
            "the fleet counts are on it: {strip:?}"
        );
        assert!(
            strip.trim_end().ends_with("tab back · ctrl+\\ split"),
            "{strip:?}"
        );
        assert!(
            !strip.contains("ctrl+] next"),
            "one viewer has no next: {strip:?}"
        );
        assert_eq!(
            app.viewers[0].viewer.screen().size(),
            (11, 80),
            "the viewer is sized to the pane, not the frame"
        );
    }

    #[test]
    fn ctrl_bracket_is_the_viewers_key_and_the_dashboard_never_offers_it() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.viewers.push(viewer_open("run:r1", "attach", "ONE"));
        app.viewers.push(viewer_open("run:r2", "logs", "TWO"));
        app.focus(0);
        assert!(!app.key(KeyCode::Char(']'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.key(KeyCode::Char('5'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, Some(0), "no cycling");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 80);
        assert!(!screen[11].contains("ctrl+]"), "{:?}", screen[11]);
        app.unfocus();
        app.status.clear();
        assert!(!app.hint_line().to_string().contains("ctrl+]"));
        assert!(!app.key(KeyCode::Char(']'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, None, "from the list the key does nothing");
    }

    /// A pi the composer starts is the process cones holds in a viewer, and pi reports no
    /// session id until its first turn writes one, so the row that discovers the process is
    /// the only way back in. One row, not two: cones adds no placeholder of its own.
    #[test]
    fn a_composer_pi_is_one_row_that_returns_to_the_viewer_holding_its_process() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let open = viewer_open("pi:start:1757682871000", "pi in /x", "");
        let pid = open.viewer.pid();
        app.viewers.push(open);
        let row = |id: &str, harness: &str| {
            let mut s = session(id, "working", "fix the tests", 1);
            s.harness = harness.into();
            s.kind = None;
            s.pid = Some(pid);
            s.title = None;
            s
        };
        let listed = |app: &mut App, s: Session| {
            let id = s.session_id.clone();
            let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
            data.sessions.push(s);
            app.apply(data);
            // A row shows a nameless session by the head of its id; the filter reads the row.
            app.filter = Input::new(id.chars().take(8).collect::<String>());
            app.apply_filter();
            app.settle();
            assert!(
                matches!(app.selected().map(|r| &r.kind), Some(Kind::Session(s, _)) if *s == id),
                "{id} is the selected row"
            );
            app.rows
                .iter()
                .filter(|r| matches!(&r.kind, Kind::Session(s, _) if *s == id))
                .count()
        };
        assert_eq!(listed(&mut app, row(&format!("pi-{pid}"), "pi")), 1);
        assert_eq!(
            app.enter_label(),
            "return",
            "the viewer cones holds is the way in"
        );
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        app.unfocus();
        // The first turn gives the session a name of its own; the process has not changed.
        listed(&mut app, row("4f3c2b1a-pi", "pi"));
        assert_eq!(app.enter_label(), "return");
        // A pid the kernel recycled into another harness is not this viewer's session.
        listed(&mut app, row("codex-99", "codex"));
        assert_eq!(app.enter_label(), "own terminal");
        app.enter().unwrap();
        assert!(app.status.contains("cannot be joined"), "{}", app.status);
    }

    fn session(id: &str, state: &str, title: &str, secs: i64) -> Session {
        Session {
            session_id: id.into(),
            harness: "claude".into(),
            kind: Some("bg".into()),
            cwd: PathBuf::from("/x"),
            state: state.into(),
            started: None,
            last_activity: Some(chrono::Utc::now() - chrono::Duration::seconds(secs)),
            model: None,
            pid: None,
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            context_window: None,
            cost_usd: None,
            title: Some(title.into()),
            last: None,
            coordinator: false,
            activity: Vec::new(),
        }
    }

    #[test]
    fn the_strip_names_the_latest_other_session_that_needs_input_and_fits_a_narrow_width() {
        let d = dir();
        let mut app = app(d.path());
        let mut data = Data::load(&d.path().join("none.yaml"), d.path(), d.path()).unwrap();
        data.sessions
            .push(session(A, "blocked", "the one on screen", 1));
        data.sessions
            .push(session(B, "blocked", "an older prompt", 60));
        data.sessions.push(session(
            C,
            "blocked",
            "a title far longer than the twenty-four columns it gets",
            30,
        ));
        app.apply(data);
        app.viewers.push(viewer_open(A, "attach", "ONE"));
        app.viewers.push(viewer_open("run:r2", "logs", "TWO"));
        app.size = (12, 200);
        app.focus(0);
        let line = app.strip(0, 200);
        let text = line.to_string();
        assert!(
            text.contains("· the one on screen ·"),
            "the row's title names the viewer: {text}"
        );
        assert!(
            text.contains(" · a title far longer than… needs input"),
            "the most recent other blocked session, clipped to 24: {text}"
        );
        assert!(!text.contains("the one on screen needs input"), "{text}");
        assert!(!text.contains("an older prompt needs input"), "{text}");
        let alert = line
            .spans
            .iter()
            .find(|s| s.content.contains("needs input"))
            .expect("the alert is its own span");
        assert_eq!(alert.style.fg, Some(Color::Yellow));
        assert!(
            text.trim_end().ends_with("tab back · ctrl+\\ split"),
            "a frame wide enough for the split offers it: {text}"
        );
        assert_eq!(line.width(), 200, "padded to the width");

        let text = app.strip(0, 60).to_string();
        assert!(!text.contains("needs"), "no partial note: {text}");
        assert!(text.starts_with("▲ cones · the one on screen"), "{text}");
        assert!(
            text.trim_end().ends_with("tab back · ctrl+\\ split"),
            "{text}"
        );
        assert_eq!(app.strip(0, 60).width(), 60);

        let text = app.strip(0, 24).to_string();
        assert!(text.trim_end().ends_with("tab back"), "{text}");
        assert!(app.strip(0, 24).width() <= 24);
        let text = app.strip(0, 10).to_string();
        assert_eq!(text, "▲ cones   ", "{text}");

        // A thread started from the composer has no id until its first ctrl+z, so its own
        // prompt cannot be told from another's; no alert until then.
        app.viewers[0].key = "codex:start:1".into();
        app.viewers[0].record = Some((PathBuf::from("/x"), chrono::Utc::now()));
        app.viewers[0].what = "codex in ~/x".into();
        let text = app.strip(0, 200).to_string();
        assert!(!text.contains("needs input"), "{text}");
        assert!(text.contains("· codex in ~/x ·"), "{text}");
        app.viewers[0].recorded = true;
        assert!(app.strip(0, 200).to_string().contains("needs input"));
    }

    #[test]
    fn a_one_row_frame_is_all_pane_and_the_strip_row_swallows_the_mouse() {
        use ratatui::crossterm::event::MouseButton;
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
        app.refresh().unwrap();
        app.viewers.push(viewer_open("run:r1", "attach", "VIEW"));
        app.focus = Some(0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.pump() {
            assert!(Instant::now() < deadline, "the viewer never drew");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 80);
        assert!(screen[0].starts_with("VIEW"), "{screen:#?}");
        assert!(
            !screen[0].contains("cones"),
            "no strip on one row: {screen:#?}"
        );
        assert_eq!(app.viewers[0].viewer.screen().size(), (1, 80));
        let ev = |kind, row| MouseEvent {
            kind,
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(app.pane_mouse(ev(down, 0)).map(|e| e.row), Some(0));

        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.pane_mouse(ev(down, 10)).map(|e| e.row), Some(10));
        assert!(
            app.pane_mouse(ev(down, 11)).is_none(),
            "a press on the strip"
        );
        assert!(
            app.pane_mouse(ev(MouseEventKind::ScrollUp, 11)).is_none(),
            "a wheel on the strip"
        );
        assert_eq!(
            app.pane_mouse(ev(MouseEventKind::Up(MouseButton::Left), 11))
                .map(|e| e.row),
            Some(10),
            "a release that crossed onto the strip lands on the pane's last row"
        );
        assert_eq!(
            app.pane_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 11))
                .map(|e| e.row),
            Some(10)
        );
    }

    fn wait_paint(app: &mut App, i: usize, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            app.pump();
            let screen = app.viewers[i].viewer.screen();
            let row: String = (0..screen.size().1)
                .filter_map(|c| screen.cell(0, c))
                .map(|c| c.contents())
                .collect();
            if row.starts_with(text) {
                return;
            }
            assert!(Instant::now() < deadline, "the viewer never drew {text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn cells(
        t: &Terminal<ratatui::backend::TestBackend>,
        y: u16,
        x: std::ops::Range<u16>,
    ) -> String {
        let buf = t.backend().buffer();
        x.map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
            .collect()
    }

    #[test]
    fn shift_enter_mid_prompt_breaks_the_line_instead_of_starting() {
        let (_d, mut app, mut t) = split_setup(200);
        app.unfocus();
        for c in "ab".chars() {
            assert!(!app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap());
        }
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
        for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert!(!app.key(KeyCode::Enter, mods).unwrap());
        }
        assert_eq!(app.text, "a\n\nb");
        assert!(app.pending.is_empty(), "nothing was started");
        // A paste brings CR breaks; they join the typed ones.
        app.paste("c\r\nd\re");
        assert_eq!(app.text, "a\n\nc\nd\neb");
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 200);
        assert!(
            screen.iter().any(|r| r.contains("a⏎⏎c⏎d⏎eb")),
            "the breaks show as a glyph on the one composer line: {screen:#?}"
        );
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(!app.pending.is_empty(), "plain enter still starts it");
    }

    #[test]
    fn shift_enter_attaches_over_the_whole_frame() {
        let (_d, mut app, mut t) = split_setup(200);
        for mods in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            app.unfocus();
            assert!(app.split);
            assert!(!app.key(KeyCode::Enter, mods).unwrap());
            assert!(app.split, "{mods:?} leaves the layout alone");
            assert!(!app.split_active(), "{mods:?} takes the whole frame");
            assert_eq!(app.focus, Some(0));
            t.draw(|f| app.draw(f)).unwrap();
            assert_eq!(app.viewers[0].viewer.screen().size(), (29, 200));
        }
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(
            app.needs_clear,
            "a viewer that had the frame is cleared away"
        );
        assert!(app.split_active(), "ctrl+z brings the pane back");
        app.needs_clear = false;
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split && app.split_active() && app.focus == Some(0));
        app.unfocus();
        app.split = false;
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(
            app.split,
            "pane off, ctrl+\\ after shift+enter asks for the split"
        );
        app.unfocus();
        app.split = false;
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(
            !app.split && app.focus == Some(0),
            "pane off already, it attaches"
        );
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split, "and ctrl+\\ there turns the pane on");
    }

    #[test]
    fn the_composers_rule_lands_on_the_harnesss_own() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let rule = "\u{2500}".repeat(60);
        // The bottom of a Claude screen: an input box with two status rows under it, hung
        // off the bottom of the screen as a harness hangs them, painted only once the first
        // draw has sized the viewer to the pane, the whole 30 rows of the frame.
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            &format!(
                "printf 'VIEW'; read x; \
                 i=0; while [ $i -lt 80 ]; do i=$((i+1)); echo history$i; done; \
                 printf '\\033[H\\033[2J'; \
                 printf '\\033[26;1H{rule}\\033[27;1H> \\033[28;1H{rule}\\033[29;1Hstatus\\033[30;1Hmode\\033[30;60Hcycle'; \
                 sleep 5"
            ),
        ]);
        app.viewers.push(Open {
            key: A.into(),
            what: "attach".into(),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: false,
        });
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.viewers[0].viewer.screen().size(), (30, 99));
        app.viewers[0].viewer.write(b"\n");
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.foot_rows() == 1 {
            app.pump();
            assert!(Instant::now() < deadline, "the viewer never drew its box");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            app.foot_rows(),
            2,
            "the two status rows the harness keeps under its rule"
        );
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 200);
        assert_eq!(
            cells(&t, 27, 0..1),
            "\u{2500}",
            "the composer's lower rule is on the harness's row: {screen:#?}"
        );
        assert_eq!(
            cells(&t, 27, 101..105),
            "\u{2500}\u{2500}\u{2500}\u{2500}",
            "which is the row the harness ruled: {screen:#?}"
        );
        assert!(
            cells(&t, 28, 0..100).starts_with("enter"),
            "the hint line is right under it: {screen:#?}"
        );
        assert_eq!(
            cells(&t, 29, 101..105),
            "mode",
            "and the harness's last status row is the frame's last row, with no row \
             of the pane held back: {screen:#?}"
        );

        app.focus = Some(0);
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 200);
        assert!(
            cells(&t, 28, 0..100).starts_with("tab back"),
            "focused, the keys are in the list's hint row: {screen:#?}"
        );
        assert!(
            cells(&t, 29, 101..200).contains("mode") && cells(&t, 29, 160..200).contains("cycle"),
            "so the harness keeps the status row its permission mode is on: {screen:#?}"
        );

        for focus in [None, Some(0)] {
            app.focus = focus;
            t.draw(|f| app.draw(f)).unwrap();
            let dashboard: Vec<_> = (0..30).map(|y| cells(&t, y, 0..100)).collect();
            for kind in [MouseEventKind::ScrollUp; 12]
                .into_iter()
                .chain([MouseEventKind::ScrollDown; 12])
            {
                app.mouse(MouseEvent {
                    kind,
                    column: 110,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                });
                t.draw(|f| app.draw(f)).unwrap();
                let scrolled: Vec<_> = (0..30).map(|y| cells(&t, y, 0..100)).collect();
                assert_eq!(
                    scrolled,
                    dashboard,
                    "scrolling the viewer must leave the dashboard in place; focus={focus:?}, \
                     scrollback={}",
                    app.viewers[0].viewer.screen().scrollback()
                );
            }
            assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        }
    }

    #[test]
    fn a_column_the_edge_cuts_through_is_left_out_whole() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let head = |app: &App, w: u16| -> String {
            app.row_lines(&app.rows, &app.visible, None, 0, 40, w)
                .iter()
                .map(ToString::to_string)
                .find(|l| l.contains("title"))
                .expect("the table names its columns")
        };
        let wide = head(&app, 400);
        let at = wide.find("age").expect("the age column is on by default");
        // The edge lands one character into the age column's name.
        let cut = (at + 2) as u16;
        assert_eq!(
            head(&app, cut).trim_end(),
            wide[..at].trim_end(),
            "the table ends on the last column that fits"
        );
        app.data.whole_columns = false;
        assert_eq!(
            head(&app, cut),
            wide,
            "without the setting the row keeps every column and the edge cuts through one"
        );
    }

    #[test]
    fn the_pane_takes_the_share_of_the_frame_its_ratio_names() {
        let d = dir();
        let mut app = app(d.path());
        let frame = Rect::new(0, 0, 200, 30);
        let [list, rule, pane] = app.split_areas(frame);
        assert_eq!((list.width, rule.width, pane.width), (100, 1, 99), "half");
        app.data.pane.ratio = 70;
        let [list, rule, pane] = app.split_areas(frame);
        assert_eq!((list.width, rule.width, pane.width), (60, 1, 139));
        app.data.pane.at = "bottom".into();
        let [top, rule, bottom] = app.split_areas(frame);
        assert_eq!((top.height, rule.height, bottom.height), (9, 1, 20));
        app.data.pane.ratio = 30;
        assert_eq!(app.split_areas(frame)[0].height, 21);
    }

    fn split_setup(
        width: u16,
    ) -> (
        tempfile::TempDir,
        App,
        Terminal<ratatui::backend::TestBackend>,
    ) {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        (d, app, t)
    }

    #[test]
    fn a_wide_frame_shows_the_selected_rows_viewer_beside_the_list_and_focus_only_moves_the_keys() {
        let (_d, mut app, mut t) = split_setup(200);
        let list = 100u16;
        let screen = rows(&t, 200);
        let left: Vec<String> = (0..30).map(|y| cells(&t, y, 0..list)).collect();
        assert!(
            left.iter().any(|r| r.contains("Type an instruction…")),
            "the composer is in the list column: {left:#?}"
        );
        assert!(
            left.iter().any(|r| r.contains(&A[..8])),
            "the session's row is in the list column: {left:#?}"
        );
        assert!(
            cells(&t, 0, list + 1..200).starts_with("VIEW"),
            "the pane starts right of the rule: {screen:#?}"
        );
        assert!(
            !screen.iter().any(|r| r.contains("tab back")),
            "unfocused, the row under the pane is clear: {screen:#?}"
        );
        // The viewer has the whole column, keys or no keys: its keys go in the list's hint row,
        // so taking them never resizes it.
        assert_eq!(
            app.viewers[0].viewer.screen().size(),
            (30, 200 - list - 1),
            "the viewer is sized to the pane"
        );
        assert_eq!(app.pane, Rect::new(list + 1, 0, 200 - list - 1, 30));
        let rule = t.backend().buffer().cell((list, 0)).unwrap().clone();
        assert_eq!(rule.symbol(), "│");
        assert_ne!(rule.fg, ORANGE, "the rule is dim while nothing is focused");
        assert!(
            !left[29].contains("ctrl+]") && !left[29].contains("ctrl+\\"),
            "unfocused, the hint line has no pane keys: {:?}",
            left[29]
        );

        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0));
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, None, "tab in the viewer comes back to the list");
        app.status.clear();
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        assert_eq!(
            app.viewers[0].viewer.screen().size(),
            (30, 200 - list - 1),
            "focusing beside the list does not resize the viewer"
        );
        t.draw(|f| app.draw(f)).unwrap();
        let hint = cells(&t, 29, 0..list);
        assert!(hint.contains("tab back"), "{hint:?}");
        assert!(hint.contains("ctrl+\\ full screen"), "{hint:?}");
        assert!(!hint.contains("ctrl+]"), "{hint:?}");
        assert!(
            cells(&t, 29, list + 1..200).trim().is_empty(),
            "the keys are in the list's hint row, not over the pane's last row: {:?}",
            cells(&t, 29, list + 1..200)
        );
        assert!(cells(&t, 0, list + 1..200).starts_with("VIEW"));
        assert_eq!(
            t.backend().buffer().cell((list, 5)).unwrap().fg,
            ORANGE,
            "the rule is orange while the viewer is focused"
        );
        let reversed = |t: &Terminal<ratatui::backend::TestBackend>| {
            let buf = t.backend().buffer();
            (0..30)
                .flat_map(|y| (0..list).map(move |x| (x, y)))
                .any(|pos| buf.cell(pos).unwrap().modifier.contains(Modifier::REVERSED))
        };
        assert!(
            !reversed(&t),
            "the composer draws no block cursor while the viewer has the terminal's"
        );
        app.filter = Input::new("one");
        assert!(
            app.hint_line()
                .to_string()
                .starts_with("filter: one  tab back"),
            "a kept filter stays on the focused hint line: {}",
            app.hint_line()
        );
        app.filter = Input::default();
        // The mouse is the pane's: a press on the list goes nowhere, a release there lands on
        // the pane's left edge, and the pane's origin is taken off what the viewer sees.
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        assert!(app.pane_mouse(ev(down, 10, 3)).is_none());
        assert!(
            app.pane_mouse(ev(MouseEventKind::ScrollUp, list, 3))
                .is_none()
        );
        assert_eq!(
            app.pane_mouse(ev(down, list + 1, 3))
                .map(|e| (e.column, e.row)),
            Some((list + 1, 3))
        );
        assert_eq!(
            app.pane_mouse(ev(MouseEventKind::Up(MouseButton::Left), 10, 40))
                .map(|e| (e.column, e.row)),
            Some((list + 1, 29)),
            "a release off the frame lands on the viewer's last row"
        );
        let inside = app.pane_mouse(ev(down, list + 1, 0)).unwrap();
        assert_eq!(
            viewer::encode_mouse(
                inside,
                (app.pane.x, app.pane.y),
                viewer::MouseProtocolMode::Press
            ),
            b"\x1b[<0;1;1M"
        );

        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert_eq!(app.focus, None);
        assert!(
            !app.needs_clear,
            "nothing left the frame, so ctrl+z beside the list does not clear it"
        );
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            cells(&t, 0, list + 1..200).starts_with("VIEW"),
            "the selected row's viewer stays on view after ctrl+z"
        );
        assert!(
            cells(&t, 29, 0..list).starts_with("enter "),
            "the key hints come back: {:?}",
            cells(&t, 29, 0..list)
        );
        assert!(
            reversed(&t),
            "unfocused, the composer's block cursor is back"
        );
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split);
        assert_eq!(app.focus, Some(0));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            cells(&t, 0, 0..200).starts_with("VIEW"),
            "the viewer has the frame"
        );
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 200));
        let strip = cells(&t, 29, 0..200);
        assert!(
            strip.trim_end().ends_with("tab back · ctrl+\\ split"),
            "{strip:?}"
        );
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(
            app.needs_clear,
            "a viewer that had the frame is cleared away"
        );
        assert!(!app.split, "the layout is kept across ctrl+z");
        app.needs_clear = false;
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            !cells(&t, 0, 0..200).contains("VIEW"),
            "in the full-frame layout the list has the frame to itself"
        );
        app.enter().unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 200));
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split);
        assert_eq!(app.focus, Some(0));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            cells(&t, 0, list + 1..200).starts_with("VIEW"),
            "beside the list again"
        );
        assert_eq!(app.viewers[0].viewer.screen().size(), (30, 200 - list - 1));
    }

    #[test]
    fn the_hint_line_drops_keys_from_its_end_to_fit_the_list_column() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.size = (30, 130);
        app.split = false;
        let wide = app.hint_line().to_string();
        assert!(wide.ends_with("shift+tab harness · esc quit"), "{wide}");
        let keys = |line: &str| line.split(" · ").map(str::to_owned).collect::<Vec<_>>();
        app.size = (30, 140);
        app.split = true;
        app.filter = Input::new("x".repeat(30));
        let fitted = app.hint_line();
        let fitted = Line::from(fitted.spans[1..].to_vec());
        assert!(fitted.width() <= 70, "{fitted}");
        let fitted = fitted.to_string();
        assert!(
            fitted.starts_with("enter attach · "),
            "the selected row's key stays: {fitted}"
        );
        assert!(fitted.ends_with(" · esc quit"), "quit stays: {fitted}");
        assert!(
            keys(&fitted).iter().all(|k| keys(&wide).contains(k)),
            "only whole keys go: {fitted}"
        );
        app.filter = Input::new("one");
        let filtered = app.hint_line();
        assert!(filtered.width() <= 70, "{filtered}");
        assert!(
            filtered
                .to_string()
                .starts_with("filter: one  enter attach")
        );
    }

    #[test]
    fn an_empty_pane_stays_blank_even_when_enter_can_open_a_viewer() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        // Enter can join this Codex thread, but the empty pane must not show a hint.
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
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        assert_eq!(key(&app).as_deref(), Some("dddd-daemon"));
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 160);
        assert!((0..30).all(|y| cells(&t, y, 81..160).trim().is_empty()));
        assert!(
            !screen[29].contains("ctrl+\\"),
            "the layout key is the viewer's, not the list's: {:?}",
            screen[29]
        );
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        t.draw(|f| app.draw(f)).unwrap();
        let pane = rows(&t, 160)
            .iter()
            .map(|r| r.chars().skip(81).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pane.contains("recent folders"), "{pane}");
        assert!(pane.contains("add folder › a row for a folder"), "{pane}");
    }

    #[test]
    fn a_menu_buttons_screen_is_in_the_pane_and_enter_gives_it_the_keys() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let pane = |t: &Terminal<ratatui::backend::TestBackend>| {
            (0..40)
                .map(|y| cells(t, y, 81..160))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let left = |t: &Terminal<ratatui::backend::TestBackend>| {
            (0..40)
                .map(|y| cells(t, y, 0..80))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            pane(&t).trim().is_empty(),
            "a session row with no viewer: blank"
        );
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        for _ in 0..3 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        assert!(app.menu_is("help"));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("move between rows"),
            "hover: {}",
            pane(&t)
        );
        assert!(pane(&t).contains("guide › the keys"), "{}", pane(&t));
        assert!(left(&t).contains(&A[..8]), "the list stays: {}", left(&t));
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(0)));
        assert!(app.split_active());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(pane(&t).contains("move between rows"), "{}", pane(&t));
        assert!(left(&t).contains(&A[..8]), "{}", left(&t));
        assert!(
            !left(&t).contains("Type an instruction…"),
            "the list is on a button, which takes no instruction: {}",
            left(&t)
        );
        assert!(!app.key(KeyCode::Down, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(1)));
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Normal), "ctrl+z leaves the guide");
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("move between rows"),
            "still picked: {}",
            pane(&t)
        );
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(matches!(app.mode, Mode::Guide(0)));
        assert!(!app.split_active(), "shift+enter takes the frame");
        t.draw(|f| app.draw(f)).unwrap();
        assert!(!left(&t).contains(&A[..8]), "{}", left(&t));
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Normal) && app.split_active());
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(!app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Normal) && app.split_active());
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert!(app.split_active(), "full ends with the screen");
        assert!(app.menu_is("folder"));
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert!(app.menu_is("jobs"));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(pane(&t).contains("new job"), "hover: {}", pane(&t));
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(app.jobs_view);
        assert!(app.on_new_job());
        assert!(
            !app.rows.iter().any(|r| r.kind == Kind::Menu),
            "the jobs screen in the pane has no menu row"
        );
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("▌ "),
            "the cursor is in the pane: {}",
            pane(&t)
        );
        assert!(left(&t).contains(&A[..8]), "{}", left(&t));
        assert!(
            left(&t).contains("jobs   config   help   the jobs: start"),
            "{}",
            left(&t)
        );
        assert!(
            (0..40).any(|y| cells(&t, y, 81..160).contains("new job")),
            "{}",
            pane(&t)
        );
        assert!(!app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        assert!(!app.jobs_view);
        assert!(app.menu_is("jobs"), "esc lands on the menu row");
        app.split = false;
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(app.rows.iter().any(|r| r.kind == Kind::Menu));
        assert!(rows(&t, 120).join("\n").contains("new job"));
    }

    #[test]
    fn right_on_a_row_with_nothing_typed_opens_the_pane_then_reaches_into_it() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Session(..))) {
            app.step(1);
        }
        app.split = false;
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert!(app.split, "the first right opens the pane");
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert!(app.split, "and the next one reaches into it");
        assert_eq!(app.status, "nothing in the pane", "as tab would");
        app.text = "hi".into();
        app.caret = 0;
        app.status.clear();
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert_eq!(
            app.caret, 1,
            "with something typed right still moves the caret"
        );
    }

    #[test]
    fn tab_gives_a_buttons_screen_in_the_pane_the_keys() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        for _ in 0..2 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        assert!(app.menu_is("config") && app.panel_shown());
        t.draw(|f| app.draw(f)).unwrap();
        let hint = cells(&t, 39, 0..80);
        assert!(
            hint.contains("tab pane"),
            "the list offers the pane: {hint:?}"
        );
        assert_ne!(
            t.backend().buffer().cell((80, 0)).unwrap().fg,
            ORANGE,
            "picked only: the rule is dim"
        );
        assert!(!app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
        assert!(
            matches!(app.mode, Mode::Config(_)),
            "tab opens the editor in the pane"
        );
        assert!(app.panel_focused() && app.split_active() && !app.panel_shown());
        t.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            t.backend().buffer().cell((80, 0)).unwrap().fg,
            ORANGE,
            "the rule says the pane has the keys"
        );
        let under = cells(&t, 39, 81..160);
        assert!(
            under.contains("esc done"),
            "the editor's keys are under the pane: {under:?}"
        );
        assert!(
            cells(&t, 39, 0..80).trim().is_empty(),
            "and not in the list's row too: {:?}",
            cells(&t, 39, 0..80)
        );
        let rules = |x: std::ops::Range<u16>| {
            (3..40u16)
                .filter(|&y| {
                    cells(&t, y, x.clone())
                        .chars()
                        .filter(|&c| c == '─')
                        .count()
                        > 40
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rules(0..80),
            rules(81..160),
            "the prompt boxes sit on the same rows"
        );
        assert!(!app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
        assert!(
            matches!(app.mode, Mode::Normal),
            "tab bounces back out of the editor as it bounced in"
        );
        assert_eq!(app.status, "back to the list", "and says so");
        assert!(!app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        let Mode::Config(form) = &app.mode else {
            panic!("the editor is open on a field");
        };
        assert!(form.open);
        assert!(!app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
        assert!(
            matches!(&app.mode, Mode::Config(f) if f.open),
            "tab in an open field is the form's own"
        );
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Normal), "ctrl+z comes back out");
    }

    #[test]
    fn ctrl_backslash_moves_a_buttons_screen_off_the_pane_and_back() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        while !app.menu_is("config") {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char('9'), KeyModifiers::NONE).unwrap();
        let typed = match &app.mode {
            Mode::Config(f) => f.values[f.row].clone(),
            _ => panic!("the editor is open on a field"),
        };
        assert!(app.split_active());
        for on in [false, true] {
            app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap();
            assert_eq!(app.split_active(), on, "the pane goes off and comes back");
            assert!(
                matches!(&app.mode, Mode::Config(f) if f.open && f.values[f.row] == typed),
                "and the editor keeps the keys and what is typed in it"
            );
        }
    }

    #[test]
    fn ctrl_o_is_not_a_dashboard_key() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.key(KeyCode::Char('o'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(app.mode, Mode::Normal), "the list keeps the key");
        assert!(app.text.is_empty(), "a ctrl key types nothing");
        assert!(
            !GUIDE.iter().any(|(k, _)| *k == "ctrl+o"),
            "no key the guide leaves out does anything"
        );
    }
    #[test]
    fn the_config_button_edits_the_defaults_block() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        for _ in 0..2 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        assert!(app.menu_is("config"));
        assert_eq!(app.enter_label(), "defaults");
        app.enter().unwrap();
        assert!(matches!(app.mode, Mode::Config(_)));
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 60)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("chart scale"), "{s}");
        assert!(
            s.contains("enter opens it") && !s.contains("time limit (min)"),
            "the runs section comes up shut: {s}"
        );
        let column = |s: &str, what: &str| {
            s.lines()
                .find(|l| l.contains(what))
                .and_then(|l| l.find(what).map(|b| l[..b].chars().count()))
                .unwrap_or_else(|| panic!("{what}: {s}"))
        };
        let height = |s: &str| {
            let mut it = s.lines().skip_while(|l| !l.contains("chart scale"));
            it.next();
            it.take_while(|l| !l.contains("›")).count()
        };
        let (col, tall) = (column(&s, "chart scale"), height(&s));
        assert_eq!(column(&s, "count per bar"), col, "{s}");
        assert_eq!(column(&s, "alias or model id"), col, "{s}");
        let go = |app: &mut App, name: &str| {
            while let Mode::Config(f) = &app.mode
                && (f.row != field_at(name) || f.on_head)
            {
                let want = field_at(name);
                // On the section's head → opens it and goes in, ↑ leaves it for the row above.
                let code = if f.on_head {
                    if f.row > want {
                        KeyCode::Up
                    } else {
                        KeyCode::Right
                    }
                } else if f.row < want {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                app.key(code, KeyModifiers::NONE).unwrap();
            }
        };
        go(&mut app, "activity.metric");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(column(&s, "count per bar"), col, "no bounce: {s}");
        assert_eq!(height(&s), tall, "no bounce: {s}");
        assert!(
            s.contains("count per bar        [lines] messages  tools  tokens"),
            "every word the field takes is on its row, the built-in bracketed: {s}"
        );
        assert!(
            s.contains("activity.metric › default: lines"),
            "the prompt line names the key and the built-in, not the words: {s}"
        );
        assert!(
            s.contains("← → change"),
            "the key that changes a value is always up: {s}"
        );
        assert!(s.contains("esc done"), "{s}");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open && f.values[f.row] == "messages"));
        t.draw(|f| app.draw(f)).unwrap();
        let s = rows(&t, 160).join("\n");
        assert!(
            s.contains("count per bar         lines [messages] tools  tokens"),
            "the built-in keeps its word while another is picked: {s}"
        );
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open && f.values[f.row].is_empty()));
        go(&mut app, "activity.bound");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        let row_of = |s: &str, label: &str| {
            s.lines()
                .find(|l| l.contains(label))
                .unwrap_or_else(|| panic!("{label}: {s}"))
                .to_owned()
        };
        assert!(s.contains("chart scale          [fleet] row  log"), "{s}");
        assert!(
            s.contains("activity.bound › default: fleet"),
            "a field with only its own words says nothing about typing: {s}"
        );
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row] == "row"));
        go(&mut app, "activity.bucket");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("activity.bucket › or type a duration · enter next · default: 1m"),
            "a field that also takes something typed says so, and only there: {s}"
        );
        assert!(
            row_of(&s, "time per bar").contains("[ a duration"),
            "an empty slot follows the words, naming what it takes: {s}"
        );
        for c in "10m".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        let bucket = row_of(&s, "time per bar");
        assert!(
            bucket.contains("30s") && bucket.contains("[ 10m"),
            "typing goes into the slot past the words: {s}"
        );
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Config(f)
            if !f.open && f.values[field_at("activity.bucket")] == "10m" && f.row == field_at("activity.bucket") + 1),
            "enter keeps the slot's value and goes on to the next setting"
        );
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        let bucket = row_of(&s, "time per bar");
        assert!(
            bucket.contains("30s") && bucket.contains("[ 10m") && !bucket.contains("[1m]"),
            "a value typed in stays in the slot, with no word picked: {bucket}"
        );
        go(&mut app, "model");
        for c in "claude-opus-5".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Config(f) if !f.open && f.values[field_at("model")] == "claude-opus-5")
        );
        go(&mut app, "write");
        app.key(KeyCode::Char('t'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row] == "true"));
        app.key(KeyCode::Char('-'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row].is_empty()));
        go(&mut app, "codex_full_access");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 82..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("no sandbox"), "{s}");
        assert!(s.contains("alias or model id"), "{s}");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        app.enter().unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 82..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("Seconds an armed ctrl+x waits"),
            "the selected field, the first one, is explained: {s}"
        );
        assert!(!s.contains("Maximum cost"), "only the selected one: {s}");
        assert!(
            s.matches("default").count() >= 3,
            "a field left to its built-in reads default in the control's place: {s}"
        );
        let at = |what: &str| s.find(what).unwrap_or_else(|| panic!("{what}: {s}"));
        assert!(
            at("\ncones  the dashboard itself") < at("    ctrl+x armed (s)")
                && at("    ctrl+x armed (s)") < at("\n  start")
                && at("\n  start") < at("    composer starts on")
                && at("    composer starts on") < at("\n  pane")
                && at("\n  pane") < at("    pane side")
                && at("    pane side") < at("\n  activity")
                && at("\n  activity") < at("    bar count")
                && at("    bar count") < at("\nharnesses  how claude and codex are run")
                && at("\nharnesses  how claude and codex are run") < at("    run on Bedrock")
                && at("    run on Bedrock") < at("\n  claude")
                && at("\n  claude") < at("    alias or model id")
                && at("    alias or model id") < at("\n  codex ")
                && at("\n  codex ") < at("    model id")
                && at("    model id") < at("\nruns  what a run starts with"),
            "the fields sit under their groups and blocks: {s}"
        );
        go(&mut app, "timeout_min");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 82..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        let at = |what: &str| s.find(what).unwrap_or_else(|| panic!("{what}: {s}"));
        assert!(
            at("\nruns  what a run starts with") < at("    time limit (min)")
                && at("    time limit (min)") < at("    import env vars"),
            "walking into the shut section opens it, fields and all: {s}"
        );
        assert!(
            s.lines()
                .skip_while(|l| !l.contains("runs  what a run starts with"))
                .nth(1)
                .is_some_and(|l| l.contains("harness")),
            "the harness a run takes is the section's first row: {s}"
        );
        assert!(s.contains("‹ 30 ›"), "built-ins show dim: {s}");
        app.key(KeyCode::Char('9'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row].is_empty()));

        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        for c in "abc".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        match &app.mode {
            Mode::Config(f) => {
                assert!(f.open);
                assert!(
                    f.error
                        .as_deref()
                        .unwrap()
                        .starts_with("timeout_min: a number"),
                    "{:?}",
                    f.error
                )
            }
            _ => panic!("stays open"),
        }
        assert_eq!(
            config::defaults(&app.jobs_path).timeout_min,
            None,
            "a field held open by its own error writes nothing"
        );
        for _ in 0..3 {
            app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Char('5'), KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open));
        assert!(app.status.starts_with("config saved"), "{}", app.status);
        assert_eq!(config::defaults(&app.jobs_path).timeout_min, Some(5.0));
        assert!(
            matches!(&app.mode, Mode::Config(f) if f.row == field_at("write")),
            "enter on a typed value saves it and goes on to the next setting"
        );
        go(&mut app, "overlap");
        go(&mut app, "write");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("allow file changes   [false] true"),
            "picks on write: {s}"
        );
        assert!(s.contains("← → change"), "{s}");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        go(&mut app, "model");
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row].is_empty()));
        for _ in 0..3 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }

        assert_eq!(config::file_columns(&app.jobs_path), None);
        let saved = config::defaults(&app.jobs_path);
        assert_eq!((saved.timeout_min, saved.write), (Some(5.0), Some(true)));
        assert_eq!(
            saved.model.as_deref(),
            Some("opus[1m]"),
            "the million-token window is a word of its own"
        );
        assert_eq!(
            saved.overlap, None,
            "empty leaves the built-in out of the file"
        );

        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal), "{}", app.status);
        assert!(
            matches!(app.selected().map(|r| &r.kind), Some(Kind::Session(id, _)) if id == A),
            "esc lands on the first session, not the button it came from"
        );
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        assert!(app.menu_is("config"));
        app.enter().unwrap();
        match &app.mode {
            Mode::Config(f) => assert_eq!(
                ["timeout_min", "write", "model"].map(|n| f.values[field_at(n)].as_str()),
                ["5", "true", "opus[1m]"]
            ),
            _ => panic!(),
        }
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn the_menu_row_picks_a_button_with_left_and_right() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 30)).unwrap();
        let screen = |app: &mut App, t: &mut Terminal<ratatui::backend::TestBackend>| {
            t.draw(|f| app.draw(f)).unwrap();
            rows(t, 160).join("\n")
        };
        let s = screen(&mut app, &mut t);
        assert!(s.contains(" folder   jobs   config   help "), "{s}");
        assert!(
            s.contains("a row for a folder") && !s.contains("start, edit, add one"),
            "{s}"
        );
        assert!(s.contains("← → pick"), "{s}");
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        let s = screen(&mut app, &mut t);
        assert!(
            s.contains("start, edit, add one") && !s.contains("a row for a folder"),
            "{s}"
        );
        assert_eq!(app.enter_label(), "jobs");
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        assert_eq!(MENU[app.menu].0, "help");
        app.text = "x".into();
        app.caret = 1;
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        assert_eq!((MENU[app.menu].0, app.caret), ("help", 0));
        app.text.clear();
        app.enter().unwrap();
        assert!(matches!(app.mode, Mode::Guide(0)));
        app.mode = Mode::Normal;
        app.step(1);
        let s = screen(&mut app, &mut t);
        assert!(!s.contains("keys and what they do"), "{s}");
    }

    #[test]
    fn ctrl_backslash_toggles_the_split_and_never_reaches_the_viewer() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.viewers.push(silent_open("run:r1"));
        app.focus(0);
        app.size = (30, 200);
        assert!(app.split, "on by default");
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split);
        assert_eq!(app.focus, Some(0));
        // crossterm reports the byte 0x1c a terminal sends for ctrl+\ as ctrl+4.
        assert!(!app.key(KeyCode::Char('4'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split);
        assert_eq!(app.focus, Some(0));
        assert!(app.text.is_empty(), "nothing typed into the composer");
        app.unfocus();
        app.status.clear();
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split, "the pane is off");
        assert_eq!(app.focus, None);
        assert!(!app.key(KeyCode::Char('4'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split, "and on again");
        assert!(app.text.is_empty(), "nothing typed into the composer");
        app.focus(0);
        app.size = (30, 80);
        app.needs_clear = false;
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split, "a narrow frame toggles too");
        assert!(app.needs_clear);
    }

    #[test]
    fn the_columns_row_arranges_the_table_and_keeps_the_order_in_jobs_yaml() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "active", 1);
        let jobs = d.path().join("none.yaml");
        fs::write(
            &jobs,
            "version: 1\ncolumns: [state, model, context]\nconfirm_secs: 3\njobs: []\n",
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let names = |app: &App| {
            app.rows
                .iter()
                .find(|r| r.cells.iter().any(|c| c.0.trim() == "title"))
                .map(|r| {
                    r.cells
                        .iter()
                        .map(|c| c.0.trim().to_owned())
                        .filter(|c| !c.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        assert_eq!(names(&app), ["state", "title", "model", "context"]);
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        for _ in 0..2 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        app.enter().unwrap();
        while let Mode::Config(f) = &app.mode
            && f.row != field_at("columns")
        {
            app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char(']'), KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["state", "title", "context", "model"]);
        assert_eq!(config::columns(&jobs), ["state", "context", "model"]);
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["state", "title", "context"]);
        let text = fs::read_to_string(&jobs).unwrap();
        assert!(
            text.contains("columns: [state, context]") && text.contains("confirm_secs: 3"),
            "{text}"
        );
    }

    #[test]
    fn the_pane_block_sets_the_opening_layout_and_its_side() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let jobs = d.path().join("none.yaml");
        std::fs::write(
            &jobs,
            "version: 1\ncolumns: [state, model]\nstart:\n  pane: false\npane:\n  at: bottom\njobs: []\n",
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert!(!app.split, "start.pane: false opens with the list alone");
        let names = |app: &App| {
            app.rows
                .iter()
                .find(|r| r.cells.iter().any(|c| c.0.trim() == "title"))
                .map(|r| {
                    r.cells
                        .iter()
                        .map(|c| c.0.trim().to_owned())
                        .filter(|c| !c.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        assert_eq!(names(&app), ["state", "title", "model"]);
        app.size = (30, 80);
        app.toggle_split();
        assert!(app.split);
        assert_eq!(
            names(&app),
            ["state", "title", "model"],
            "the same columns in the same places with the pane on; the width cuts the rest"
        );
        let [list, rule, pane] = app.split_areas(Rect::new(0, 0, 80, 30));
        assert_eq!((list.height, list.width), (15, 80));
        assert_eq!((rule.y, rule.height, rule.width), (15, 1, 80));
        assert_eq!((pane.y, pane.height, pane.width), (16, 14, 80));
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert_eq!(cells(&t, 15, 0..80), "─".repeat(80), "a horizontal rule");
        assert!(
            (16..30).any(|y| cells(&t, y, 0..80).contains("VIEW")),
            "the viewer is under the rule"
        );
        app.data.pane.at = "right".into();
        let [list, rule, pane] = app.split_areas(Rect::new(0, 0, 80, 30));
        assert_eq!((list.width, rule.x, pane.x, pane.width), (40, 40, 41, 39));
    }

    #[test]
    fn the_wheel_over_the_pane_scrolls_its_viewer_back_without_focusing_it() {
        let (_d, mut app, mut t) = split_setup(200);
        let list = 100u16;
        // A viewer that reads no mouse and has left forty lines above its screen.
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            "i=0; while [ $i -lt 40 ]; do i=$((i+1)); echo line$i; done; printf 'END'; sleep 5",
        ]);
        app.viewers[0].viewer = Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.viewers[0].viewer.screen().contents().contains("END") {
            app.pump();
            assert!(Instant::now() < deadline, "the viewer never drew END");
            std::thread::sleep(Duration::from_millis(5));
        }
        let wheel = |kind, modifiers| MouseEvent {
            kind,
            column: list + 5,
            row: 3,
            modifiers,
        };
        app.mouse(wheel(MouseEventKind::ScrollUp, KeyModifiers::NONE));
        assert_eq!(app.focus, None, "the wheel does not focus the pane");
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 3);
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE));
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        app.focus(0);
        assert!(!app.key(KeyCode::PageUp, KeyModifiers::SHIFT).unwrap());
        // A page is 29 rows but only eleven lines have left a 30-row pane.
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 11);
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            cells(&t, 0, list + 1..list + 6) == "line1",
            "the pane shows the lines that had left: {:?}",
            cells(&t, 0, list + 1..200)
        );
        assert!(!app.key(KeyCode::Char('a'), KeyModifiers::NONE).unwrap());
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        // Full-frame and narrow viewers need wheel reports too, even when the client
        // leaves scrolling to the terminal.
        app.toggle_split();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(app.wants_mouse(), "a full-frame viewer needs wheel reports");
        app.mouse(wheel(MouseEventKind::ScrollUp, KeyModifiers::NONE));
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 3);
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE));
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        t.resize(Rect::new(0, 0, 80, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(app.wants_mouse(), "a narrow viewer needs wheel reports");
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 3);
        app.unfocus();
        assert!(!app.wants_mouse(), "the list alone releases the mouse");
    }

    #[test]
    fn a_click_focuses_the_pane_and_a_click_on_a_row_takes_the_keys_back() {
        let (_d, mut app, mut t) = split_setup(200);
        let list = 100u16;
        assert!(app.wants_mouse(), "beside the list the mouse is read");
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(click(list + 5, 3));
        assert_eq!(app.focus, Some(0));
        assert_eq!(app.viewers[0].viewer.screen().size(), (30, 200 - list - 1));
        app.mouse(click(list + 5, 3));
        assert_eq!(app.focus, Some(0));
        assert!(app.split);
        app.toggle_split();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 0..200).starts_with("VIEW"));
        app.mouse(click(10, 5));
        assert_eq!(app.focus, Some(0));
        app.toggle_split();
        t.draw(|f| app.draw(f)).unwrap();
        let n = app
            .visible
            .iter()
            .position(|&i| app.rows[i].kind.key() == Some(A))
            .unwrap();
        let row = app.list_area.y + (n - app.scroll) as u16;
        app.cursor = 0;
        app.mouse(click(10, row));
        assert_eq!(app.focus, None);
        assert_eq!(key(&app).as_deref(), Some(A));
        app.mouse(click(10, row));
        assert_eq!(app.focus, None);
        assert_eq!(key(&app).as_deref(), Some(A));
        app.mouse(click(10, 29));
        assert_eq!(key(&app).as_deref(), Some(A));
        assert_eq!(app.focus, None);
        app.mode = Mode::Config(app.config_form());
        app.mouse(click(list + 5, 5));
        assert!(
            matches!(app.mode, Mode::Config(_)),
            "a click inside the editor stays in it"
        );
        app.mouse(click(10, row));
        assert!(
            matches!(app.mode, Mode::Normal),
            "a click on the list takes the keys back from the editor"
        );
        assert_eq!(key(&app).as_deref(), Some(A));
        app.split = false;
        assert!(
            !app.wants_mouse(),
            "with the pane off and no focused viewer none is read"
        );
        assert_eq!(app.rest_for(), REST);
        app.split = true;
        assert_eq!(app.rest_for(), REST_SPLIT);
    }

    #[test]
    fn ctrl_n_renames_the_selected_claude_session_where_the_resume_picker_reads_it() {
        let d = dir();
        registry_kind(d.path(), A, "/src/one", "idle", 1, "interactive");
        let dir = d.path().join("projects").join("-src-one");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("{A}.jsonl"));
        fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Old title\"}\n",
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.key(KeyCode::Char('n'), KeyModifiers::CONTROL).unwrap();
        assert!(
            matches!(&app.mode, Mode::Rename(i) if i.text == "Old title"),
            "the prompt opens on the current title"
        );
        app.key(KeyCode::Char('u'), KeyModifiers::CONTROL).unwrap();
        for c in "Ship it".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status, "renamed to Ship it");
        let text = fs::read_to_string(&transcript).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "custom-title");
        assert_eq!(last["customTitle"], "Ship it");
        assert_eq!(last["sessionId"], A);
        app.refresh().unwrap();
        assert_eq!(
            app.selected_session().and_then(|s| s.title.as_deref()),
            Some("Ship it"),
            "the row shows the new title on the next read"
        );
        app.key(KeyCode::Char('n'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(&app.mode, Mode::Rename(i) if i.text == "Ship it"));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read_to_string(&transcript).unwrap(), text);
    }

    #[test]
    fn the_pane_stays_blank_for_a_session_in_its_own_terminal() {
        let d = dir();
        registry_kind(d.path(), A, "/src/one", "idle", 1, "interactive");
        let dir = d.path().join("projects").join("-src-one");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("{A}.jsonl"));
        let line = |role: &str, text: &str| {
            serde_json::json!({"type": role, "message": {"content": [{"type": "text", "text": text}]}})
                .to_string()
        };
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n{}\n",
                line("user", "fix it"),
                line("assistant", "first reply"),
                line("assistant", "second reply")
            ),
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!((0..30).all(|y| cells(&t, y, 101..200).trim().is_empty()));
        // Growing the transcript must not turn the pane into a transcript reader.
        fs::write(
            &transcript,
            format!("{}\n", line("assistant", "third reply")),
        )
        .unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!((0..30).all(|y| cells(&t, y, 101..200).trim().is_empty()));
    }

    #[test]
    fn the_pane_is_blank_until_a_coming_viewer_paints() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let dir = d.path().join("projects").join("-src-one");
        fs::create_dir_all(&dir).unwrap();
        let line = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "a reply"}]}});
        fs::write(
            dir.join(format!("{A}.jsonl")),
            format!(
                "{line}
"
            ),
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        let blank = |t: &Terminal<ratatui::backend::TestBackend>| {
            (0..30).all(|y| cells(t, y, 101..200).trim().is_empty())
        };
        t.draw(|f| app.draw(f)).unwrap();
        assert!(blank(&t));
        app.viewers.push(speculative_open(A));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(blank(&t));
        assert_eq!(app.viewers[0].viewer.screen().size(), (30, 99));
        app.viewers.clear();
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("VIEW"));
    }

    #[test]
    fn a_session_without_a_viewer_clears_the_previous_live_screen() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 2);
        registry_kind(d.path(), B, "/src/two", "idle", 1, "interactive");
        let dir = d.path().join("projects").join("-src-two");
        fs::create_dir_all(&dir).unwrap();
        let line = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "two's reply"}]}});
        fs::write(dir.join(format!("{B}.jsonl")), format!("{line}\n")).unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("VIEW"));
        // B has a transcript, but no live viewer. The previous screen must clear.
        app.step(1);
        assert_eq!(key(&app).as_deref(), Some(B));
        t.draw(|f| app.draw(f)).unwrap();
        assert!((0..30).all(|y| cells(&t, y, 101..200).trim().is_empty()));
        app.step(-1);
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("VIEW"));
    }

    #[test]
    fn a_codex_pane_stays_blank_with_or_without_a_transcript_until_its_viewer_paints() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 2);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let mut data = Data::load(&d.path().join("jobs.yaml"), d.path(), d.path()).unwrap();
        data.sessions.push(Session {
            session_id: "codex-77".into(),
            harness: "codex".into(),
            kind: None,
            cwd: PathBuf::from("/src/one"),
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
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        app.step(-1);
        assert_eq!(key(&app).as_deref(), Some(A));
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("VIEW"));
        app.step(1);
        assert!(matches!(&app.selected().unwrap().kind, Kind::Session(id, _) if id == "codex-77"));
        assert_eq!(app.shown(), None);
        t.draw(|f| app.draw(f)).unwrap();
        assert!((0..30).all(|y| cells(&t, y, 101..200).trim().is_empty()));
        let transcript = d.path().join("codex-rollout.jsonl");
        let line = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "a Codex transcript reply"}]
            }
        });
        fs::write(&transcript, format!("{line}\n")).unwrap();
        app.data
            .sessions
            .iter_mut()
            .find(|s| s.session_id == "codex-77")
            .unwrap()
            .transcript_path = Some(transcript);
        t.draw(|f| app.draw(f)).unwrap();
        assert!((0..30).all(|y| cells(&t, y, 101..200).trim().is_empty()));
        app.viewers
            .push(viewer_open("codex-77", "codex", "CODEX LIVE"));
        wait_paint(&mut app, 1, "CODEX LIVE");
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("CODEX LIVE"));
    }

    #[test]
    fn speculative_viewers_pool_up_to_the_limit_of_their_own_and_the_oldest_goes() {
        let d = dir();
        let mut app = app(d.path());
        // Three live viewers take nothing from the speculative pool.
        for k in ["l1", "l2", "l3"] {
            app.viewers.push(silent_open(k));
            app.viewers.last_mut().unwrap().last_focused = Instant::now() - Duration::from_secs(60);
        }
        for k in ["s1", "s2", "s3"] {
            app.viewers.push(speculative_open(k));
            app.viewers.last_mut().unwrap().last_focused = Instant::now();
            app.pool_speculative();
        }
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        // Live viewers are never the ones to go; the oldest speculative did.
        assert_eq!(keys, vec!["l1", "l2", "l3", "s2", "s3"]);
        assert_eq!(app.viewers.len() - app.live_viewers(), SPECULATIVE_VIEWERS);
    }

    #[test]
    fn with_the_pane_off_the_viewer_is_full_screen_over_the_strip() {
        let (_d, mut app, mut t) = split_setup(120);
        app.split = false;
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 120);
        assert!(
            !screen.iter().any(|r| r.contains("VIEW")),
            "unfocused with the pane off the list is alone: {screen:#?}"
        );
        assert!(screen.iter().any(|r| r.contains("Type an instruction…")));
        assert_eq!(app.pane, Rect::new(0, 0, 120, 29));
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 120);
        assert!(screen[0].starts_with("VIEW"), "{screen:#?}");
        assert!(
            screen[29].trim_end().ends_with("tab back · ctrl+\\ split"),
            "the strip: {:?}",
            screen[29]
        );
        assert!(!screen.iter().any(|r| r.contains("Type an instruction…")));
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 120));
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

    /// A viewer that writes nothing: `/bin/sleep` on a pty. These tests never look at a
    /// viewer's screen, so a shell that draws would only add to the close.
    fn silent_open(key: &str) -> Open {
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        Open {
            key: key.into(),
            what: "attach".into(),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: false,
        }
    }

    fn speculative_open(key: &str) -> Open {
        let mut open = silent_open(key);
        open.speculative = true;
        open
    }

    fn rested(app: &mut App, key: &str, age: Duration) {
        app.rest = Some((key.into(), Instant::now() - age));
    }

    const OLD: Duration = Duration::from_millis(500);

    #[test]
    fn a_rested_claude_session_row_is_the_prespawn_target_and_nothing_else_is() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.split = false;
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        rested(&mut app, A, OLD);
        assert_eq!(
            app.prespawn_target(),
            Some((A.to_owned(), PathBuf::from("/src/one")))
        );
        rested(&mut app, A, Duration::from_millis(100));
        assert_eq!(app.prespawn_target(), None, "the cursor has not rested yet");
        rested(&mut app, A, OLD);
        app.prespawned = Some(A.into());
        assert_eq!(app.prespawn_target(), None, "tried once during this rest");
        app.prespawned = None;
        app.mode = Mode::Filter;
        assert_eq!(app.prespawn_target(), None, "not while the filter is typed");
        app.mode = Mode::Normal;
        app.text = "an instruction".into();
        assert_eq!(app.prespawn_target(), None, "enter would start a session");
        app.text = "  ".into();
        assert!(
            app.prespawn_target().is_some(),
            "blank text is what enter treats as empty"
        );
        app.text.clear();
        let state = std::mem::replace(&mut app.data.sessions[0].state, "done".into());
        assert!(
            app.prespawn_target().is_some(),
            "a job whose prompt is done is a live worker that enter attaches"
        );
        app.data.sessions[0].state = "stopped".into();
        assert_eq!(app.prespawn_target(), None, "a stopped job has no worker");
        app.data.sessions[0].state = state;
        app.viewers.push(silent_open("run:r1"));
        app.focus = Some(0);
        assert_eq!(
            app.prespawn_target(),
            None,
            "not while a viewer has the frame"
        );
        app.focus = None;
        app.viewers.clear();
        let (_tx, rx) = mpsc::channel();
        app.opening = Some(Opening {
            what: "codex".into(),
            key: B.into(),
            command: rx,
            record: None,
            prompt: None,
        });
        assert_eq!(
            app.prespawn_target(),
            None,
            "not while a viewer is prepared"
        );
        app.opening = None;
        app.viewers.push(silent_open(A));
        assert_eq!(app.prespawn_target(), None, "its viewer is already alive");
        app.viewers.clear();
        assert!(app.prespawn_target().is_some(), "the policy is back to yes");
        let (_tx, rx) = mpsc::channel();
        app.stopping.push(PendingStop {
            id: A.into(),
            label: "one".into(),
            verb: "delete",
            result: rx,
        });
        assert_eq!(app.prespawn_target(), None, "not while it is being stopped");
        app.stopping.clear();
        app.removed_sessions.insert(A.into());
        assert_eq!(app.prespawn_target(), None, "not once it was removed");
    }

    /// An interactive Claude runs in its own terminal, which `claude attach` refuses.
    #[test]
    fn an_own_terminal_row_is_never_a_prespawn_target() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        rested(&mut app, A, OLD);
        assert_eq!(app.prespawn_target(), None, "own terminal");
    }

    /// A finished run's attach resumes it, so it is never opened ahead of time.
    #[test]
    fn a_run_row_is_never_a_prespawn_target() {
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
        assert!(matches!(&app.selected().unwrap().kind, Kind::Run(id, _) if id == A));
        app.track_rest();
        assert_eq!(
            app.rest.as_ref().map(|(k, _)| k.as_str()),
            Some("run:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        );
        rested(&mut app, &format!("run:{A}"), OLD);
        assert_eq!(app.prespawn_target(), None, "a run row is never a target");
    }

    #[test]
    fn a_viewer_left_in_the_agent_view_is_dropped_and_a_session_is_kept() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let titled = |app: &mut App, title: &str| {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", &format!("printf '\\x1b]2;{title}\\x07'; sleep 5")]);
            let mut open = silent_open(A);
            open.viewer = Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap();
            app.viewers.push(open);
            let i = app.viewers.len() - 1;
            app.focus = Some(i);
            let deadline = Instant::now() + Duration::from_secs(3);
            while app.viewers[i].viewer.title().is_none() {
                app.pump();
                assert!(Instant::now() < deadline, "the viewer set no title");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        titled(&mut app, AGENT_VIEW_TITLE);
        app.unfocus();
        assert!(app.viewers.is_empty(), "the agent view outlived leaving it");
        assert!(app.status.is_empty(), "{}", app.status);
        titled(&mut app, "◑ a session");
        app.unfocus();
        assert_eq!(app.viewers.len(), 1, "a session's client stays alive");
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn a_speculative_viewer_neither_counts_toward_the_limit_nor_gets_evicted() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        for k in ["one", "two", "three"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        app.viewers.push(speculative_open(A));
        app.viewers[0].last_focused = Instant::now() - Duration::from_secs(60);
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        app.open((12, 80), c, "attach", "four".into(), None);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["two", "three", A, "four"], "{keys:?}");
        assert!(app.viewers[2].speculative, "the speculative one survived");
        assert_eq!(app.live_viewers(), MAX_FOCUSED_VIEWERS);
        assert_eq!(app.focus, Some(3));
    }

    /// `MAX_FOCUSED_VIEWERS` bounds the focused pool alone, so the count that matters for the
    /// machine is the sum of the two constants. The five is a literal rather than a sum of them on
    /// purpose: raising either constant adds a whole harness client, and this is what says so.
    #[test]
    fn the_focused_cap_plus_the_prespawned_pool_is_the_real_client_ceiling() {
        assert_eq!(
            MAX_FOCUSED_VIEWERS + SPECULATIVE_VIEWERS,
            5,
            "harness clients one dashboard runs, each a process at roughly 165MB"
        );
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        for k in ["one", "two", "three"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        for k in [A, B] {
            app.viewers.push(speculative_open(k));
        }
        assert_eq!(app.viewers.len(), 5, "three focused and two prespawned");
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        app.open((12, 80), c, "attach", "four".into(), None);
        assert_eq!(
            app.live_viewers(),
            MAX_FOCUSED_VIEWERS,
            "the focused pool held its cap"
        );
        assert_eq!(
            app.viewers.len(),
            5,
            "a fourth focused viewer evicted one instead of raising the ceiling"
        );
    }

    #[test]
    fn a_codex_client_is_never_evicted_for_a_fourth_viewer() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let mut codex = silent_open("codex-1");
        codex.what = "codex".into();
        codex.last_focused = Instant::now() - Duration::from_secs(60);
        app.viewers.push(codex);
        for k in ["one", "two"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        app.open((12, 80), c, "attach", "four".into(), None);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["codex-1", "two", "four"], "{keys:?}");
        // Two more Codex clients take the attaches' places, then an attach with no attach left
        // to close opens as a fourth live viewer.
        for k in ["codex-2", "codex-3", "five"] {
            let mut c = Command::new("/bin/sleep");
            c.arg("5");
            let what = if k == "five" { "attach" } else { "codex" };
            app.open((12, 80), c, what, k.into(), None);
        }
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["codex-1", "codex-2", "codex-3", "five"],
            "{keys:?}"
        );
        app.viewers.push(speculative_open(A));
        app.focus(4);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["codex-1", "codex-2", "codex-3", A], "{keys:?}");
        assert_eq!(app.focus, Some(3));
    }

    #[test]
    fn entering_a_speculative_viewer_with_three_live_ones_closes_the_oldest() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        for k in ["one", "two", "three"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        app.viewers.insert(0, speculative_open(A));
        // The speculative viewer is the oldest by time; "one" is the oldest the user was in.
        app.viewers[0].last_focused = Instant::now() - Duration::from_secs(60);
        app.viewers[1].last_focused = Instant::now() - Duration::from_secs(30);
        app.focus(0);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec![A, "two", "three"], "{keys:?}");
        assert_eq!(app.focus, Some(0));
        assert!(!app.viewers[0].speculative);
        assert_eq!(app.live_viewers(), MAX_FOCUSED_VIEWERS);
    }

    #[test]
    fn entering_a_speculative_viewer_keeps_the_focus_on_it_after_the_shift() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        for k in ["one", "two", "three"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        app.viewers.push(speculative_open(A));
        app.viewers[0].last_focused = Instant::now() - Duration::from_secs(60);
        app.focus(3);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["two", "three", A], "{keys:?}");
        assert_eq!(app.focus, Some(2));
        assert!(!app.viewers[2].speculative);
    }

    #[test]
    fn enter_on_a_row_with_a_speculative_viewer_makes_it_the_real_one() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.viewers.push(speculative_open(A));
        assert_eq!(
            app.enter_label(),
            "attach",
            "the user has not been there, so the verb does not say return"
        );
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        assert_eq!(app.viewers.len(), 1, "nothing was spawned");
        assert!(!app.viewers[0].speculative);
        app.unfocus();
        assert_eq!(app.enter_label(), "return");
    }

    #[test]
    fn a_speculative_viewer_closes_when_its_session_leaves_the_list() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.viewers.push(speculative_open(A));
        app.viewers.push(silent_open(B));
        app.refresh().unwrap();
        app.prespawn_tick();
        assert_eq!(app.viewers.len(), 2, "the session is still listed");
        fs::remove_file(d.path().join("sessions").join(format!("{A}.json"))).unwrap();
        app.refresh().unwrap();
        app.prespawn_tick();
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![B],
            "the speculative viewer went; a viewer the user has been in stays"
        );
    }

    /// A dropped `Viewer` is the only thing that reaps a viewer's `setsid` child, so a signal has to
    /// reach the loop rather than kill the dashboard where it stands and orphan one viewer apiece.
    #[test]
    fn a_signal_asks_the_dashboard_loop_to_quit() {
        let signalled = quit_on_signals().unwrap();
        assert!(
            !signalled.load(Ordering::Relaxed),
            "nothing has signalled yet"
        );
        // ponytail: raises SIGHUP alone; all three share the one registration above.
        unsafe { libc::raise(libc::SIGHUP) };
        let flipped = signalled.load(Ordering::Relaxed);
        // Hand the signals back, or this test binary becomes the thing only SIGKILL can stop.
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            unsafe { libc::signal(signal, libc::SIG_DFL) };
        }
        assert!(flipped, "SIGHUP sets the flag the dashboard loop reads");
    }
}
