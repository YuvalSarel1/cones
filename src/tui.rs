//! Native dashboard; user-facing behavior is documented in docs/dashboard.md.
//! Run statuses must not reuse `active`, `idle`, `blocked` or `exited`;
//! shared match arms would sort and render those runs as sessions.
#[cfg(test)]
#[path = "../assets/tui_capture.rs"]
mod readme_capture;

use crate::{
    codex,
    config::{self, HarnessKind, ResolvedJob},
    fleet::{self, Session},
    harness::{self, Start},
    history, launchd,
    ledger::{Ledger, Run},
    output, runner, terminal, transcript,
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
use serde_json::{Value, json};
use std::{
    cell::RefCell,
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
    /// A historical session's key includes its harness and native home.
    History(String),
    HistoryStatus,
    /// Run id and status.
    Run(String, String),
    /// Inserted by `App::rebuild`, outside `Data::rows`.
    Menu,
    /// A pinned folder with no live sessions, in `~` form.
    Folder(String),
    NewJob,
}

impl Kind {
    fn diagnostic_name(&self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Columns => "columns",
            Self::Blank => "blank",
            Self::Job(_) => "job",
            Self::Session(..) => "session",
            Self::History(_) => "history",
            Self::HistoryStatus => "history_status",
            Self::Run(..) => "run",
            Self::Menu => "menu",
            Self::Folder(_) => "folder",
            Self::NewJob => "new_job",
        }
    }

    fn selectable(&self) -> bool {
        !matches!(
            self,
            Kind::Header | Kind::Columns | Kind::Blank | Kind::HistoryStatus
        )
    }

    /// Exclude state so a row keeps its identity across reloads.
    pub fn key(&self) -> Option<&str> {
        match self {
            Kind::Job(name) => Some(name),
            Kind::Session(id, _) | Kind::Run(id, _) => Some(id),
            Kind::History(key) => Some(key),
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
    pub run_columns: Vec<String>,
    pub job_columns: Vec<String>,
    pub history_columns: Vec<String>,
    columns_default: bool,
    branches: BTreeMap<PathBuf, String>,
    next_runs: BTreeMap<String, chrono::DateTime<chrono::Utc>>,
    run_reports: HashMap<String, history::Columns>,
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
    diagnostics: Option<LoadDiagnostics>,
}

impl Data {
    pub fn load(jobs_path: &Path, state: &Path, claude: &Path) -> Result<Self> {
        Self::load_observed(jobs_path, state, claude, None, None)
    }

    fn load_observed(
        jobs_path: &Path,
        state: &Path,
        claude: &Path,
        log: Option<&Diagnostics>,
        operation: Option<&DiagnosticOperation>,
    ) -> Result<Self> {
        let mut diagnostics = log.map(|_| LoadDiagnostics::default());
        macro_rules! phase {
            ($name:expr, $read:expr) => {{
                let started = Instant::now();
                let result: Result<_> = $read;
                if let Some(d) = &mut diagnostics { d.phase($name, started); }
                if let (Some(log), Err(error)) = (log, &result) {
                    log.event("error", "load.failed", json!({
                        "operation_id": operation.map(|o| &o.id),
                        "phase": $name, "error": format!("{error:#}"),
                        "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
                    }));
                }
                result?
            }};
        }
        let ledger = phase!("ledger.open", Ledger::new(state));
        let hidden = phase!("ledger.hidden", ledger.hidden());
        let mut runs = phase!("ledger.runs", ledger.runs());
        runs.retain(|r| !hidden.contains(&r.started.run_id));
        // Read before discovery: the harnesses config offers decide what is scanned.
        let offered = config::defaults(jobs_path);
        let sessions = phase!(
            "discovery",
            fleet_rows_observed(
                claude,
                state,
                &runs,
                &offered,
                diagnostics.as_mut(),
                log,
                operation,
            )
        );
        let reports_started = Instant::now();
        let run_reports = runs
            .iter()
            .rev()
            .take(200)
            .filter_map(|r| {
                if r.started.harness != Some(HarnessKind::Claude) {
                    return None;
                }
                let path = r
                    .started
                    .output
                    .as_deref()
                    .filter(|p| p.is_file())
                    .or_else(|| r.terminal.as_ref().and_then(|t| t.transcript.as_deref()));
                Some((
                    r.started.run_id.clone(),
                    fleet::run_columns(path, claude, r.started.session_id.as_deref()),
                ))
            })
            .collect();
        if let Some(d) = &mut diagnostics {
            d.phase("run_reports", reports_started);
        }
        let seen: Vec<PathBuf> = sessions.iter().map(|s| s.cwd.clone()).collect();
        let folders = phase!("ledger.folders", ledger.folders());
        let config_started = Instant::now();
        let jobs = match config::read_jobs(jobs_path) {
            Ok(jobs) => jobs,
            Err(error) => {
                if jobs_path.exists()
                    && let Some(d) = &mut diagnostics
                {
                    d.warnings.push(format!("configuration: {error:#}"));
                }
                Vec::new()
            }
        };
        let columns = config::columns(jobs_path);
        let columns_default = config::file_columns(jobs_path).is_none();
        let job_columns = config::job_columns(jobs_path);
        let history_columns = config::history_columns(jobs_path);
        let run_columns = config::run_columns(jobs_path);
        let pane = config::pane(jobs_path);
        let start = config::start(jobs_path);
        let spark = config::activity(jobs_path);
        let confirm_secs = config::confirm_secs(jobs_path);
        let whole_columns = config::whole_columns(jobs_path);
        if let Some(d) = &mut diagnostics {
            d.phase("configuration", config_started);
        }
        let branches_started = Instant::now();
        let branches = if columns.iter().any(|c| c == "branch") {
            seen.iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter_map(|dir| git_branch(dir).map(|b| (dir.clone(), b)))
                .collect()
        } else {
            BTreeMap::new()
        };
        let next_runs = if job_columns.iter().any(|c| c == "next_run") {
            next_runs(&jobs)
        } else {
            BTreeMap::new()
        };
        if let Some(d) = &mut diagnostics {
            d.phase("branches_and_schedule", branches_started);
        }
        let git_started = Instant::now();
        // Every added folder, not just the ones standing empty right now: a deletion shows the
        // folder's row before the registry drops the session, and a row that gains its branch on
        // the next read reads as a flicker.
        // ponytail: one git status per added folder per read; cache by mtime if a read drags.
        let git = folders
            .iter()
            .filter_map(|f| git_state(f).map(|g| (f.clone(), g)))
            .collect();
        if let Some(d) = &mut diagnostics {
            d.phase("git", git_started);
        }
        let recent = phase!("ledger.recent", ledger.recent(&seen));
        Ok(Self {
            jobs,
            runs,
            sessions,
            columns,
            columns_default,
            run_columns,
            job_columns,
            history_columns,
            branches,
            next_runs,
            run_reports,
            pane,
            start,
            spark,
            confirm_secs,
            whole_columns,
            folders,
            recent,
            git,
            diagnostics,
        })
    }

    /// A pending deletion is already hidden, so its folder must not stay covered by it.
    fn has_rows_in(&self, dir: &Path, deleting: &HashSet<&str>) -> bool {
        self.sessions
            .iter()
            .any(|s| s.cwd == dir && !deleting.contains(s.session_id.as_str()))
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
        self.rows_excluding(by_state, false, false, &HashSet::new(), &mut Widths::new())
    }

    /// Hide pending deletions without changing source data, so failures can restore their rows.
    fn rows_excluding(
        &self,
        by_state: bool,
        jobs_view: bool,
        hide_reply: bool,
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
            if group.is_empty() && !self.has_rows_in(dir, deleting) {
                group.push(Entry::Folder(dir));
            }
        }
        // One table across all groups, so columns line up between directories.
        let flat: Vec<(&(String, String), &Entry)> = groups
            .iter()
            .flat_map(|(key, group)| group.iter().map(move |e| (key, e)))
            .collect();
        let table = flat.iter().any(|(_, e)| !matches!(e, Entry::Folder(_)));
        let session_columns = visible_session_columns(&self.columns, by_state, hide_reply);
        let set = if jobs_view {
            &self.job_columns
        } else {
            &session_columns
        };
        let state_key = if jobs_view { "status" } else { "state" };
        let has_state = set.iter().any(|c| c == state_key);
        let has_harness = set.iter().any(|c| c == "harness");
        let cols: Vec<&String> = set
            .iter()
            .filter(|c| *c != state_key && *c != "harness")
            .collect();
        let sparks = fleet::sparklines(&self.sessions, &self.spark, chrono::Utc::now());
        let cells = flat
            .iter()
            .filter(|(_, e)| !matches!(e, Entry::Folder(_)))
            .map(|(_, e)| match e {
                Entry::Folder(_) => vec![],
                Entry::Session(s) => session_cells(
                    s,
                    set,
                    by_state,
                    sparks.get(&s.session_id).map(String::as_str),
                    self.branches.get(&s.cwd).map(String::as_str),
                ),
                Entry::Job(j) => {
                    let last = self
                        .runs
                        .iter()
                        .rev()
                        .find(|r| r.started.job.as_deref() == Some(&j.name));
                    let status = last.map_or("-".to_owned(), |r| r.status());
                    let next = self.next_runs.get(&j.name).copied();
                    let h = j.harness.to_string();
                    let mut row = vec![
                        (if j.enabled { "◆" } else { "◇" }.into(), color(&status)),
                        (
                            if has_harness {
                                logo_cell(&h)
                            } else {
                                mark(&h).into()
                            },
                            brand(&h),
                        ),
                    ];
                    if has_state {
                        row.push(job_cell("status", j, last, &status, next));
                    }
                    row.push((j.name.clone(), plain()));
                    row.extend(cols.iter().map(|c| job_cell(c, j, last, &status, next)));
                    row
                }
            })
            .collect();
        let mut names = vec!["", ""];
        if has_state {
            names.push(state_key);
        }
        names.push(if jobs_view { "job" } else { "title" });
        names.extend(cols.iter().map(|c| column_label(c)));
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
                        cells.push((g.clone(), plain()));
                    }
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
            let set = &self.run_columns;
            let has_status = set.iter().any(|c| c == "status");
            let cols: Vec<&str> = set
                .iter()
                .map(String::as_str)
                .filter(|c| !matches!(*c, "harness" | "status"))
                .collect();
            let mut names = vec!["", ""];
            if has_status {
                names.push("status");
            }
            names.push("job");
            names.extend(cols.iter().map(|c| column_label(c)));
            let cells = runs
                .iter()
                .map(|r| {
                    let status = r.status();
                    let h = r.started.harness.map(|h| h.to_string()).unwrap_or_default();
                    let harness = if h.is_empty() {
                        "-".into()
                    } else if set.iter().any(|c| c == "harness") {
                        logo_cell(&h)
                    } else {
                        mark(&h).into()
                    };
                    let mut row =
                        vec![(icon(&status).into(), color(&status)), (harness, brand(&h))];
                    if has_status {
                        row.push((status.clone(), color(&status)));
                    }
                    row.push((r.started.job.clone().unwrap_or_else(|| "-".into()), plain()));
                    let report = self
                        .run_reports
                        .get(&r.started.run_id)
                        .cloned()
                        .unwrap_or_default();
                    row.extend(cols.iter().map(|c| run_cell(c, r, &report)));
                    row
                })
                .collect();
            let (names, cells) = columns(&names, cells, widths);
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
                let mut out = vec![
                    fleet::tilde(&s.cwd),
                    format!(
                        "{} · {} {}{} · {} · started {} · last activity {} · {} context · {} tokens · pid {} · {}",
                        logo(&s.harness),
                        label(&s.state),
                        s.kind.as_deref().unwrap_or(""),
                        if s.coordinator { " orchestrator" } else { "" },
                        s.model.as_deref().map_or_else(|| "-".into(), fleet::model),
                        local_stamp(s.started),
                        local_stamp(s.last_activity),
                        fleet::context(s),
                        fleet::tokens(s),
                        s.pid.map(|p| p.to_string()).unwrap_or_default(),
                        s.session_id
                    ),
                    String::new(),
                ];
                if let Some(cost) = crate::cost::describe(s.cost_usd, s.cost_info.as_ref()) {
                    out.insert(2, cost);
                }
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
            Kind::Header | Kind::Columns | Kind::Blank | Kind::HistoryStatus => {
                ("hdr".to_owned(), "-".to_owned())
            }
            Kind::Job(n) => ("job".to_owned(), n.clone()),
            Kind::Session(id, s) | Kind::Run(id, s) => (id.clone(), s.clone()),
            Kind::History(key) => (key.clone(), "-".into()),
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
const MENU: [(&str, &str, &str); 5] = [
    (
        "folder",
        "add folder",
        "a row for a folder nothing runs in, to start work there",
    ),
    ("jobs", "jobs", "the jobs: start, edit, add one"),
    ("config", "defaults", "job defaults and dashboard settings"),
    (
        "columns",
        "columns",
        "choose and order the columns in each table",
    ),
    ("help", "guide", "the keys and what they do"),
];

fn enter_verb(kind: Option<&Kind>, menu: usize) -> &'static str {
    match kind {
        Some(Kind::Job(_)) => "start job",
        Some(Kind::Run(_, s)) if s == "started" => "follow log",
        Some(Kind::Session(..) | Kind::Run(..)) => "attach",
        Some(Kind::History(_)) => "resume",
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

/// Match every search word against the shortcut, explanation and section name.
fn guide_rows(find: &str) -> Vec<&'static (&'static str, &'static str)> {
    let words: Vec<String> = find.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return GUIDE.iter().collect();
    }
    let mut rows = vec![];
    let mut head = None;
    let mut shown = false;
    for entry in GUIDE {
        let (key, what) = entry;
        if key.is_empty() {
            head = Some(entry);
            shown = false;
        } else {
            let text =
                format!("{} {key} {what}", head.map_or("", |(_, title)| *title)).to_lowercase();
            if words.iter().all(|word| text.contains(word)) {
                if !shown {
                    rows.extend(head);
                    shown = true;
                }
                rows.push(entry);
            }
        }
    }
    rows
}

fn guide_lines(columns: u16, find: &str) -> Vec<Line<'static>> {
    let rows = guide_rows(find);
    if rows.is_empty() {
        return vec![
            Line::from("No shortcuts match your search."),
            Line::default(),
            Line::from(Span::styled(
                "Try a key or topic, such as config, clipboard or pane.",
                dim(),
            )),
            Line::from(Span::styled("Esc clears the search.", dim())),
        ]
        .into_iter()
        .flat_map(|line| hang(line.spans, 0, columns.max(1) as usize))
        .collect();
    }
    let width = GUIDE
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let indent = 2 + width + 2;
    let mut lines = vec![];
    for (key, what) in rows {
        if key.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                (*what).to_owned(),
                bold().fg(ORANGE),
            )));
            continue;
        }
        if columns < 48 {
            lines.extend(hang(
                vec![Span::styled(format!("  {key}"), bold())],
                0,
                columns.max(1) as usize,
            ));
            lines.extend(hang(
                vec![Span::raw("    "), Span::raw((*what).to_owned())],
                4.min(columns.saturating_sub(1) as usize),
                columns.max(1) as usize,
            ));
        } else {
            lines.extend(hang(
                vec![
                    Span::styled(format!("  {key:width$}  "), bold()),
                    Span::raw((*what).to_owned()),
                ],
                indent,
                columns as usize,
            ));
        }
    }
    lines
}

#[derive(Default)]
struct Guide {
    top: usize,
    find: Input,
    area: Rect,
}

impl Guide {
    fn body_height(&self) -> usize {
        self.area
            .height
            .saturating_sub(if self.area.height >= 4 { 3 } else { 0 })
            .max(1) as usize
    }

    fn max_scroll(&self) -> usize {
        guide_lines(self.area.width.max(1), &self.find.text)
            .len()
            .saturating_sub(self.body_height())
    }

    /// Return true only when leaving Help. Search editing never launches an action.
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match code {
            KeyCode::Esc if !self.find.text.is_empty() => {
                self.find = Input::default();
                self.top = 0;
            }
            KeyCode::Esc => return true,
            KeyCode::Char('g') if mods.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Left if self.find.text.is_empty() => return true,
            KeyCode::Enter => {}
            KeyCode::Char('/') if self.find.text.is_empty() => {}
            KeyCode::Char('f') if mods.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                self.find = Input::default();
                self.top = 0;
            }
            KeyCode::Up => self.top = self.top.saturating_sub(1),
            KeyCode::Down => self.top = (self.top + 1).min(self.max_scroll()),
            KeyCode::PageUp => self.top = self.top.saturating_sub(self.body_height()),
            KeyCode::PageDown => self.top = (self.top + self.body_height()).min(self.max_scroll()),
            KeyCode::Home => self.top = 0,
            KeyCode::End => self.top = self.max_scroll(),
            _ => {
                if self.find.key(code, mods) {
                    self.top = 0;
                }
            }
        }
        false
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, active: bool) {
        self.area = area;
        let lines = guide_lines(area.width.max(1), &self.find.text);
        self.top = self.top.min(lines.len().saturating_sub(self.body_height()));
        let mut visible = vec![];
        if area.height >= 4 {
            let count = guide_rows(&self.find.text)
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .count();
            visible.push(Line::from(Span::styled("help", lit())));
            visible.push(Line::from(Span::styled(
                if self.find.text.trim().is_empty() {
                    format!(
                        "{} · {count} shortcuts",
                        if active {
                            "Type to search"
                        } else {
                            "Enter to search"
                        }
                    )
                } else {
                    format!(
                        "{count} {} · esc clears search",
                        if count == 1 { "match" } else { "matches" }
                    )
                },
                dim(),
            )));
            visible.push(Line::default());
        }
        visible.extend(lines.into_iter().skip(self.top).take(self.body_height()));
        frame.render_widget(Paragraph::new(visible), area);
    }

    fn hints(&self) -> Line<'static> {
        hints(&[
            ("↑↓", "scroll"),
            ("pgup/dn", "page"),
            (
                "esc",
                if self.find.text.is_empty() {
                    "back"
                } else {
                    "clear"
                },
            ),
        ])
    }
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
            .map(|(t, _)| Span::raw(t.as_str()).width())
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
                    let pad = widths[c] - Span::raw(text.as_str()).width()
                        + if c + 1 == n { 0 } else { 2 };
                    (format!("{text}{:pad$}", ""), style)
                })
                .collect()
        })
        .collect()
}

fn column_label(column: &str) -> &str {
    match config::column_name(column) {
        "tokens" => "tokens in/out",
        "last_reply" => "last reply",
        "last_active" => "last active",
        "last_run" => "last run",
        "next_run" => "next run",
        c => c,
    }
}

fn visible_session_columns(set: &[String], by_state: bool, hide_reply: bool) -> Vec<String> {
    set.iter()
        .map(|c| config::column_name(c))
        .filter(|c| (*c != "folder" || by_state) && (*c != "last_reply" || !hide_reply))
        .map(str::to_owned)
        .collect()
}

fn job_cell(
    column: &str,
    j: &ResolvedJob,
    last: Option<&Run>,
    status: &str,
    next: Option<chrono::DateTime<chrono::Utc>>,
) -> (String, Style) {
    match config::column_name(column) {
        "status" if !j.enabled => ("off".into(), dim()),
        "status" => (status.to_owned(), color(status)),
        "schedule" => (j.schedule.clone(), dim()),
        "next_run" => (
            if j.enabled {
                local_stamp(next)
            } else {
                "-".into()
            },
            dim(),
        ),
        "model" => (
            j.model.as_deref().map_or_else(|| "-".into(), fleet::model),
            dim(),
        ),
        "last_run" => (
            last.and_then(|r| r.started.fired_at)
                .map_or_else(|| "-".into(), fleet::age),
            dim(),
        ),
        "folder" => (fleet::tilde(&j.cwd), dim()),
        _ => ("-".into(), dim()),
    }
}

/// `spark` is scaled once for the fleet so rows share a bound.
fn session_cells(
    s: &Session,
    set: &[String],
    by_state: bool,
    spark: Option<&str>,
    branch: Option<&str>,
) -> Vec<(String, Style)> {
    let harness = if set.iter().any(|c| c == "harness") {
        logo_cell(&s.harness)
    } else {
        mark(&s.harness).into()
    };
    let mut row = vec![
        (icon(&s.state).into(), color(&s.state)),
        (harness, brand(&s.harness)),
    ];
    if set.iter().any(|c| c == "state") {
        row.push(cell("state", s, by_state, None));
    }
    let title = clip(
        &s.title
            .clone()
            .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
        40,
    );
    row.push(if s.coordinator {
        (format!("{COORDINATOR} {title}"), lit())
    } else {
        (title, plain())
    });
    row.extend(
        set.iter()
            .filter(|c| *c != "state" && *c != "harness")
            .map(|c| {
                if c == "branch" {
                    (branch.unwrap_or("-").into(), dim())
                } else {
                    cell(c, s, by_state, spark)
                }
            }),
    );
    row
}

fn cell(column: &str, s: &Session, _by_state: bool, spark: Option<&str>) -> (String, Style) {
    let since = |t: Option<chrono::DateTime<chrono::Utc>>| t.map_or_else(|| "-".into(), fleet::age);
    match config::column_name(column) {
        "state" => (label(&s.state).into(), color(&s.state)),
        "activity" => {
            let bars = spark.unwrap_or_default().to_owned();
            let quiet = bars == "-" || bars.chars().all(|c| c == '▁');
            (bars, if quiet { dim() } else { plain() })
        }
        "model" => (
            s.model.as_deref().map_or_else(|| "-".into(), fleet::model),
            dim(),
        ),
        "age" => (since(s.started), dim()),
        "context" => (fleet::context(s), dim()),
        "tokens" => (fleet::tokens(s), dim()),
        "folder" => (fleet::tilde(&s.cwd), dim()),
        "last_active" => (since(s.last_activity), dim()),
        "cost" => (
            crate::cost::display(s.cost_usd, s.cost_info.as_ref()),
            dim(),
        ),
        "last_reply" => (
            s.last.as_deref().map(|l| clip(l, 100)).unwrap_or_default(),
            dim(),
        ),
        _ => ("?".into(), dim()),
    }
}

fn local_stamp(at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    at.map(|t| {
        t.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string()
    })
    .unwrap_or_else(|| "-".into())
}

fn run_cell(column: &str, run: &Run, report: &history::Columns) -> (String, Style) {
    let last = run.terminal.as_ref().unwrap_or(&run.started);
    let text = match config::column_name(column) {
        "started" => local_stamp(run.started.fired_at),
        "ended" => local_stamp(last.ended_at),
        "duration" => last
            .duration_s
            .or_else(|| {
                (run.terminal.is_none() && run.status() == "started")
                    .then(|| {
                        run.started.fired_at.map(|at| {
                            (chrono::Utc::now() - at).num_milliseconds().max(0) as f64 / 1000.0
                        })
                    })
                    .flatten()
            })
            .map(|d| format!("{d:.0}s"))
            .unwrap_or_else(|| "-".into()),
        "model" => report
            .model
            .as_deref()
            .map(fleet::model)
            .unwrap_or_else(|| "-".into()),
        "context" => fleet::context_values(report.context_tokens, report.context_window),
        "tokens" => fleet::token_values(
            last.tokens_in.or(report.tokens_in),
            last.tokens_out.or(report.tokens_out),
        ),
        "cost" => last
            .cost_usd
            .or_else(|| run.terminal.is_none().then_some(report.cost_usd).flatten())
            .map(fleet::cost)
            .unwrap_or_else(|| "-".into()),
        "reason" => last.reason.clone().unwrap_or_else(|| "-".into()),
        "folder" => run
            .started
            .cwd
            .as_deref()
            .map(fleet::tilde)
            .unwrap_or_else(|| "-".into()),
        "trigger" => run.started.trigger.clone().unwrap_or_else(|| "-".into()),
        "last_reply" => report
            .last
            .as_deref()
            .map(|s| clip(s, 100))
            .unwrap_or_else(|| "-".into()),
        _ => "-".into(),
    };
    (text, dim())
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

fn tab_buttons(
    labels: impl Iterator<Item = &'static str>,
    selected: usize,
    focused: bool,
) -> Vec<Span<'static>> {
    labels
        .enumerate()
        .flat_map(|(i, label)| {
            [
                Span::styled(
                    format!(" {label} "),
                    match (i == selected, focused) {
                        (true, true) => pressed(),
                        (true, false) => button().fg(ORANGE),
                        _ => button(),
                    },
                ),
                Span::raw(" "),
            ]
        })
        .collect()
}

fn tab_key(code: KeyCode, selected: usize, count: usize) -> Option<usize> {
    match code {
        KeyCode::Left | KeyCode::Char('[') => Some((selected + count - 1) % count),
        KeyCode::Right | KeyCode::Char(']') => Some((selected + 1) % count),
        KeyCode::Home => Some(0),
        KeyCode::End => Some(count - 1),
        _ => None,
    }
}

fn lit() -> Style {
    Style::default().fg(ORANGE).add_modifier(Modifier::BOLD)
}

/// Pad an editor row out to the pane and shade it, so the row the cursor is on reads as one line.
fn on_row(lines: &mut [Line<'static>], columns: u16) {
    for l in lines {
        let w: usize = l.spans.iter().map(Span::width).sum();
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
    if harness == "terminal" {
        return "$";
    }
    harness::by_name(harness).map_or(harness, |spec| spec.icon.as_str())
}

fn logo(harness: &str) -> String {
    if harness == "terminal" {
        return format!("{} {harness}", mark(harness));
    }
    harness::by_name(harness).map_or_else(
        || harness.to_owned(),
        |spec| format!("{} {harness}", spec.icon),
    )
}

/// A logo for a stacked table cell: a one-cell mark is padded so every name
/// starts in the column codex's two-cell `>_` sets. ponytail: two cells is the
/// widest mark shipped, widen it when a harness defines a wider icon.
fn logo_cell(harness: &str) -> String {
    let mark = mark(harness);
    if logo(harness) == harness {
        return harness.to_owned();
    }
    let pad = " ".repeat(2usize.saturating_sub(Span::raw(mark).width()));
    format!("{mark}{pad} {harness}")
}

/// Prefix on the coordinator's title, which is also drawn in orange.
const COORDINATOR: &str = "★";

fn brand(harness: &str) -> Style {
    harness::by_name(harness)
        .and_then(|spec| spec.colour)
        .map_or_else(dim, |[r, g, b]| Style::default().fg(Color::Rgb(r, g, b)))
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
    let count = spans.len();
    for (i, span) in spans.into_iter().enumerate() {
        // The two spaces between columns are the cell's own; falling off the edge is no cut.
        // Retain the allocated width, so short headings and missing values cannot
        // appear under a column whose wider values would be omitted.
        let gap = if i + 1 == count { 0 } else { 2 };
        if used + span.width().saturating_sub(gap) > width {
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
/// to a ledger run collapse into that run's row. A harness config does not offer is not scanned.
pub fn fleet_rows(
    claude: &Path,
    state: &Path,
    runs: &[Run],
    offered: &config::Policy,
) -> Result<Vec<Session>> {
    fleet_rows_observed(claude, state, runs, offered, None, None, None)
}

#[allow(clippy::too_many_arguments)]
fn fleet_rows_observed(
    claude: &Path,
    state: &Path,
    runs: &[Run],
    offered: &config::Policy,
    mut diagnostics: Option<&mut LoadDiagnostics>,
    log: Option<&Diagnostics>,
    operation: Option<&DiagnosticOperation>,
) -> Result<Vec<Session>> {
    let owned: HashSet<&str> = runs
        .iter()
        .filter_map(|r| r.started.session_id.as_deref())
        .collect();
    let live = fleet::all_observed(claude, offered, |harness, home, elapsed, result| {
        if let Some(d) = &mut diagnostics {
            d.phases.insert(
                format!("discovery.{harness}"),
                elapsed.as_secs_f64() * 1000.0,
            );
            if let Ok(rows) = result {
                let source =
                    if harness::by_name(harness).is_some_and(|s| s.discovery.registry.is_some()) {
                        "registry"
                    } else {
                        "process_table"
                    };
                for row in rows {
                    d.sources.insert(
                        format!("{harness}:{}", row.session_id),
                        json!({
                            "reader": source, "native_home": home.to_string_lossy(),
                        }),
                    );
                }
            }
        }
        if let (Some(log), Err(error)) = (log, result) {
            log.event(
                "error",
                "discovery.failed",
                json!({
                    "operation_id": operation.map(|o| &o.id),
                    "harness": harness, "native_home": home.to_string_lossy(), "error": format!("{error:#}"),
                }),
            );
        }
    })?;
    let mut out = Vec::new();
    for s in live {
        if owned.contains(s.session_id.as_str()) {
            if let Some(d) = &mut diagnostics {
                d.excluded
                    .insert(s.session_id.clone(), "represented_by_ledger_run");
            }
        } else {
            out.push(s);
        }
    }
    // Detached daemon threads have no client in the process table; include their saved records.
    let removed = Ledger::new(state)?.hidden()?;
    if let Some(d) = &mut diagnostics {
        d.excluded
            .extend(removed.iter().cloned().map(|id| (id, "hidden")));
    }
    let codex_homes = match offered.enabled_for(HarnessKind::Codex) {
        true => codex::homes(claude),
        false => vec![],
    };
    for home in codex_homes {
        let started = Instant::now();
        let rows = codex::thread_rows_observed(&home, state, &out, &removed, |id, source| {
            if let Some(d) = &mut diagnostics {
                d.sources.insert(
                    format!("codex:{id}"),
                    json!({
                        "reader": source, "native_home": home.to_string_lossy(),
                    }),
                );
            }
        });
        if let Some(d) = &mut diagnostics {
            *d.phases
                .entry("discovery.codex_threads".into())
                .or_default() += started.elapsed().as_secs_f64() * 1000.0;
        }
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

fn git_branch(dir: &Path) -> Option<String> {
    let read = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    read(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|| read(&["rev-parse", "--short", "HEAD"]).map(|hash| format!("@{hash}")))
}

fn next_runs(jobs: &[ResolvedJob]) -> BTreeMap<String, chrono::DateTime<chrono::Utc>> {
    type Stamp = chrono::DateTime<chrono::Utc>;
    static CACHE: std::sync::Mutex<BTreeMap<String, (Stamp, Option<Stamp>)>> =
        std::sync::Mutex::new(BTreeMap::new());
    let now = chrono::Utc::now();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|schedule, _| jobs.iter().any(|j| j.enabled && &j.schedule == schedule));
    jobs.iter()
        .filter(|j| j.enabled)
        .filter_map(|j| {
            let next = match cache.get(&j.schedule) {
                Some((checked, next))
                    if now >= *checked
                        && next
                            .map_or(now - *checked < chrono::Duration::days(1), |at| at > now) =>
                {
                    *next
                }
                _ => {
                    let next = launchd::next_fire(&j.schedule, now.with_timezone(&chrono::Local))
                        .ok()
                        .flatten()
                        .map(|at| at.with_timezone(&chrono::Utc));
                    cache.insert(j.schedule.clone(), (now, next));
                    next
                }
            };
            next.map(|next| (j.name.clone(), next))
        })
        .collect()
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
                Answer::Typed | Answer::Columns | Answer::Check => false,
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
        job.harness = harness::known()
            .iter()
            .copied()
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
                let default = if d.is_empty() || outside(&d).is_some() {
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
    hint: &'static str,
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
    /// A row that runs something instead of holding a value.
    Check,
}

impl Field {
    fn display<'a>(&self, value: &'a str) -> &'a str {
        if self
            .picks()
            .is_some_and(|p| p.contains(&"true") && p.contains(&"false"))
        {
            match value {
                "true" => return "on",
                "false" => return "off",
                _ => {}
            }
        }
        value
    }

    fn picks(&self) -> Option<&'static [&'static str]> {
        match self.input {
            Answer::Typed | Answer::Number(_) | Answer::Columns | Answer::Check => None,
            Answer::Pick(o) | Answer::PickOrType(o, _) => Some(o),
        }
    }

    fn typed(&self) -> bool {
        !matches!(
            self.input,
            Answer::Pick(_) | Answer::Columns | Answer::Check
        )
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
        } else if outside(builtin).is_some() {
            builtin
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

/// Empty AWS fields pass no override either, but what answers them is the AWS
/// resolution chain of the launching shell and ~/.aws/config, not the harness.
const AWS: &str = "AWS default";

/// What a row says for an empty value whose answer is owned outside cones.
fn outside(builtin: &str) -> Option<&'static str> {
    match builtin {
        SYSTEM => Some("harness default"),
        AWS => Some("AWS default"),
        _ => None,
    }
}

const GROUPS: [(&str, &str); 3] = [
    ("cones", "the dashboard itself"),
    ("harnesses", "models and providers"),
    ("runs", "what a run starts with"),
];

/// `start.harness` controls the composer; `defaults.harness` supplies the default for jobs.
const FIELDS: [Field; 38] = [
    Field {
        group: "cones",
        sub: "",
        name: "confirm_secs",
        short: "ctrl+x armed (s)",
        hint: "Time allowed for the second ctrl+x press.",
        long: "Seconds an armed ctrl+x waits for its second press with no key pressed, up to 600. 0 keeps the mark until the next key.",
        builtin: "2",
        input: Answer::Number(1.0),
    },
    Field {
        group: "cones",
        sub: "",
        name: "columns",
        short: "columns",
        hint: "Choose the columns shown in each table.",
        long: "Open the column picker for sessions, runs, jobs and history. Each table keeps its own visibility and order. Changes save immediately.",
        builtin: "state, context, activity, model, age, last_active, folder, last_reply",
        input: Answer::Columns,
    },
    Field {
        group: "cones",
        sub: "",
        name: "run_columns",
        short: "run columns",
        hint: "Choose columns for supervised runs.",
        long: "Columns for supervised runs. The harness icon and job always show; harness adds the name and status sits before the job. Left and right select, space shows or hides, [ ] reorder, and backspace restores defaults. Times use your local timezone.",
        builtin: "status, started, duration, model, cost, folder, reason",
        input: Answer::Columns,
    },
    Field {
        group: "cones",
        sub: "",
        name: "job_columns",
        short: "job columns",
        hint: "Choose columns for scheduled jobs.",
        long: "Job columns. Enabled and harness icons and the job name always show. Schedule and last run status are separate. Next run is the next local time matching the enabled job's configured schedule.",
        builtin: "status, schedule, next_run, model, last_run, folder",
        input: Answer::Columns,
    },
    Field {
        group: "cones",
        sub: "",
        name: "history_columns",
        short: "history columns",
        hint: "Choose columns for session history.",
        long: "Historical session columns, independent of live agents. Last active is time since the latest recorded activity. Folder identifies the conversation's directory. Live state and activity charts do not apply here.",
        builtin: "last_active, folder, model, context, last_reply",
        input: Answer::Columns,
    },
    Field {
        group: "cones",
        sub: "",
        name: "whole_columns",
        short: "whole columns only",
        hint: "Hide columns that would be cut off at the edge.",
        long: "true leaves out a column the list's right edge would cut through, so the table ends on a column that fits. false draws as much of it as there is room for. The mark, harness, state and title are always drawn, so a row names itself however narrow the list is.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "cones",
        sub: "start",
        name: "start.harness",
        short: "composer starts on",
        hint: "Default harness for the composer.",
        long: "The harness the composer is on in a new cones terminal; shift+tab changes it or selects a terminal, and cones writes nothing back. Codex, pi and OpenCode sessions start; their jobs remain unavailable. Pi and OpenCode run in the dashboard's own viewer and end with it. Model and provider defaults are below.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex", "pi", "opencode"]),
    },
    Field {
        group: "cones",
        sub: "start",
        name: "start.pane",
        short: "open with the pane",
        hint: "Show the viewer pane when cones starts.",
        long: "Whether a new cones terminal opens with the viewer pane beside the list; ctrl+\\ toggles it from there and cones writes nothing back.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "cones",
        sub: "pane",
        name: "pane.at",
        short: "pane side",
        hint: "Place the pane beside or below the list.",
        long: "right puts the pane beside the list, bottom under it.",
        builtin: "right",
        input: Answer::Pick(&["-", "right", "bottom"]),
    },
    Field {
        group: "cones",
        sub: "pane",
        name: "pane.ratio",
        short: "pane share (%)",
        hint: "Percentage of the screen used by the pane.",
        long: "Percent of the frame the pane takes, 30 to 70 in tens. The list keeps the rest, less the divider between them; a taller or wider terminal gives both more.",
        builtin: "50",
        input: Answer::Pick(&["-", "30", "40", "50", "60", "70"]),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bars",
        short: "bar count",
        hint: "Number of bars in the activity chart.",
        long: "Number of bars, 1 to 64, oldest first. 16 bars at 1m show the last 16 minutes.",
        builtin: "16",
        input: Answer::Number(1.0),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bucket",
        short: "time per bar",
        hint: "Time represented by each activity bar.",
        long: "Time per bar, such as 30s, 1m or 5m. Maximum 24h.",
        builtin: "1m",
        input: Answer::PickOrType(&["-", "30s", "1m", "5m", "15m", "1h"], "a duration"),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.metric",
        short: "count per bar",
        hint: "What the activity chart counts.",
        long: "lines: all transcript lines. messages: assistant replies. tools: tool calls. tokens: output tokens.",
        builtin: "lines",
        input: Answer::Pick(&["-", "lines", "messages", "tools", "tokens"]),
    },
    Field {
        group: "cones",
        sub: "activity",
        name: "activity.bound",
        short: "chart scale",
        hint: "How activity is scaled across sessions.",
        long: "fleet: busiest bucket on screen. row: each row's busiest bucket. log: fleet on a log scale. A number in jobs.yaml sets the count for a full bar.",
        builtin: "fleet",
        input: Answer::Pick(&["-", "fleet", "row", "log"]),
    },
    Field {
        group: "harnesses",
        sub: "",
        name: "check",
        short: "connectivity",
        hint: "Check which harnesses can launch here.",
        long: "Run the launch probe every harness is given before a session starts: it looks for the binary on cones's own launch PATH, then checks that the installed version takes the flags a dashboard session needs. The answer for each harness replaces this line. Nothing is written and no model is called.",
        builtin: "",
        input: Answer::Check,
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "claude_enabled",
        short: "enabled",
        hint: "Include Claude in the composer and session list.",
        long: "true offers claude in the composer; false takes it out of the shift+tab cycle, so a harness this machine does not have stops being something to land on. Sessions it already has stay listed, and a job that names it still runs it.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "model",
        short: "model",
        hint: "Claude model or alias for new sessions and jobs.",
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
        sub: "claude",
        name: "effort",
        short: "effort",
        hint: "How hard Claude thinks in new sessions and jobs.",
        long: "The level passed to Claude as --effort, for jobs and for sessions the composer starts. Higher levels spend more thinking tokens on a turn, so they cost more and answer slower. system default passes nothing and Claude's own settings decide.",
        builtin: SYSTEM,
        input: Answer::Pick(&["-", "low", "medium", "high", "xhigh", "max"]),
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "bedrock",
        short: "use Bedrock",
        hint: "Use Amazon Bedrock for Claude.",
        long: "true sends Claude to Amazon Bedrock, false to its own endpoint; system default passes nothing and the harness's own configuration decides. Claude is the only harness it reaches: a Codex job is refused outright, the Codex daemon keeps the provider it started with, and a composer pi uses its own pi_provider setting. true is refused without the profile and region below, since the switch alone reaches Bedrock with nothing to authenticate it.",
        builtin: SYSTEM,
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "aws_profile",
        short: "AWS profile",
        hint: "AWS profile for Claude on Bedrock.",
        long: "The profile every Bedrock run is given as AWS_PROFILE, as named in ~/.aws/config. Required by bedrock: true and unused without it; a session left on AWS default passes nothing and AWS resolves the profile itself. The run still inherits every other AWS_ variable for the credentials themselves.",
        builtin: AWS,
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "aws_region",
        short: "AWS region",
        hint: "AWS region for Claude on Bedrock.",
        long: "The region every Bedrock run is given as AWS_REGION, as in us-east-1. Required by bedrock: true and unused without it; a session left on AWS default passes nothing and AWS resolves the region from the shell or the profile. A model id is answered only by the regions that carry it.",
        builtin: AWS,
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
        sub: "codex",
        name: "codex_enabled",
        short: "enabled",
        hint: "Include Codex in the composer and session list.",
        long: "true offers codex in the composer; false takes it out of the shift+tab cycle, so a harness this machine does not have stops being something to land on. Sessions it already has stay listed, and a job that names it still runs it.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "codex",
        name: "codex_model",
        short: "model",
        hint: "Model for new Codex sessions.",
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
        short: "full access",
        hint: "Allow access outside the workspace sandbox.",
        long: "true allows all paths and network access without a sandbox. false uses the workspace sandbox; write controls file changes. Codex jobs are currently unavailable.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "pi",
        name: "pi_enabled",
        short: "enabled",
        hint: "Include pi in the composer and session list.",
        long: "true offers pi in the composer; false takes it out of the shift+tab cycle, so a harness this machine does not have stops being something to land on. Sessions it already has stay listed, and a job that names it still runs it.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "pi",
        name: "pi_model",
        short: "model",
        hint: "Model for new pi sessions.",
        long: "Passed to pi as --model for sessions the composer starts. Empty follows pi's own model configuration. Pi jobs are unavailable.",
        builtin: SYSTEM,
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "pi",
        name: "pi_provider",
        short: "provider",
        hint: "Provider for new pi sessions.",
        long: "Passed to pi as --provider for sessions the composer starts. Empty follows pi's own provider configuration.",
        builtin: SYSTEM,
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "pi",
        name: "pi_thinking",
        short: "thinking",
        hint: "How hard pi thinks in new sessions.",
        long: "The level passed to pi as --thinking for sessions the composer starts. Empty follows pi's own thinking configuration. Codex and OpenCode take no such flag, so neither offers this row.",
        builtin: SYSTEM,
        input: Answer::Pick(&[
            "-", "off", "minimal", "low", "medium", "high", "xhigh", "max",
        ]),
    },
    Field {
        group: "harnesses",
        sub: "opencode",
        name: "opencode_enabled",
        short: "enabled",
        hint: "Include OpenCode in the composer and session list.",
        long: "true offers opencode in the composer; false takes it out of the shift+tab cycle, so a harness this machine does not have stops being something to land on. Sessions it already has stay listed, and a job that names it still runs it.",
        builtin: "true",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "harnesses",
        sub: "opencode",
        name: "opencode_model",
        short: "provider/model",
        hint: "Provider/model for new OpenCode sessions.",
        long: "Passed to OpenCode as --model for sessions the composer starts. Use provider/model, as listed by opencode models. Empty follows OpenCode's own configuration. OpenCode jobs are unavailable.",
        builtin: SYSTEM,
        input: Answer::Typed,
    },
    Field {
        group: "runs",
        sub: "",
        name: "harness",
        short: "harness",
        hint: "Default harness for supervised jobs.",
        long: "The harness a run starts under, unless the job names one of its own. What the composer comes up on is start.harness above. Codex, pi and OpenCode jobs are unavailable.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex", "pi", "opencode"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "timeout_min",
        short: "time limit (min)",
        hint: "Stop a run after this many minutes.",
        long: "Positive minutes, up to 10080 (one week). cones stops overdue runs and records a timeout.",
        builtin: "30",
        input: Answer::Number(5.0),
    },
    Field {
        group: "runs",
        sub: "",
        name: "write",
        short: "allow file changes",
        hint: "Allow supervised jobs to edit files.",
        long: "false lets a job Read, Grep and Glob only. true adds Edit, Write and sandboxed Bash; a Codex job becomes workspace-write.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "overlap",
        short: "already running",
        hint: "What to do when a job is already running.",
        long: "When a job is already running: skip the next run, allow both, or replace the active run.",
        builtin: "skip",
        input: Answer::Pick(&["-", "skip", "allow", "replace"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "catch_up",
        short: "missed ticks",
        hint: "Whether to run once after missed schedule ticks.",
        long: "launchd loses a tick that passes while the Mac is powered off or logged out. once starts one run at the next login when any tick was missed, however many passed; skip leaves them lost. A slept-through tick already fires on wake and needs neither.",
        builtin: "skip",
        input: Answer::Pick(&["-", "skip", "once"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "notify",
        short: "failure alerts",
        hint: "Show notifications for failures and timeouts.",
        long: "Notify on failures and timeouts.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "archive_transcript",
        short: "save transcript",
        hint: "Keep a copy of each completed run’s transcript.",
        long: "true copies Claude's transcript into ~/.cones/transcripts/<run id>/ when the run ends.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
    Field {
        group: "runs",
        sub: "",
        name: "env",
        short: "import env vars",
        hint: "Names of shell variables imported by jobs.",
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
    hint: "Include this job in its schedule.",
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
        let builtin = if outside(builtin).is_some() {
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
    Columns,
    /// Validated values to persist. Absent sets use defaults; empty sets hide optional columns.
    Save(
        Box<config::Policy>,
        Option<Vec<String>>,
        Option<Box<config::Activity>>,
        Option<config::Pane>,
        Option<config::Start>,
        Option<f64>,
        Option<bool>,
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<Vec<String>>,
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
    /// Each group keeps its last selected field.
    selected: [usize; 3],
    /// A choice list is separate from text editing; browsing never changes the value.
    choice: Option<usize>,
    /// Focus is on the group tabs, the button row the dashboard menu uses.
    tabs: bool,
    /// What a row that runs something reported, held until the next key.
    note: Option<String>,
    area: Rect,
    top: usize,
    choice_top: usize,
    /// Scroll position in the selected field's full explanation.
    help: Option<usize>,
}

impl ConfigForm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        d: &config::Policy,
        columns: Option<&[String]>,
        spark: Option<&config::Activity>,
        pane: Option<&config::Pane>,
        start: Option<&config::Start>,
        confirm_secs: Option<f64>,
        whole_columns: Option<bool>,
        run_columns: Option<&[String]>,
        job_columns: Option<&[String]>,
        history_columns: Option<&[String]>,
    ) -> Self {
        let num = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
        let flag = |v: Option<bool>| v.map(|v| v.to_string()).unwrap_or_default();
        let spark = |f: fn(&config::Activity) -> String| spark.map(f).unwrap_or_default();
        let pane = |f: fn(&config::Pane) -> String| pane.map(f).unwrap_or_default();
        let sets = [
            ("columns", columns),
            ("run_columns", run_columns),
            ("job_columns", job_columns),
            ("history_columns", history_columns),
        ];
        let column_value = |key: &str| {
            sets.iter()
                .find(|(name, _)| *name == key)
                .and_then(|(_, set)| *set)
                .map(|c| {
                    if c.is_empty() {
                        "[]".into()
                    } else {
                        c.iter()
                            .map(|c| config::column_name(c))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                })
                .unwrap_or_default()
        };
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
                "pi_model" => d.pi_model.clone().unwrap_or_default(),
                "pi_provider" => d.pi_provider.clone().unwrap_or_default(),
                "effort" => d.effort.clone().unwrap_or_default(),
                "pi_thinking" => d.pi_thinking.clone().unwrap_or_default(),
                "opencode_model" => d.opencode_model.clone().unwrap_or_default(),
                "claude_enabled" => flag(d.claude_enabled),
                "codex_enabled" => flag(d.codex_enabled),
                "pi_enabled" => flag(d.pi_enabled),
                "opencode_enabled" => flag(d.opencode_enabled),
                "check" => String::new(),
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
                "columns" | "run_columns" | "job_columns" | "history_columns" => {
                    column_value(f.name)
                }
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
            selected: std::array::from_fn(|i| {
                FIELDS.iter().position(|f| f.group == GROUPS[i].0).unwrap()
            }),
            choice: None,
            tabs: false,
            note: None,
            area: Rect::default(),
            top: 0,
            choice_top: 0,
            help: None,
        }
    }

    fn tab(&self) -> usize {
        GROUPS
            .iter()
            .position(|g| g.0 == self.field().group)
            .unwrap()
    }

    fn switch(&mut self, tab: usize) {
        self.selected[self.tab()] = self.row;
        self.step(self.selected[tab.min(GROUPS.len() - 1)]);
        self.top = 0;
    }

    /// A validation error or link can jump into another group.
    fn go(&mut self, row: usize) {
        let row = if config_field_visible(row) {
            row
        } else {
            field_at("columns")
        };
        self.step(row);
    }

    fn step(&mut self, row: usize) {
        self.tabs = false;
        self.row = row;
        self.cursor = usize::MAX;
        self.choice = None;
        self.help = None;
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
            Option<Vec<String>>,
            Option<config::Activity>,
            Option<config::Pane>,
            Option<config::Start>,
            Option<f64>,
            Option<bool>,
            Option<Vec<String>>,
            Option<Vec<String>>,
            Option<Vec<String>>,
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
        let column_values = |key: &str| match v(key) {
            "" => None,
            "[]" => Some(Vec::new()),
            _ => Some(names(key)),
        };
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
            pi_model: text("pi_model"),
            pi_provider: text("pi_provider"),
            effort: text("effort"),
            pi_thinking: text("pi_thinking"),
            opencode_model: text("opencode_model"),
            claude_enabled: flag("claude_enabled"),
            codex_enabled: flag("codex_enabled"),
            pi_enabled: flag("pi_enabled"),
            opencode_enabled: flag("opencode_enabled"),
            bedrock: flag("bedrock"),
            aws_profile: text("aws_profile"),
            aws_region: text("aws_region"),
            harness: harness::known()
                .iter()
                .copied()
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
                harness: harness::known()
                    .iter()
                    .copied()
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
            column_values("columns"),
            spark,
            pane,
            start,
            mark,
            flag("whole_columns"),
            column_values("run_columns"),
            column_values("job_columns"),
            column_values("history_columns"),
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

    fn fields(&self) -> Vec<usize> {
        (0..FIELDS.len())
            .filter(|&i| config_field_visible(i) && FIELDS[i].group == self.field().group)
            .collect()
    }

    fn down(&mut self) {
        if let Some(row) = self.fields().into_iter().find(|&i| i > self.row) {
            self.step(row);
        }
    }

    fn up(&mut self) {
        match self.fields().into_iter().rev().find(|&i| i < self.row) {
            Some(row) => self.step(row),
            None => self.tabs = true,
        }
    }

    fn choices(&self) -> Vec<String> {
        let mut choices = self.field().ring(&self.values[self.row]);
        if matches!(self.field().input, Answer::PickOrType(..)) {
            choices.push(self.values[self.row].clone());
        }
        choices
    }

    fn choose(&mut self) -> ConfigAction {
        let at = self.choice.unwrap();
        let choices = self.choices();
        if matches!(self.field().input, Answer::PickOrType(..)) && at + 1 == choices.len() {
            self.choice = None;
            self.enter();
            if self.field().picked(&self.values[self.row]) {
                self.values[self.row].clear();
            }
            return ConfigAction::Stay;
        }
        self.before = self.values[self.row].clone();
        self.values[self.row] = choices[at].clone();
        let action = self.commit();
        if self.error.is_none() {
            self.choice = None;
        }
        action
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
            Ok((p, c, s, pn, st, m, w, rc, jc, hc)) => {
                self.open = false;
                if changed {
                    ConfigAction::Save(Box::new(p), c, s.map(Box::new), pn, st, m, w, rc, jc, hc)
                } else {
                    ConfigAction::Stay
                }
            }
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> ConfigAction {
        self.error = None;
        self.note = None;
        if let Some(top) = self.help {
            let page = self.area.height.max(1) as usize;
            let last = self
                .details()
                .line_count(self.area.width.max(1))
                .saturating_sub(page);
            self.help = match code {
                KeyCode::Esc | KeyCode::Left | KeyCode::F(1) | KeyCode::Char('?') => None,
                KeyCode::Up => Some(top.saturating_sub(1)),
                KeyCode::Down => Some((top + 1).min(last)),
                KeyCode::PageUp => Some(top.saturating_sub(page)),
                KeyCode::PageDown => Some((top + page).min(last)),
                KeyCode::Home => Some(0),
                KeyCode::End => Some(last),
                _ => Some(top),
            };
            return ConfigAction::Stay;
        }
        if code == KeyCode::F(1) || (!self.open && code == KeyCode::Char('?')) {
            self.help = Some(0);
            return ConfigAction::Stay;
        }
        if let Some(at) = self.choice {
            let last = self.choices().len().saturating_sub(1);
            let page = self.area.height.saturating_sub(self.header_rows()).max(1) as usize;
            match code {
                KeyCode::Esc | KeyCode::Left => self.choice = None,
                KeyCode::Enter | KeyCode::Char(' ') => return self.choose(),
                KeyCode::Up => self.choice = Some(at.saturating_sub(1)),
                KeyCode::Down => self.choice = Some((at + 1).min(last)),
                KeyCode::Home => self.choice = Some(0),
                KeyCode::End => self.choice = Some(last),
                KeyCode::PageUp => self.choice = Some(at.saturating_sub(page)),
                KeyCode::PageDown => self.choice = Some((at + page).min(last)),
                _ => {}
            }
            return ConfigAction::Stay;
        }
        // The tab row behaves like the dashboard's menu buttons: ←→ pick, ↓ enters the fields.
        if self.tabs && !self.open {
            let tab = match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                KeyCode::Down | KeyCode::Enter | KeyCode::Char(' ') => {
                    self.tabs = false;
                    return ConfigAction::Stay;
                }
                _ => match tab_key(code, self.tab(), GROUPS.len()) {
                    Some(tab) => tab,
                    None => return ConfigAction::Stay,
                },
            };
            self.switch(tab);
            self.tabs = true;
            return ConfigAction::Stay;
        }
        if !self.open {
            match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ')
                    if matches!(self.field().input, Answer::Columns) =>
                {
                    return ConfigAction::Columns;
                }
                KeyCode::Backspace if matches!(self.field().input, Answer::Columns) => {}
                KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ')
                    if matches!(self.field().input, Answer::Check) =>
                {
                    self.note = Some(connectivity());
                }
                KeyCode::Backspace if matches!(self.field().input, Answer::Check) => {}
                KeyCode::Char('[') => self.switch(self.tab().saturating_sub(1)),
                KeyCode::Char(']') => self.switch((self.tab() + 1).min(GROUPS.len() - 1)),
                KeyCode::Enter if self.field().picks().is_some() => {
                    let f = self.field();
                    let ring = f.ring(&self.values[self.row]);
                    self.choice = Some(f.stop(&ring, &self.values[self.row]));
                    self.choice_top = 0;
                }
                KeyCode::Char(' ') if self.field().picks().is_some() => {
                    self.before = self.values[self.row].clone();
                    if self.turn(false) {
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
                    return self.commit();
                }
                KeyCode::Enter
                    if matches!(self.field().input, Answer::Typed | Answer::Number(_)) =>
                {
                    self.enter()
                }
                KeyCode::Enter | KeyCode::Down => self.down(),
                KeyCode::Up => self.up(),
                KeyCode::Home => self.step(self.fields()[0]),
                KeyCode::End => self.step(*self.fields().last().unwrap()),
                KeyCode::PageUp | KeyCode::PageDown => {
                    let count = self.area.height.saturating_sub(self.header_rows()).max(1);
                    for _ in 0..count {
                        if code == KeyCode::PageUp {
                            self.up();
                        } else {
                            self.down();
                        }
                    }
                }
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
                return self.commit();
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

    fn header_rows(&self) -> u16 {
        match self.area.height {
            0..=3 => 0,
            4..=7 => 2,
            _ => 4,
        }
    }

    fn label_width(&self, width: u16) -> usize {
        self.fields()
            .iter()
            .map(|&i| FIELDS[i].short.len() + if FIELDS[i].sub.is_empty() { 0 } else { 4 })
            .max()
            .unwrap_or(0)
            .min((width as usize / 2).saturating_sub(2))
    }

    fn tab_spans(&self) -> Vec<Span<'static>> {
        let mut spans = tab_buttons(GROUPS.iter().map(|&(name, _)| name), self.tab(), self.tabs);
        spans.push(Span::styled(
            if self.tabs { " ←→ group" } else { "" },
            dim(),
        ));
        spans
    }

    /// One physical row per setting or choice, including during text editing.
    fn lines(&self, columns: u16) -> (Vec<Line<'static>>, usize) {
        let mut lines = vec![];
        let mut at = 0;
        if let Some(choice) = self.choice {
            let f = self.field();
            let choices = self.choices();
            let ring = f.ring(&self.values[self.row]);
            let current = f.stop(&ring, &self.values[self.row]);
            for (i, value) in choices.iter().enumerate() {
                let label = if i == ring.len() {
                    if f.picked(value) {
                        "type a custom value…".to_owned()
                    } else {
                        format!("custom: {value}")
                    }
                } else if value.is_empty() {
                    if let Some(word) = outside(f.builtin) {
                        word.to_owned()
                    } else {
                        format!("{} (default)", f.display(f.builtin))
                    }
                } else {
                    f.display(value).to_owned()
                };
                let mut line = Line::from(fit(
                    vec![
                        Span::styled(if i == choice { "› " } else { "  " }, lit()),
                        Span::styled(
                            if i == current { "(*) " } else { "( ) " },
                            if i == current { plain() } else { dim() },
                        ),
                        Span::styled(label, if i == choice { lit() } else { plain() }),
                    ],
                    columns as usize,
                ));
                if i == choice {
                    at = lines.len();
                    on_row(std::slice::from_mut(&mut line), columns);
                }
                lines.push(line);
            }
            return (lines, at);
        }
        let label_w = self.label_width(columns);
        let mut sub = "";
        for i in self.fields() {
            let f = &FIELDS[i];
            if !f.sub.is_empty() && f.sub != sub {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(format!("  {}", f.sub), bold())));
            }
            sub = f.sub;
            let selected = i == self.row;
            let focused = selected && !self.tabs;
            let indent = if f.sub.is_empty() {
                0
            } else {
                4.min(label_w.saturating_sub(4))
            };
            let field_w = label_w - indent;
            let label = if field_w == 0 {
                String::new()
            } else {
                clip(f.short, field_w)
            };
            let mut spans = vec![
                Span::styled(if selected { "› " } else { "  " }, lit()),
                Span::raw(" ".repeat(indent)),
                Span::styled(
                    format!("{label:<field_w$} "),
                    if selected { lit() } else { plain() },
                ),
            ];
            let room = (columns as usize).saturating_sub(label_w + 3);
            spans.extend(self.control(i, room));
            let mut line = Line::from(fit(spans, columns as usize));
            if focused {
                at = lines.len();
                on_row(std::slice::from_mut(&mut line), columns);
            }
            lines.push(line);
        }
        (lines, at)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        if let Some(top) = self.help {
            let details = self.details();
            let last = details
                .line_count(area.width.max(1))
                .saturating_sub(area.height as usize);
            let top = top.min(last);
            frame.render_widget(details.scroll((top as u16, 0)), area);
            self.help = Some(top);
            return;
        }
        let header = self.header_rows();
        let (body, at) = self.lines(area.width);
        let height = area.height.saturating_sub(header) as usize;
        let top = if self.choice.is_some() {
            &mut self.choice_top
        } else {
            &mut self.top
        };
        *top = (*top)
            .min(at)
            .max(at.saturating_sub(height.saturating_sub(1)))
            .min(body.len().saturating_sub(height));
        let top = *top;
        let (position, count) = if let Some(choice) = self.choice {
            (choice + 1, self.choices().len())
        } else {
            let fields = self.fields();
            (
                fields.iter().position(|&i| i == self.row).unwrap() + 1,
                fields.len(),
            )
        };
        let mut lines = if self.choice.is_some() {
            vec![
                Line::from(vec![
                    Span::styled("config / ", dim()),
                    Span::styled(self.field().short, lit()),
                ]),
                Line::from(Span::styled(self.field().name, dim())),
                Line::from(Span::styled(
                    format!("↑↓ choose    {position} / {count}"),
                    dim(),
                )),
                Line::default(),
            ]
        } else {
            vec![
                Line::from(Span::styled("config", lit())),
                Line::from(self.tab_spans()),
                Line::from(Span::styled(
                    format!("[ ] group    {position}/{count}    * set"),
                    dim(),
                )),
                Line::default(),
            ]
        };
        if header == 2 {
            lines.remove(0);
            lines.truncate(2);
        } else if header == 0 {
            lines.clear();
        }
        lines.extend(body.into_iter().skip(top).take(height));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn control(&self, i: usize, width: usize) -> Vec<Span<'static>> {
        let (f, value) = (&FIELDS[i], &self.values[i]);
        if matches!(f.input, Answer::Columns) {
            return vec![Span::styled("open picker…  →", dim())];
        }
        if matches!(f.input, Answer::Check) {
            return vec![Span::styled("[ check ]", button())];
        }
        if i == self.row && self.open {
            let cursor = snap(value, self.cursor);
            let mut start = 0;
            let room = width.saturating_sub(4).max(1);
            while start < cursor && Span::raw(&value[start..cursor]).width() >= room {
                start += value[start..].chars().next().unwrap().len_utf8();
            }
            let mut spans = vec![Span::styled("[ ", lit())];
            spans.extend(fit(typed(&value[start..], cursor - start, f.builtin), room));
            spans.push(Span::styled(" ]", lit()));
            return spans;
        }
        let configured = !value.is_empty();
        let value = if value.is_empty() {
            outside(f.builtin).unwrap_or(f.builtin)
        } else {
            value
        };
        let value = f.display(value);
        let style = if i == self.row {
            lit()
        } else if configured {
            bold()
        } else {
            dim()
        };
        let edges = if f.picks().is_some() || f.step().is_some() {
            ("‹ ", " ›")
        } else {
            ("[ ", " ]")
        };
        let room = width.saturating_sub(if configured { 6 } else { 4 });
        let value = if room == 0 {
            String::new()
        } else {
            clip(value, room)
        };
        let mut spans = vec![
            Span::styled(edges.0, dim()),
            Span::styled(value, style),
            Span::styled(edges.1, dim()),
        ];
        if configured {
            spans.push(Span::styled(" *", lit()));
        }
        spans
    }

    fn default_label(&self) -> &str {
        let f = self.field();
        outside(f.builtin).unwrap_or_else(|| f.display(f.builtin))
    }

    fn details(&self) -> Paragraph<'static> {
        let f = self.field();
        let mut lines = vec![
            Line::from(Span::styled(format!("{} / {}", f.group, f.short), bold())),
            Line::from(Span::styled(f.name, dim())),
        ];
        if !matches!(f.input, Answer::Check) {
            lines.push(Line::from(format!("Default: {}", self.default_label())));
            if !self.values[self.row].is_empty() {
                lines.push(Line::from(format!(
                    "Set in config: {}",
                    f.display(&self.values[self.row])
                )));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(f.long));
        Paragraph::new(lines).wrap(Wrap { trim: false })
    }

    fn line(&self) -> Line<'static> {
        if let Some(error) = &self.error {
            return Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red)));
        }
        if let Some(note) = &self.note {
            return Line::from(Span::styled(note.clone(), plain()));
        }
        let f = self.field();
        // A row that runs something has no value, so it has no default to name either.
        if matches!(f.input, Answer::Check) {
            return Line::from(f.hint);
        }
        Line::from(vec![
            Span::raw(f.hint),
            Span::styled(
                format!(
                    "  {}: {}",
                    if self.values[self.row].is_empty() {
                        "Default"
                    } else {
                        "Reset"
                    },
                    self.default_label()
                ),
                dim(),
            ),
        ])
    }

    fn prompt_rows(&self, width: u16) -> u16 {
        // Two hint lines plus borders. Only a result or error can ask for more room.
        if self.error.is_some() || self.note.is_some() {
            (Paragraph::new(self.line())
                .wrap(Wrap { trim: false })
                .line_count(width)
                + 2)
            .clamp(4, 10) as u16
        } else {
            4
        }
    }

    fn hints(&self) -> Line<'static> {
        if self.help.is_some() {
            return hints(&[("↑↓", "scroll"), ("esc", "back")]);
        }
        if self.open {
            return hints(&[("enter", "keep"), ("esc", "revert")]);
        }
        if self.choice.is_some() {
            return hints(&[
                ("↑↓", "choice"),
                ("enter", "choose"),
                ("?", "help"),
                ("esc", "back"),
            ]);
        }
        if self.tabs {
            return hints(&[
                ("←→", "group"),
                ("↓", "fields"),
                ("?", "help"),
                ("esc", "done"),
            ]);
        }
        let f = self.field();
        let mut keys = vec![("↑↓", "field")];
        if matches!(f.input, Answer::Columns) {
            keys.push(("enter", "picker"));
        } else if matches!(f.input, Answer::Check) {
            keys.push(("enter", "run"));
        } else {
            if f.picks().is_some() || f.step().is_some() {
                keys.push(("←→", "change"));
            }
            keys.push((
                "enter",
                if f.picks().is_some() {
                    "choices"
                } else {
                    "type"
                },
            ));
            if !self.values[self.row].is_empty() {
                keys.push(("bksp", "reset"));
            }
        }
        keys.push(("?", "help"));
        keys.push(("esc", "done"));
        let width = if self.area.width == 0 {
            60
        } else {
            self.area.width
        } as usize;
        for omit in ["↑↓", "bksp", "←→"] {
            if hints(&keys).width() > width {
                keys.retain(|(key, _)| *key != omit);
            }
        }
        if hints(&keys).width() > width {
            for (key, label) in &mut keys {
                if *key == "enter" {
                    *label = "open";
                }
            }
        }
        hints(&keys)
    }

    fn mouse(&mut self, ev: MouseEvent) -> ConfigAction {
        if self.help.is_some() {
            if let Some(code) = match ev.kind {
                MouseEventKind::ScrollUp => Some(KeyCode::Up),
                MouseEventKind::ScrollDown => Some(KeyCode::Down),
                _ => None,
            } {
                for _ in 0..WHEEL_LINES {
                    self.key(code, KeyModifiers::NONE);
                }
            }
            return ConfigAction::Stay;
        }
        if self.open {
            return ConfigAction::Stay;
        }
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_LINES {
                    self.key(
                        if ev.kind == MouseEventKind::ScrollUp {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        },
                        KeyModifiers::NONE,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let x = ev.column.saturating_sub(self.area.x);
                let y = ev.row.saturating_sub(self.area.y);
                let header = self.header_rows();
                if self.choice.is_none() && ((header == 4 && y == 1) || (header == 2 && y == 0)) {
                    let mut left = 0;
                    for (i, (name, _)) in GROUPS.iter().enumerate() {
                        let right = left + name.len() as u16 + 2;
                        if (left..right).contains(&x) {
                            self.switch(i);
                            self.tabs = true;
                            return ConfigAction::Stay;
                        }
                        left = right + 1;
                    }
                } else if y >= header {
                    if self.choice.is_some() {
                        let at = self.choice_top + (y - header) as usize;
                        if at < self.choices().len() {
                            self.choice = Some(at);
                            return self.choose();
                        }
                    } else {
                        let at = self.top + (y - header) as usize;
                        let mut line = 0;
                        let mut sub = "";
                        for i in self.fields() {
                            let f = &FIELDS[i];
                            if !f.sub.is_empty() && f.sub != sub {
                                if line > 0 {
                                    line += 1;
                                }
                                line += 1;
                            }
                            sub = f.sub;
                            if line == at {
                                self.step(i);
                                if x as usize >= self.label_width(self.area.width) + 3 {
                                    return self.key(KeyCode::Enter, KeyModifiers::NONE);
                                }
                                break;
                            }
                            line += 1;
                        }
                    }
                }
            }
            _ => {}
        }
        ConfigAction::Stay
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

/// The launch probe for every harness the composer can start, as one line. This is the
/// check a launch makes, so a harness that answers here starts a session too.
fn connectivity() -> String {
    harness::launchable()
        .iter()
        .map(|&kind| {
            let name = kind.to_string();
            match harness::leave_and_return(kind) {
                Ok(_) => format!("{name} ok"),
                Err(e) => format!(
                    "{name} {}",
                    format!("{e:#}").trim_start_matches(&format!("{name} "))
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn config_field_visible(row: usize) -> bool {
    !matches!(
        FIELDS[row].name,
        "run_columns" | "job_columns" | "history_columns"
    )
}

fn built_column_set(key: &str) -> Vec<String> {
    config::column_set(key)
        .1
        .iter()
        .map(|c| (*c).to_owned())
        .collect()
}

fn built_columns() -> Vec<String> {
    config::DEFAULT_COLUMNS
        .iter()
        .map(|c| (*c).to_owned())
        .collect()
}

fn built_run_columns() -> Vec<String> {
    config::DEFAULT_RUN_COLUMNS
        .iter()
        .map(|c| (*c).to_owned())
        .collect()
}

const COLUMN_SETS: [(&str, &str); 4] = [
    ("columns", "sessions"),
    ("run_columns", "runs"),
    ("job_columns", "jobs"),
    ("history_columns", "history"),
];

/// Visibility is independent of row position, so toggling never moves the cursor.
#[derive(Debug, Clone, PartialEq)]
struct ColumnForm {
    order: Vec<String>,
    shown: HashSet<String>,
    at: usize,
    default: bool,
}

impl ColumnForm {
    fn new(key: &str, columns: Option<&[String]>) -> Self {
        let chosen = columns.map_or_else(
            || built_column_set(key),
            |columns| {
                columns
                    .iter()
                    .map(|c| config::column_name(c).to_owned())
                    .collect()
            },
        );
        let mut order = chosen.clone();
        order.extend(
            config::column_set(key)
                .0
                .iter()
                .filter(|c| !chosen.iter().any(|h| h == *c))
                .map(|c| (*c).to_owned()),
        );
        Self {
            shown: chosen.into_iter().collect(),
            at: 0,
            order,
            default: columns.is_none(),
        }
    }

    fn chosen(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|c| self.shown.contains(*c))
            .cloned()
            .collect()
    }

    fn selected(&self) -> &str {
        &self.order[self.at]
    }

    fn position(&self) -> Option<usize> {
        self.chosen().iter().position(|c| c == self.selected())
    }

    fn key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Up => self.at = self.at.saturating_sub(1),
            KeyCode::Down => self.at = (self.at + 1).min(self.order.len() - 1),
            KeyCode::Home => self.at = 0,
            KeyCode::End => self.at = self.order.len() - 1,
            KeyCode::Char(' ') => {
                let selected = self.selected().to_owned();
                if !self.shown.remove(&selected) {
                    self.shown.insert(selected);
                }
                self.default = false;
                return true;
            }
            KeyCode::Char('[' | ']') if self.shown.contains(self.selected()) => {
                let target = if code == KeyCode::Char('[') {
                    (0..self.at)
                        .rev()
                        .find(|&i| self.shown.contains(&self.order[i]))
                } else {
                    (self.at + 1..self.order.len()).find(|&i| self.shown.contains(&self.order[i]))
                };
                if let Some(target) = target {
                    self.order.swap(self.at, target);
                    self.at = target;
                    self.default = false;
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

enum ColumnAction {
    Stay,
    Close,
    /// Restore this snapshot if validation or writing fails.
    Save(ColumnForm),
}

struct ColumnsPicker {
    sets: [ColumnForm; 4],
    tab: usize,
    tabs: bool,
    return_config: Option<Box<ConfigForm>>,
    error: Option<String>,
    area: Rect,
    top: usize,
}

impl ColumnsPicker {
    fn new(path: &Path, tab: usize) -> Self {
        let values = [
            config::file_columns(path),
            config::file_run_columns(path),
            config::file_job_columns(path),
            config::file_history_columns(path),
        ];
        Self {
            sets: std::array::from_fn(|i| ColumnForm::new(COLUMN_SETS[i].0, values[i].as_deref())),
            tab: tab.min(3),
            tabs: false,
            return_config: None,
            error: None,
            area: Rect::default(),
            top: 0,
        }
    }

    fn current(&self) -> &ColumnForm {
        &self.sets[self.tab]
    }

    fn header_rows(&self) -> u16 {
        match self.area.height {
            0..=3 => 0,
            4..=7 => 2,
            _ => 5,
        }
    }

    fn key(&mut self, code: KeyCode) -> ColumnAction {
        self.error = None;
        if self.tabs {
            match code {
                KeyCode::Esc => return ColumnAction::Close,
                KeyCode::Down | KeyCode::Enter | KeyCode::Char(' ') => self.tabs = false,
                _ => {
                    if let Some(tab) = tab_key(code, self.tab, COLUMN_SETS.len()) {
                        self.tab = tab;
                    }
                }
            }
            return ColumnAction::Stay;
        }
        match code {
            KeyCode::Esc => return ColumnAction::Close,
            KeyCode::Up if self.current().at == 0 => self.tabs = true,
            KeyCode::Left => self.tab = self.tab.saturating_sub(1),
            KeyCode::Right => self.tab = (self.tab + 1).min(3),
            _ => {
                let before = self.current().clone();
                let page = self.area.height.saturating_sub(self.header_rows()).max(1) as usize;
                let form = &mut self.sets[self.tab];
                if code == KeyCode::Backspace && !form.default {
                    let selected = form.selected().to_owned();
                    *form = ColumnForm::new(COLUMN_SETS[self.tab].0, None);
                    form.at = form.order.iter().position(|c| *c == selected).unwrap_or(0);
                    return ColumnAction::Save(before);
                }
                let code = match code {
                    KeyCode::PageUp => {
                        form.at = form.at.saturating_sub(page);
                        return ColumnAction::Stay;
                    }
                    KeyCode::PageDown => {
                        form.at = (form.at + page).min(form.order.len() - 1);
                        return ColumnAction::Stay;
                    }
                    c => c,
                };
                if form.key(code) {
                    return ColumnAction::Save(before);
                }
            }
        }
        ColumnAction::Stay
    }

    fn line(&self) -> Line<'static> {
        let form = self.current();
        let state = form.position().map_or_else(
            || "hidden".to_owned(),
            |i| format!("shown · position {}", i + 1),
        );
        let mut spans = vec![
            Span::styled(format!("{} › ", form.selected()), lit()),
            Span::styled(state, dim()),
            Span::raw(format!(" · {}", column_help(form.selected()))),
        ];
        if let Some(error) = &self.error {
            spans = vec![Span::styled(error.clone(), Style::default().fg(Color::Red))];
        }
        Line::from(spans)
    }

    fn prompt_rows(&self, width: u16) -> u16 {
        self.current()
            .order
            .iter()
            .map(|name| {
                let text = format!("{name} › shown · position 13 · {}", column_help(name));
                (Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    + 2)
                .clamp(3, 10) as u16
            })
            .max()
            .unwrap_or(3)
    }

    fn hints(&self) -> Line<'static> {
        if self.tabs {
            return hints(&[("←→", "table"), ("↓", "columns"), ("esc", "back")]);
        }
        let form = self.current();
        let mut keys = vec![(
            "space",
            if form.shown.contains(form.selected()) {
                "hide"
            } else {
                "show"
            },
        )];
        if let Some(pos) = form.position() {
            if pos > 0 {
                keys.push(("[", "earlier"));
            }
            if pos + 1 < form.shown.len() {
                keys.push(("]", "later"));
            }
        }
        if !form.default {
            keys.push(("bksp", "reset"));
        }
        keys.push(("esc", "back"));
        hints(&keys)
    }

    fn tab_spans(&self) -> Vec<Span<'static>> {
        let mut spans = tab_buttons(
            COLUMN_SETS.iter().map(|&(_, label)| label),
            self.tab,
            self.tabs,
        );
        spans.push(Span::styled(
            if self.tabs {
                " ←→ table"
            } else {
                " ↑ table"
            },
            dim(),
        ));
        spans
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        let form = self.current();
        let header = self.header_rows();
        let height = area.height.saturating_sub(header) as usize;
        let top = form
            .at
            .saturating_sub(height.saturating_sub(1))
            .min(form.order.len().saturating_sub(height));
        let chosen = form.chosen();
        let mut lines = vec![
            Line::from(Span::styled("columns", lit())),
            Line::from(self.tab_spans()),
            Line::from(Span::styled(
                format!(
                    "{} of {} shown · {}    {} / {}",
                    chosen.len(),
                    form.order.len(),
                    if form.default { "defaults" } else { "custom" },
                    form.at + 1,
                    form.order.len(),
                ),
                dim(),
            )),
            Line::default(),
            Line::from(Span::styled("↑↓ show column          order", dim())),
        ];
        if header == 2 {
            lines = vec![Line::from(self.tab_spans()), lines.pop().unwrap()];
        } else if header == 0 {
            lines.clear();
        }
        for (i, name) in form.order.iter().enumerate().skip(top).take(height) {
            let selected = i == form.at && !self.tabs;
            let on = form.shown.contains(name);
            let pos = chosen
                .iter()
                .position(|c| c == name)
                .map(|n| format!("{:02}", n + 1))
                .unwrap_or_default();
            let mut row = Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, lit()),
                Span::styled(
                    if on { "[x]   " } else { "[ ]   " },
                    if on { plain() } else { dim() },
                ),
                Span::styled(
                    format!("{name:<16}"),
                    if selected { lit() } else { plain() },
                ),
                Span::styled(format!("{pos:<7}"), dim()),
            ]);
            if area.width > 48 {
                row.spans.push(Span::styled(
                    clip(column_help(name), area.width.saturating_sub(31) as usize),
                    dim(),
                ));
            }
            if selected {
                on_row(std::slice::from_mut(&mut row), area.width);
            }
            lines.push(row);
        }
        self.top = top;
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn mouse(&mut self, ev: MouseEvent) -> ColumnAction {
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                for _ in 0..WHEEL_LINES {
                    self.key(if ev.kind == MouseEventKind::ScrollUp {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let x = ev.column.saturating_sub(self.area.x);
                let y = ev.row.saturating_sub(self.area.y);
                let header = self.header_rows();
                if (header == 5 && y == 1) || (header == 2 && y == 0) {
                    let mut left = 0;
                    for (i, (_, label)) in COLUMN_SETS.iter().enumerate() {
                        let right = left + label.len() as u16 + 2;
                        if (left..right).contains(&x) {
                            self.tab = i;
                            self.tabs = true;
                            return ColumnAction::Stay;
                        }
                        left = right + 1;
                    }
                } else if y >= header {
                    let at = self.top + (y - header) as usize;
                    if at < self.current().order.len() {
                        self.tabs = false;
                        self.sets[self.tab].at = at;
                        if (2..5).contains(&x) {
                            return self.key(KeyCode::Char(' '));
                        }
                    }
                }
            }
            _ => {}
        }
        ColumnAction::Stay
    }
}

fn column_help(name: &str) -> &'static str {
    match name {
        "state" => "Session state, before the title.",
        "status" => "Status, before the job name.",
        "harness" => "Harness name beside its permanent icon.",
        "model" => "Model reported by the harness.",
        "context" => "Reported context usage and window.",
        "tokens" => "Reported input and output tokens.",
        "cost" => "Session or run cost; ~ marks an estimate.",
        "age" => "Time since the session started.",
        "last_active" => "Time since the latest activity.",
        "last_reply" => "Latest reply; an explicit choice keeps it beside the pane.",
        "activity" => "Activity over the configured chart window.",
        "folder" => "Working directory; sessions show it when grouped by state.",
        "branch" => "Current Git branch.",
        "started" => "Run start time, in local time.",
        "ended" => "Run end time, in local time.",
        "duration" => "Elapsed run time.",
        "reason" => "Why the run ended.",
        "trigger" => "How the run was started.",
        "schedule" => "Configured job schedule.",
        "next_run" => "Next scheduled local time.",
        "last_run" => "Most recent job run.",
        _ => "",
    }
}

enum Mode {
    Normal,
    Filter,
    Job(Box<JobForm>),
    Config(Box<ConfigForm>),
    Columns(Box<ColumnsPicker>),
    Folder(Input),
    Rename(Input),
    /// Search and scroll state for the keyboard guide.
    Guide(Guide),
}

/// Guide entries with an empty key are headings; tests check keys against docs/dashboard.md.
const GUIDE: &[(&str, &str)] = &[
    ("", "Rows"),
    (
        "↑ ↓",
        "Move between rows. Up past the first table reaches the menu.",
    ),
    (
        "enter",
        "Open the selected session or run, start a job, or enter a menu screen.",
    ),
    (
        "shift+enter",
        "Open a viewer over the full frame. While typing, insert a line break.",
    ),
    (
        "ctrl+x twice",
        "Stop a session or run; remove an idle job, finished run or pinned folder.",
    ),
    ("ctrl+e", "Edit the selected job."),
    ("ctrl+p", "Pin the selected folder so it stays in the list."),
    ("ctrl+s", "Group sessions by state or folder."),
    (
        "ctrl+h",
        "Show or hide history. Type to search; enter resumes a saved session.",
    ),
    (
        "ctrl+f",
        "Filter rows. History searches words and meaning; enter keeps the search, esc clears it.",
    ),
    ("ctrl+n", "Rename the selected Claude session."),
    (
        "ctrl+r",
        "Refresh now. The list also refreshes every second.",
    ),
    ("", "Composer"),
    (
        "any key",
        "Type an instruction. Enter starts a session in the selected folder.",
    ),
    (
        "shift+tab",
        "Choose Claude, Codex, pi, OpenCode or a terminal.",
    ),
    ("ctrl+v", "Paste a clipboard image into the instruction."),
    (
        "← →",
        "Move the text cursor. Alt moves by word; ctrl+a and ctrl+e jump to either end.",
    ),
    (
        "backspace",
        "Delete a character. Ctrl+w or alt+d deletes a word; ctrl+u or ctrl+k deletes to an end.",
    ),
    ("", "Viewers"),
    (
        "tab",
        "Focus the pane. From an empty supported prompt, return to the list.",
    ),
    (
        "ctrl+z",
        "Return to the list, keeping the viewer and its draft alive.",
    ),
    (
        "ctrl+\\",
        "Toggle the pane from the list; toggle fullscreen inside a viewer.",
    ),
    (
        "wheel",
        "Scroll the viewer or the history list under the pointer.",
    ),
    ("", "Config"),
    ("[ ]", "Switch between cones, harnesses and runs."),
    (
        "↑ ↓",
        "Select a setting. Up from the first setting reaches the group tabs.",
    ),
    (
        "← →",
        "Change a value immediately. Enter opens choices or text editing.",
    ),
    (
        "backspace",
        "Reset a setting to its default. An asterisk marks a value set in config.",
    ),
    (
        "?",
        "Read the selected setting’s full explanation. F1 also works while editing.",
    ),
    ("", "Columns"),
    ("↑ ↓", "Select a column."),
    ("← →", "Switch between sessions, runs, jobs and history."),
    (
        "space",
        "Show or hide the selected column. Changes save immediately.",
    ),
    ("[ ]", "Move a visible column earlier or later."),
    ("backspace", "Restore this table’s default columns."),
    ("", "Help"),
    (
        "/",
        "Search this guide by shortcut, topic or section. You can also just type.",
    ),
    (
        "page up",
        "Scroll up a page. Home goes to the first result.",
    ),
    (
        "page down",
        "Scroll down a page. End goes to the last result.",
    ),
    ("esc", "Clear a search first, then return to the list."),
    ("", "Leaving"),
    (
        "esc",
        "Back out of the current action, prompt, screen or dashboard.",
    ),
    (
        "ctrl+c twice",
        "Quit cones. In a terminal, ctrl+c interrupts the running command.",
    ),
    ("ctrl+g", "Open or close this guide."),
];

const HISTORY_PAGE: usize = 50;
const HISTORY_PREFETCH: usize = 10;

struct HistoryRow {
    key: String,
    entry: history::Entry,
}

struct HistoryBatch {
    after: Option<history::Cursor>,
    keys: HashSet<history::Key>,
}

struct HistoryFetch {
    revision: u64,
    after: Option<history::Cursor>,
    hydrate: bool,
    operation: Option<DiagnosticOperation>,
}

#[derive(Default)]
struct TranscriptView {
    reader: Option<transcript::Reader>,
    target: Option<transcript::Target>,
    document: Option<Arc<transcript::Transcript>>,
    error: Option<String>,
    requested: bool,
    since: Option<Instant>,
    focused: bool,
    lines: Vec<Line<'static>>,
    width: u16,
    height: usize,
    scroll: usize,
    bottom: bool,
    loaded_at: Option<Instant>,
    load_older: bool,
    load_newer: bool,
    jump_to_match: bool,
    request_cursor: Option<transcript::Cursor>,
    prepend_lines: Option<usize>,
    operation: Option<(DiagnosticOperation, transcript::Target)>,
}

impl TranscriptView {
    fn select(&mut self, target: Option<transcript::Target>) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.document = None;
        self.error = None;
        self.requested = false;
        self.since = Some(Instant::now());
        self.focused = false;
        self.lines.clear();
        self.width = 0;
        self.scroll = 0;
        self.bottom = true;
        self.loaded_at = None;
        self.load_older = false;
        self.load_newer = false;
        self.jump_to_match = true;
        self.request_cursor = None;
        self.prepend_lines = None;
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn scroll(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll());
        self.bottom = self.scroll == self.max_scroll();
        if delta < 0 && self.scroll <= self.height {
            self.load_older = true;
        }
        if delta > 0 && self.scroll == self.max_scroll() {
            self.load_newer = true;
        }
    }

    fn layout(&mut self, width: u16, height: u16, colors: &viewer::Colors) {
        if self.width != width {
            self.width = width;
            self.lines.clear();
            if let Some(doc) = &self.document {
                if doc.earlier {
                    self.lines.push(Line::styled(
                        if doc.older.is_some() {
                            "Scroll up for earlier messages"
                        } else {
                            "Earlier text omitted"
                        },
                        dim(),
                    ));
                    self.lines.push(Line::default());
                }
                let mut matched_line = None;
                for (i, message) in doc.messages.iter().enumerate() {
                    if doc.matched == Some(i) {
                        matched_line = Some(self.lines.len());
                    }
                    let harness = self.target.as_ref().map_or("", |t| t.harness.as_str());
                    self.lines
                        .extend(conversation_message(message, harness, width, colors));
                    self.lines.push(Line::default());
                }
                if let Some(at) = matched_line
                    && self.jump_to_match
                {
                    self.scroll = at;
                    self.bottom = false;
                    self.jump_to_match = false;
                }
                while self.lines.last().is_some_and(|line| line.width() == 0) {
                    self.lines.pop();
                }
                if doc.messages.is_empty() {
                    let empty = if self
                        .target
                        .as_ref()
                        .is_some_and(|t| matches!(t.source, transcript::Source::Run { .. }))
                    {
                        "No captured output for this run"
                    } else {
                        "No conversation text in this preview"
                    };
                    self.lines.push(Line::styled(empty, dim()));
                }
            } else if let Some(error) = &self.error {
                self.lines.extend(transcript_wrap(
                    &format!("Preview unavailable: {}", transcript::plain(error)),
                    width,
                ));
            }
        }
        self.height = usize::from(height);
        if let Some(previous) = self.prepend_lines.take()
            && !self.bottom
        {
            self.scroll += self.lines.len().saturating_sub(previous);
        }
        self.scroll = if self.bottom {
            self.max_scroll()
        } else {
            self.scroll.min(self.max_scroll())
        };
    }
}

/// Cache wrapped lines once per width, preserving newlines, indentation and Unicode graphemes.
fn transcript_wrap(text: &str, width: u16) -> Vec<Line<'static>> {
    transcript_wrap_lines(text.split('\n').map(Line::raw), width)
}

#[derive(Clone)]
struct ConversationStyle {
    pi: bool,
}

impl tui_markdown::StyleSheet for ConversationStyle {
    fn heading(&self, _level: u8) -> Style {
        if self.pi {
            bold().fg(Color::Rgb(240, 198, 116))
        } else {
            bold()
        }
    }
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }
    fn code(&self) -> Style {
        if self.pi {
            Style::default().fg(Color::Rgb(138, 190, 183))
        } else {
            Style::default()
        }
    }
    fn code_block_fence(&self) -> &str {
        ""
    }
    fn link(&self) -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        dim()
    }
    fn table_header(&self) -> Style {
        bold()
    }
}

#[cfg(test)]
fn transcript_markdown(text: &str, width: u16) -> Vec<Line<'static>> {
    transcript_markdown_for(text, width, false)
}

fn transcript_markdown_for(text: &str, width: u16, pi: bool) -> Vec<Line<'static>> {
    let options = tui_markdown::Options::new(ConversationStyle { pi });
    let rendered = tui_markdown::from_str_with_options(text, &options);
    let mut lines = transcript_wrap_lines(rendered.lines, width);
    while lines.first().is_some_and(|l| l.width() == 0) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    lines
}

fn conversation_message(
    message: &transcript::Message,
    harness: &str,
    width: u16,
    colors: &viewer::Colors,
) -> Vec<Line<'static>> {
    if message.role == transcript::Role::Output {
        return transcript_wrap(&message.text, width);
    }
    let user = message.role == transcript::Role::User;
    let marker = match (harness, user) {
        ("claude", true) => "❯ ",
        ("claude", false) => "⏺ ",
        ("codex", true) => "› ",
        ("codex", false) => "• ",
        ("pi", _) => "",
        (_, true) => "> ",
        (_, false) => "• ",
    };
    let gutter = if width > 2 {
        Span::raw(marker).width() as u16
    } else {
        0
    };
    let content_width = width.saturating_sub(gutter);
    let mut lines = if message.text.is_empty() {
        Vec::new()
    } else if user && harness != "pi" {
        transcript_wrap(&message.text, content_width)
    } else {
        transcript_markdown_for(&message.text, content_width, harness == "pi")
    };
    let marker_style = if harness == "claude" && !user {
        brand(harness)
    } else {
        dim()
    };
    for (i, line) in lines.iter_mut().enumerate() {
        if gutter > 0 {
            line.spans.insert(
                0,
                Span::styled(
                    if i == 0 {
                        marker.to_owned()
                    } else {
                        " ".repeat(usize::from(gutter))
                    },
                    marker_style,
                ),
            );
        }
    }
    if user && matches!(harness, "codex" | "pi") {
        let bg = conversation_prompt_background(harness, colors);
        let pad = || Line::styled(" ".repeat(usize::from(width)), Style::default().bg(bg));
        for line in &mut lines {
            line.spans.push(Span::raw(
                " ".repeat(usize::from(width).saturating_sub(line.width())),
            ));
            line.style = line.style.bg(bg);
        }
        lines.insert(0, pad());
        lines.push(pad());
    }
    for tool in &message.tools {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let detail = if tool.input.is_empty() {
            tool.name.clone()
        } else if harness == "claude" {
            format!("{}({})", tool.name, tool.input)
        } else {
            format!("{} {}", tool.name, tool.input)
        };
        for (i, mut line) in transcript_wrap(&detail, content_width)
            .into_iter()
            .enumerate()
        {
            line.style = dim();
            if gutter > 0 {
                line.spans.insert(
                    0,
                    Span::styled(
                        if i == 0 {
                            marker.to_owned()
                        } else {
                            " ".repeat(usize::from(gutter))
                        },
                        marker_style,
                    ),
                );
            }
            lines.push(line);
        }
    }
    lines
}

fn conversation_prompt_background(harness: &str, colors: &viewer::Colors) -> Color {
    let channels: Vec<_> = colors
        .bg
        .strip_prefix("rgb:")
        .unwrap_or("")
        .split('/')
        .filter_map(|s| u16::from_str_radix(s, 16).ok().map(|v| (v / 257) as u8))
        .collect();
    let [r, g, b] = channels.as_slice() else {
        return Color::Reset;
    };
    let light = u16::from(*r) + u16::from(*g) + u16::from(*b) > 3 * 128;
    if harness == "pi" && !light {
        return Color::Rgb(52, 53, 65);
    }
    let tint = |channel: u8| {
        if light {
            (u16::from(channel) * 96 / 100) as u8
        } else {
            (u16::from(channel) * 88 / 100 + 255 * 12 / 100) as u8
        }
    };
    Color::Rgb(tint(*r), tint(*g), tint(*b))
}

fn transcript_wrap_lines<'a>(
    lines: impl IntoIterator<Item = Line<'a>>,
    width: u16,
) -> Vec<Line<'static>> {
    fn whitespace(span: &Span<'_>) -> bool {
        span.content.chars().all(char::is_whitespace)
    }
    fn line(mut spans: Vec<Span<'static>>) -> Line<'static> {
        while spans.last().is_some_and(whitespace) {
            spans.pop();
        }
        let mut merged: Vec<Span<'static>> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut()
                && last.style == span.style
            {
                last.content.to_mut().push_str(&span.content);
            } else {
                merged.push(span);
            }
        }
        Line::from(merged)
    }
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for source in lines {
        let first = out.len();
        let mut buffer = Vec::new();
        let mut used = 0;
        for g in source.styled_graphemes(source.style) {
            let w = Span::raw(g.symbol).width();
            if used + w > width && !buffer.is_empty() {
                if g.is_whitespace() {
                    out.push(line(std::mem::take(&mut buffer)));
                    used = 0;
                    continue;
                }
                if let Some(at) = buffer
                    .iter()
                    .rposition(whitespace)
                    .filter(|&i| buffer[..i].iter().any(|s| !whitespace(s)))
                {
                    let rest = buffer.split_off(at);
                    out.push(line(std::mem::take(&mut buffer)));
                    buffer = rest.into_iter().skip_while(whitespace).collect();
                    used = buffer.iter().map(Span::width).sum();
                } else {
                    out.push(line(std::mem::take(&mut buffer)));
                    used = 0;
                }
            }
            buffer.push(Span::styled(g.symbol.to_owned(), g.style));
            used += w;
        }
        if !buffer.is_empty() || out.len() == first {
            out.push(line(buffer));
        }
    }
    out
}

#[derive(Default)]
struct HistoryView {
    visible: bool,
    reader: Option<history::Reader>,
    rows: Vec<HistoryRow>,
    batches: Vec<HistoryBatch>,
    next: Option<history::Cursor>,
    fetch: Option<HistoryFetch>,
    revision: u64,
    first: bool,
    refresh: bool,
    ready: bool,
    filter: String,
    filter_rest: Option<Instant>,
    error: Option<String>,
    search_pending: bool,
    search_status: Option<String>,
    updated: Option<Instant>,
    select_first: bool,
    return_to: Option<String>,
    homes: HashMap<PathBuf, PathBuf>,
    widths: Widths,
    /// Retained independently of loaded pages so a resumed viewer survives hiding history.
    opened: HashMap<String, history::Entry>,
}

fn history_key(key: &history::Key) -> String {
    format!("history:{}:{:?}:{}", key.harness, key.home, key.session_id)
}

/// Reuse column formatting without putting history into the live fleet or its counters.
fn history_session(entry: &history::Entry) -> Session {
    let c = entry.columns.clone().unwrap_or_default();
    Session {
        session_id: entry.key.session_id.clone(),
        harness: entry.key.harness.clone(),
        kind: None,
        cwd: entry.cwd.clone(),
        state: "-".into(),
        started: entry.started,
        last_activity: entry.last_activity,
        model: c.model,
        pid: None,
        transcript_path: Some(entry.transcript.clone()),
        tokens_in: c.tokens_in,
        tokens_out: c.tokens_out,
        context_tokens: c.context_tokens,
        context_window: c.context_window,
        cost_usd: c.cost_usd,
        cost_info: c.cost_info,
        title: entry.title.clone(),
        last: Some(c.last.unwrap_or_else(|| "-".into())),
        coordinator: false,
        activity: Vec::new(),
    }
}

/// Build native resume commands on the preparation thread; opening history alone does nothing.
fn history_command(entry: &history::Entry) -> Result<Command> {
    harness::resume_history(entry)
}

impl HistoryView {
    fn reset(&mut self, filter: &str, refresh: bool) {
        self.revision += 1;
        self.rows.clear();
        self.batches.clear();
        self.next = None;
        self.first = true;
        self.refresh |= refresh;
        self.ready = false;
        self.error = None;
        self.search_pending = false;
        self.search_status = None;
        self.updated = None;
        self.filter = filter.to_owned();
        self.filter_rest = Some(Instant::now());
    }

    fn row(&self, key: &str) -> Option<&history::Entry> {
        self.rows
            .iter()
            .find(|r| r.key == key)
            .map(|r| &r.entry)
            .or_else(|| self.opened.get(key))
    }

    fn table(&mut self, data: &Data, excluded: &HashSet<history::Key>) -> Vec<Row> {
        if !self.visible {
            return Vec::new();
        }
        let mut rows = vec![
            Row {
                kind: Kind::Blank,
                cells: vec![],
            },
            Row {
                kind: Kind::Header,
                cells: vec![("history".into(), bold())],
            },
        ];
        let shown: Vec<&HistoryRow> = self
            .rows
            .iter()
            .filter(|r| !excluded.contains(&r.entry.key))
            .collect();
        if !shown.is_empty() {
            let mut names = vec!["", ""];
            names.push("title");
            names.extend(
                data.history_columns
                    .iter()
                    .filter(|c| *c != "state" && *c != "harness")
                    .map(|c| column_label(c)),
            );
            let cells = shown
                .iter()
                .map(|r| {
                    session_cells(
                        &history_session(&r.entry),
                        &data.history_columns,
                        false,
                        Some("-"),
                        None,
                    )
                })
                .collect();
            let (head, cells) = columns(&names, cells, &mut self.widths);
            rows.push(head);
            for (r, cells) in shown.iter().zip(cells) {
                rows.push(Row {
                    kind: Kind::History(r.key.clone()),
                    cells,
                });
                if let Some(hit) = &r.entry.hit
                    && !hit.snippet.is_empty()
                {
                    let mut cells =
                        vec![(if hit.semantic { "    ≈ " } else { "    " }.into(), dim())];
                    cells.extend(highlight_search(&hit.snippet, &self.filter));
                    rows.push(Row {
                        kind: Kind::HistoryStatus,
                        cells,
                    });
                }
            }
        }
        let status = if let Some(error) = &self.error {
            Some(format!("history unavailable: {error} · ctrl+r retries"))
        } else if !self.ready || self.fetch.as_ref().is_some_and(|f| !f.hydrate) {
            Some("loading history".into())
        } else if self.search_status.is_some() {
            self.search_status.clone()
        } else if shown.is_empty() {
            Some("no matching history".into())
        } else {
            None
        };
        if let Some(status) = status {
            rows.push(Row {
                kind: Kind::HistoryStatus,
                cells: vec![(status, dim())],
            });
        }
        rows
    }
}

fn highlight_search(text: &str, query: &str) -> Vec<(String, Style)> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut cells: Vec<(String, Style)> = Vec::new();
    for piece in text.split_inclusive(char::is_whitespace) {
        let matched = words.iter().any(|word| piece.to_lowercase().contains(word));
        let style = if matched {
            Style::default().fg(Color::Yellow)
        } else {
            dim()
        };
        if let Some((text, previous)) = cells.last_mut()
            && *previous == style
        {
            text.push_str(piece);
        } else {
            cells.push((piece.into(), style));
        }
    }
    cells
}

#[derive(Clone)]
struct Diagnostics {
    path: PathBuf,
    dashboard_id: String,
    trace: bool,
}

impl Diagnostics {
    fn new(path: PathBuf, trace: bool) -> Self {
        Self {
            path,
            dashboard_id: uuid::Uuid::new_v4().to_string(),
            trace,
        }
    }

    fn event(&self, level: &str, event: &str, mut data: Value) {
        let context = data
            .get("row")
            .or_else(|| data.get("viewer"))
            .or_else(|| data.get("after"))
            .or_else(|| data.get("before"))
            .cloned()
            .unwrap_or(Value::Null);
        for field in [
            "operation_id",
            "row_kind",
            "row_id",
            "harness",
            "session_id",
            "viewer_pid",
            "source",
        ] {
            if data.get(field).is_none_or(Value::is_null)
                && let Some(value) = context.get(field)
            {
                data[field] = value.clone();
            }
        }
        let _ = debug_line(
            &self.path,
            json!({
                "v": 1,
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "pid": std::process::id(),
                "dashboard_id": self.dashboard_id,
                "level": level,
                "event": event,
                "data": data,
            }),
        );
    }

    fn trace(&self, event: &str, data: impl FnOnce() -> Value) {
        if self.trace {
            self.event("trace", event, data());
        }
    }
}

#[derive(Clone)]
struct DiagnosticOperation {
    id: String,
    started: Instant,
}

impl DiagnosticOperation {
    fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            started: Instant::now(),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
}

#[derive(Default)]
struct TimingSummary {
    count: u64,
    total_ms: f64,
    max_ms: f64,
}

#[derive(Default)]
struct LoadDiagnostics {
    phases: BTreeMap<String, f64>,
    sources: HashMap<String, Value>,
    excluded: HashMap<String, &'static str>,
    warnings: Vec<String>,
}

impl LoadDiagnostics {
    fn phase(&mut self, name: &str, started: Instant) {
        self.phases
            .insert(name.to_owned(), started.elapsed().as_secs_f64() * 1000.0);
    }
}

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
    claude: PathBuf,
    /// Fallback launch directory and base for relative folder input.
    cwd: PathBuf,
    /// Index into `MENU`.
    menu: usize,
    /// Last table visited before moving onto the menu.
    column_context: usize,
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
    history: HistoryView,
    transcript: TranscriptView,
    mode: Mode,
    status: String,
    /// Composer text; `caret` is a byte offset.
    text: String,
    caret: usize,
    /// The PNGs pasted into the instruction, in the order their markers were typed.
    images: Vec<PathBuf>,
    /// Index into `harness::launchable()`, followed by the terminal option.
    harness: usize,
    shell: PathBuf,
    shell_startup: Option<tempfile::TempDir>,
    terminal_input: Input,
    /// Shell rows belong to this dashboard and have no harness registry.
    terminals: Vec<Session>,
    /// Background launches keyed by placeholder row id.
    started: Vec<(String, mpsc::Receiver<Launched>)>,
    /// Immediate rows until discovery reports the launched sessions.
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
    log: Option<Diagnostics>,
    dashboard_id: String,
    timing_summary: RefCell<BTreeMap<String, TimingSummary>>,
    summary_at: Instant,
    diagnostic_view: Option<Value>,
    diagnostic_peek: Option<Value>,
    diagnostic_rows: HashMap<String, Value>,
    diagnostic_warnings: Vec<String>,
    loading_operation: Option<DiagnosticOperation>,
    input_operation: Option<(DiagnosticOperation, Value)>,
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
/// with memory to spare. Evict only listed-session Claude attaches; a Codex client is peeked the
/// same way but stays once entered, and other clients may exceed this cap too.
const MAX_FOCUSED_VIEWERS: usize = 3;

/// Two speculative slots avoid reattaching when moving between adjacent rows. They are held on top
/// of `MAX_FOCUSED_VIEWERS` rather than inside it, so raising either constant costs a whole client.
const SPECULATIVE_VIEWERS: usize = 2;

/// Launch status and, on failure, the prompt to restore.
type Launched = (String, Option<String>);

/// Match a launch by Claude's returned id or the foreground viewer's child pid.
struct Pending {
    session: Session,
    short: Option<String>,
    at: Instant,
}

impl Pending {
    fn matches(&self, s: &Session) -> bool {
        s.harness == self.session.harness
            && s.cwd == self.session.cwd
            && (self.session.pid.is_some_and(|pid| s.pid == Some(pid))
                || (harness::by_name(&s.harness).is_some_and(|spec| {
                    spec.launch.as_ref().is_some_and(|launch| {
                        launch.identity == harness::spec::LaunchIdentity::BackgroundId
                    })
                }) && self
                    .short
                    .as_deref()
                    .is_some_and(|short| s.session_id.starts_with(short))))
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

fn placeholder(kind: HarnessKind, id: &str, dir: &Path, prompt: &str) -> Session {
    Session {
        session_id: id.to_owned(),
        harness: kind.to_string(),
        kind: harness::spec(kind)
            .launch
            .as_ref()
            .and_then(|launch| launch.session_kind.clone()),
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
        cost_info: None,
        title: fleet::headline(prompt),
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
    /// Native input behavior follows the launch/session identity, never the viewer's title.
    harness: Option<HarnessKind>,
    viewer: Viewer,
    /// A Codex thread to record from its rollout once the viewer is left or ends.
    record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    recorded: bool,
    first_paint_logged: bool,
    /// For a speculative viewer, which was never focused, this is when it was spawned.
    last_focused: Instant,
    /// Unfocused attaches use the separate speculative pool until first focus.
    speculative: bool,
    operation: Option<DiagnosticOperation>,
}

impl Open {
    fn is_terminal(&self) -> bool {
        self.key.starts_with("terminal:")
    }

    fn returns_to_list(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        self.harness.map_or_else(
            || {
                (mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('z'))
                    || (!self.is_terminal() && code == KeyCode::Tab && mods.is_empty())
            },
            |kind| {
                viewer::returns_to_list(
                    self.viewer.screen(),
                    &harness::spec(kind).input,
                    code,
                    mods,
                )
            },
        )
    }

    fn return_key(&self) -> &'static str {
        if self.returns_to_list(KeyCode::Tab, KeyModifiers::NONE) {
            "tab"
        } else if self.returns_to_list(KeyCode::Left, KeyModifiers::NONE) {
            "←"
        } else {
            "ctrl+z"
        }
    }
}

struct PendingStop {
    id: String,
    label: String,
    verb: &'static str,
    result: mpsc::Receiver<Result<bool>>,
    operation: Option<DiagnosticOperation>,
    context: Value,
}

struct Opening {
    what: String,
    key: String,
    command: mpsc::Receiver<Result<Command>>,
    record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    prompt: Option<String>,
    operation: Option<DiagnosticOperation>,
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
    #[cfg(test)]
    fn new(exe: &Path, jobs_path: &Path, state: &Path, claude: &Path) -> Result<Self> {
        Self::new_logged(exe, jobs_path, state, claude, None)
    }

    fn new_logged(
        exe: &Path,
        jobs_path: &Path,
        state: &Path,
        claude: &Path,
        log: Option<Diagnostics>,
    ) -> Result<Self> {
        let operation = log.as_ref().map(|_| DiagnosticOperation::new());
        let data = Data::load_observed(jobs_path, state, claude, log.as_ref(), operation.as_ref())?;
        let start = data.start;
        let dashboard_id = log
            .as_ref()
            .map(|l| l.dashboard_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Ok(Self {
            exe: exe.to_owned(),
            jobs_path: jobs_path.to_owned(),
            state: state.to_owned(),
            claude: claude.to_owned(),
            cwd: std::env::current_dir().context("dashboard working directory")?,
            menu: 0,
            column_context: 0,
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
            history: HistoryView::default(),
            transcript: TranscriptView::default(),
            mode: Mode::Normal,
            status: String::new(),
            text: String::new(),
            caret: 0,
            images: Vec::new(),
            harness: Self::harness_at(Some(start.harness), &config::defaults(jobs_path)),
            shell: terminal::default_shell(),
            shell_startup: None,
            terminal_input: Input::default(),
            terminals: Vec::new(),
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
            log,
            dashboard_id,
            timing_summary: RefCell::new(BTreeMap::new()),
            summary_at: Instant::now(),
            diagnostic_view: None,
            diagnostic_peek: None,
            diagnostic_rows: HashMap::new(),
            diagnostic_warnings: Vec::new(),
            loading_operation: operation,
            input_operation: None,
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
        self.event("debug", "message", || json!({"message": msg()}));
    }

    fn event(&self, level: &str, event: &str, data: impl FnOnce() -> Value) {
        if let Some(log) = &self.log {
            log.event(level, event, data());
        }
    }

    fn timing(&self, phase: &str, started: Instant) {
        self.measured(
            phase,
            started.elapsed().as_secs_f64() * 1000.0,
            || json!({}),
        );
    }

    fn measured(&self, phase: &str, ms: f64, context: impl FnOnce() -> Value) {
        let Some(log) = &self.log else { return };
        let mut summary = self.timing_summary.borrow_mut();
        let entry = summary.entry(phase.to_owned()).or_default();
        entry.count += 1;
        entry.total_ms += ms;
        entry.max_ms = entry.max_ms.max(ms);
        drop(summary);
        let limit = if phase.contains("draw") || phase == "viewer_pump" {
            16.0
        } else {
            250.0
        };
        if log.trace || ms >= limit {
            let mut data = context();
            data["phase"] = json!(phase);
            data["duration_ms"] = json!(ms);
            data["slow"] = json!(ms >= limit);
            log.event(if ms >= limit { "debug" } else { "trace" }, "timing", data);
        }
    }

    fn summarize_timings(&mut self, force: bool) {
        if self.log.is_none() || (!force && self.summary_at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        let phases: BTreeMap<_, _> = std::mem::take(&mut *self.timing_summary.borrow_mut())
            .into_iter()
            .map(|(name, s)| {
                (name, json!({"count": s.count, "mean_ms": s.total_ms / s.count as f64, "max_ms": s.max_ms}))
            })
            .collect();
        if !phases.is_empty() {
            self.event("debug", "timing.summary", || {
                json!({
                    "interval_ms": self.summary_at.elapsed().as_secs_f64() * 1000.0,
                    "phases": phases,
                })
            });
        }
        self.summary_at = Instant::now();
    }

    fn row_context(&self, kind: &Kind) -> Value {
        let mut data = json!({"row_kind": kind.diagnostic_name(), "row_id": kind.key()});
        match kind {
            Kind::Session(id, state) => {
                data["state"] = json!(state);
                if let Some(s) = self.data.sessions.iter().find(|s| &s.session_id == id) {
                    data["harness"] = json!(s.harness);
                    data["session_id"] = json!(s.session_id);
                    data["native_kind"] = json!(s.kind);
                    data["pid"] = json!(s.pid);
                    data["source"] = if self.terminals.iter().any(|t| t.session_id == *id) {
                        json!({"reader": "owned_terminal"})
                    } else {
                        self.data
                            .diagnostics
                            .as_ref()
                            .and_then(|d| d.sources.get(&format!("{}:{}", s.harness, id)))
                            .cloned()
                            .unwrap_or(Value::Null)
                    };
                }
            }
            Kind::Run(id, state) => {
                data["state"] = json!(state);
                data["source"] = json!("ledger");
                if let Some(r) = self.data.runs.iter().find(|r| &r.started.run_id == id) {
                    data["harness"] = json!(r.started.harness);
                    data["session_id"] = json!(r.started.session_id);
                }
            }
            Kind::History(key) => {
                data["source"] = json!("history");
                if let Some(r) = self.history.row(key) {
                    return Self::history_context(key, r);
                }
            }
            Kind::Job(name) => {
                data["source"] = json!("jobs");
                if let Some(job) = self.data.jobs.iter().find(|j| &j.name == name) {
                    data["harness"] = json!(job.harness);
                }
            }
            Kind::Folder(_) => data["source"] = json!("pinned_folder"),
            _ => {}
        }
        data
    }

    fn history_context(key: &str, entry: &history::Entry) -> Value {
        json!({
            "row_kind": "history", "row_id": key, "source": "history",
            "harness": entry.key.harness, "session_id": entry.key.session_id,
            "native_home": entry.key.home.to_string_lossy(), "archived": entry.archived,
        })
    }

    fn key_context(&self, key: &str) -> Value {
        self.rows
            .iter()
            .chain(&self.other)
            .find(|r| r.kind.key() == Some(key))
            .map(|r| self.row_context(&r.kind))
            .unwrap_or_else(|| json!({"row_id": key}))
    }

    fn view_context(&self) -> Value {
        let mode = match self.mode {
            Mode::Normal => "normal",
            Mode::Filter => "filter",
            Mode::Job(_) => "job",
            Mode::Config(_) => "config",
            Mode::Columns(_) => "columns",
            Mode::Folder(_) => "folder",
            Mode::Rename(_) => "rename",
            Mode::Guide(..) => "guide",
        };
        json!({
            "mode": mode,
            "selected": self.selected().map(|r| self.row_context(&r.kind)),
            "viewer_pid": self.focus.map(|i| self.viewers[i].viewer.pid()),
            "viewer_key": self.focus.map(|i| &self.viewers[i].key),
            "transcript_focused": self.transcript.focused,
            "split": self.split,
            "full": self.full,
            "jobs_view": self.jobs_view,
            "cursor": self.cursor,
            "scroll": self.scroll,
            "status": self.status,
        })
    }

    fn report_view(&mut self, reason: &str) {
        if self.log.is_none() {
            return;
        }
        let after = self.view_context();
        if self.diagnostic_view.as_ref() != Some(&after) {
            self.event("debug", "view.changed", || {
                json!({
                    "reason": reason, "before": self.diagnostic_view, "after": after,
                })
            });
            self.diagnostic_view = Some(after);
        }
    }

    fn report_rows(&mut self, reason: &str) {
        if self.log.is_none() {
            return;
        }
        let visible: HashSet<_> = self.visible.iter().copied().collect();
        let history: HashMap<_, _> = self
            .history
            .opened
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
            .chain(self.history.rows.iter().map(|r| (r.key.as_str(), &r.entry)))
            .collect();
        let mut next = HashMap::new();
        for (active, rows) in [(true, &self.rows), (false, &self.other)] {
            for (i, row) in rows.iter().enumerate() {
                let Some(key) = row.kind.key() else { continue };
                let mut context = match &row.kind {
                    Kind::History(key) if history.contains_key(key.as_str()) => {
                        Self::history_context(key, history[key.as_str()])
                    }
                    _ => self.row_context(&row.kind),
                };
                context["visible"] = json!(active && visible.contains(&i));
                next.insert(format!("{}:{key}", row.kind.diagnostic_name()), context);
            }
        }
        for (key, after) in &next {
            let before = self.diagnostic_rows.get(key);
            if before != Some(after) {
                self.event(
                    "debug",
                    if before.is_some() {
                        "row.changed"
                    } else {
                        "row.added"
                    },
                    || {
                        json!({
                            "reason": reason, "before": before, "after": after,
                            "operation_id": if matches!(reason, "refresh" | "startup") {
                                self.loading_operation.as_ref().map(|o| &o.id)
                            } else {
                                self.input_operation.as_ref().map(|(o, _)| &o.id)
                            },
                        })
                    },
                );
            }
        }
        for (key, before) in &self.diagnostic_rows {
            if !next.contains_key(key) {
                let detail = before["session_id"]
                    .as_str()
                    .and_then(|id| self.data.diagnostics.as_ref()?.excluded.get(id))
                    .copied()
                    .unwrap_or(reason);
                self.event(
                    "debug",
                    "row.removed",
                    || json!({"reason": detail, "before": before}),
                );
            }
        }
        self.diagnostic_rows = next;
    }

    fn log_input(&mut self, event: &Event) {
        let Some(log) = &self.log else { return };
        let operation = DiagnosticOperation::new();
        let context = self.view_context();
        let mut data = json!({"operation_id": operation.id, "route": context});
        match event {
            Event::Key(k) => {
                let plain_text = matches!(k.code, KeyCode::Char(_))
                    && !k.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    );
                if log.trace || !plain_text {
                    data["key"] = json!(format!("{:?}", k.code));
                    data["modifiers"] = json!(format!("{:?}", k.modifiers));
                    data["kind"] = json!(format!("{:?}", k.kind));
                    log.event(
                        if plain_text { "trace" } else { "debug" },
                        "input.key",
                        data,
                    );
                }
            }
            Event::Paste(text) => {
                data["bytes"] = json!(text.len());
                data["empty"] = json!(text.is_empty());
                if log.trace {
                    data["text"] = json!(text);
                }
                log.event("debug", "input.paste", data);
            }
            Event::Mouse(m) => {
                if log.trace || matches!(m.kind, MouseEventKind::Down(_)) {
                    data["mouse"] = json!(format!("{m:?}"));
                    log.event(
                        if log.trace { "trace" } else { "debug" },
                        "input.mouse",
                        data,
                    );
                }
            }
            Event::Resize(width, height) => {
                data["width"] = json!(width);
                data["height"] = json!(height);
                log.event("debug", "terminal.resize", data);
            }
            _ => log.trace("input.other", || json!({"event": format!("{event:?}")})),
        }
        self.input_operation = Some((operation, context));
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
        let operation = log.as_ref().map(|_| DiagnosticOperation::new());
        self.loading_operation = operation.clone();
        if let Some(log) = &log {
            log.trace(
                "refresh.started",
                || json!({"operation_id": operation.as_ref().map(|o| &o.id)}),
            );
        }
        std::thread::spawn(move || {
            let data =
                Data::load_observed(&jobs, &state, &claude, log.as_ref(), operation.as_ref());
            let _ = tx.send(data);
        });
        self.loading = Some(rx);
        self.loading_started = Some(started);
    }

    fn report_load(&mut self) {
        let Some(d) = &self.data.diagnostics else {
            return;
        };
        for (phase, ms) in &d.phases {
            self.measured(phase, *ms, || {
                json!({
                    "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                })
            });
        }
        for warning in &d.warnings {
            if !self.diagnostic_warnings.contains(warning) {
                self.event(
                    "error",
                    "configuration.failed",
                    || json!({"error": warning}),
                );
            }
        }
        if d.warnings.is_empty() && !self.diagnostic_warnings.is_empty() {
            self.event("debug", "configuration.recovered", || json!({}));
        }
        self.diagnostic_warnings = d.warnings.clone();
    }

    /// Invalidate an in-flight read without starting a second reader.
    fn invalidate(&mut self) {
        if self.loading.is_some() {
            self.reload_pending = true;
        } else {
            self.reload();
        }
    }

    fn session_history_key(&self, s: &Session) -> Option<history::Key> {
        let home = harness::by_name(&s.harness)?.session_home(&self.claude, s);
        Some(history::Key {
            harness: s.harness.clone(),
            home: self.history.homes.get(&home).cloned().unwrap_or(home),
            session_id: s.session_id.clone(),
        })
    }

    fn history_excluded(&self) -> HashSet<history::Key> {
        let mut keys: HashSet<_> = self
            .data
            .sessions
            .iter()
            .filter_map(|s| self.session_history_key(s))
            .collect();
        for (key, entry) in &self.history.opened {
            if self
                .data
                .sessions
                .iter()
                .any(|s| self.history_matches(key, s))
            {
                keys.insert(entry.key.clone());
            }
        }
        for run in &self.data.runs {
            if let (Some(HarnessKind::Claude), Some(id)) =
                (run.started.harness, &run.started.session_id)
            {
                keys.insert(history::Key {
                    harness: "claude".into(),
                    home: self
                        .history
                        .homes
                        .get(&self.claude)
                        .cloned()
                        .unwrap_or_else(|| self.claude.clone()),
                    session_id: id.clone(),
                });
            }
        }
        keys
    }

    fn history_matches(&self, key: &str, s: &Session) -> bool {
        let Some(entry) = self.history.opened.get(key) else {
            return false;
        };
        if entry.key.harness != s.harness {
            return false;
        }
        if s.harness == "pi" {
            return self
                .viewer_index(key)
                .is_some_and(|i| Some(self.viewers[i].viewer.pid()) == s.pid);
        }
        self.session_history_key(s).as_ref() == Some(&entry.key)
    }

    fn history_selected(&self) -> bool {
        self.history.visible
            && !self.jobs_view
            && (self.history.select_first
                || matches!(
                    self.selected().map(|r| &r.kind),
                    Some(Kind::History(_) | Kind::HistoryStatus)
                ))
    }

    fn toggle_history(&mut self) {
        let from_history = self.history_selected();
        self.history.visible = !self.history.visible;
        if self.history.visible {
            self.history.return_to = self
                .selected()
                .and_then(|r| r.kind.key().map(str::to_owned));
            self.history.select_first = self.composer_text().is_empty();
            self.history.reset(&self.filter.text, true);
        } else {
            self.history.revision += 1;
            self.history.select_first = false;
        }
        self.rebuild_with_reason("history_visibility");
        if !self.history.visible
            && from_history
            && let Some(key) = &self.history.return_to
            && let Some(i) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].kind.key() == Some(key.as_str()))
        {
            self.cursor = i;
            self.settle();
        }
    }

    fn history_viewport(&self) -> HashSet<history::Key> {
        self.visible
            .iter()
            .skip(self.scroll)
            .take(usize::from(self.list_area.height.max(1)))
            .filter_map(|&i| match &self.rows[i].kind {
                Kind::History(key) => self.history.row(key).map(|e| e.key.clone()),
                _ => None,
            })
            .collect()
    }

    fn transcript_target(&self) -> Option<transcript::Target> {
        if self.focus.is_some() || self.panel().is_some() {
            return None;
        }
        let row = self.selected()?;
        if self.viewer_of(&row.kind).is_some() {
            return None;
        }
        match &row.kind {
            Kind::History(key) if self.history.visible => {
                let entry = self.history.row(key)?;
                let source = if entry.key.harness == "opencode" {
                    transcript::Source::Opencode {
                        database: entry.transcript.clone(),
                        session_id: entry.key.session_id.clone(),
                    }
                } else {
                    transcript::Source::Conversation(entry.transcript.clone())
                };
                Some(transcript::Target {
                    key: key.clone(),
                    harness: entry.key.harness.clone(),
                    source: if let Some(anchor) =
                        entry.hit.as_ref().and_then(|hit| hit.anchor.clone())
                    {
                        transcript::Source::Match {
                            source: Box::new(source),
                            anchor,
                        }
                    } else {
                        source
                    },
                })
            }
            Kind::Run(id, _) => {
                let run = self.data.runs.iter().find(|r| &r.started.run_id == id)?;
                let source = if run.started.output.is_none()
                    && run.started.stderr.is_none()
                    && let Some(path) = run.terminal.as_ref().and_then(|r| r.transcript.as_ref())
                {
                    transcript::Source::Conversation(path.clone())
                } else {
                    transcript::Source::Run {
                        events: run.started.output.clone(),
                        stderr: run.started.stderr.clone(),
                    }
                };
                Some(transcript::Target {
                    key: format!("run:{id}"),
                    harness: run
                        .started
                        .harness
                        .unwrap_or(HarnessKind::Claude)
                        .to_string(),
                    source,
                })
            }
            _ => None,
        }
    }

    fn transcript_shown(&self) -> bool {
        (self.split_active() || self.transcript.focused) && self.transcript_target().is_some()
    }

    fn focus_transcript(&mut self) {
        if let Some(target) = self.transcript_target() {
            self.transcript.select(Some(target));
            self.transcript.focused = true;
            self.status.clear();
        }
    }

    fn leave_transcript(&mut self) {
        if self.transcript.focused {
            self.needs_clear |= !self.split_active();
            self.transcript.focused = false;
            self.full = false;
        }
    }

    fn transcript_tick(&mut self) {
        let target = if self.transcript_shown() {
            self.transcript_target()
        } else {
            None
        };
        if self.transcript.target != target {
            self.leave_transcript();
            self.transcript.select(target);
        }
        if let Some(response) = self
            .transcript
            .reader
            .as_mut()
            .and_then(transcript::Reader::poll)
        {
            let operation = self.transcript.operation.take();
            match &response {
                Ok(response) => {
                    let applied = self.transcript.target.as_ref() == Some(&response.target)
                        && self.transcript.requested
                        && self.transcript.request_cursor == response.cursor;
                    self.event(if response.result.is_err() { "error" } else { "debug" }, "transcript.completed", || json!({
                        "operation_id": operation.as_ref().map(|(o, _)| &o.id),
                        "row_id": response.target.key, "harness": response.target.harness,
                        "outcome": if !applied { "discarded" } else if response.result.is_ok() { "loaded" } else { "failed" },
                        "worker_ms": response.elapsed_ms,
                        "duration_ms": operation.as_ref().map(|(o, _)| o.elapsed_ms()),
                        "cache_hit": response.cache_hit, "bytes_read": response.bytes_read,
                        "messages": response.result.as_ref().ok().map(|d| d.messages.len()),
                        "error": response.result.as_ref().err().map(|e| format!("{e:#}")),
                    }));
                    self.measured("transcript.read", response.elapsed_ms, || {
                        json!({
                            "operation_id": operation.as_ref().map(|(o, _)| &o.id),
                            "row_id": response.target.key, "harness": response.target.harness,
                        })
                    });
                }
                Err(error) => self.event("error", "transcript.failed", || {
                    json!({
                        "operation_id": operation.as_ref().map(|(o, _)| &o.id),
                        "row_id": operation.as_ref().map(|(_, target)| &target.key),
                        "error": format!("{error:#}"), "phase": "worker",
                    })
                }),
            }
            match response {
                Ok(response)
                    if self.transcript.target.as_ref() == Some(&response.target)
                        && self.transcript.requested
                        && self.transcript.request_cursor == response.cursor =>
                {
                    self.transcript.loaded_at = Some(Instant::now());
                    match response.result {
                        Ok(document) => {
                            if response.cursor.is_some()
                                && let Some(current) = &mut self.transcript.document
                            {
                                if response.cursor.as_ref().is_some_and(|c| c.forward()) {
                                    self.transcript.bottom = false;
                                    Arc::make_mut(current).append((*document).clone());
                                } else {
                                    self.transcript.prepend_lines =
                                        Some(self.transcript.lines.len());
                                    Arc::make_mut(current).prepend((*document).clone());
                                }
                            } else {
                                self.transcript.document = Some(document);
                            }
                            self.transcript.error = None;
                        }
                        Err(error) => {
                            if response.cursor.is_some() {
                                self.status = format!("Preview: {error:#}");
                            }
                            self.transcript.error = Some(format!("{error:#}"));
                        }
                    }
                    self.transcript.width = 0;
                    self.feedback = Some(("transcript_to_draw", Instant::now()));
                }
                Err(error) => {
                    self.transcript.loaded_at = Some(Instant::now());
                    self.transcript.error = Some(format!("{error:#}"));
                    self.transcript.requested = true;
                    self.transcript.reader = None;
                    self.transcript.width = 0;
                    self.feedback = Some(("transcript_to_draw", Instant::now()));
                }
                _ => {}
            }
        }
        let Some(target) = self.transcript.target.clone() else {
            return;
        };
        if self.transcript.load_older && self.transcript.loaded_at.is_some() {
            self.transcript.load_older = false;
            if let Some(cursor) = self
                .transcript
                .document
                .as_ref()
                .and_then(|doc| doc.older.clone())
            {
                self.transcript.request_cursor = Some(cursor);
                self.transcript.requested = false;
            }
        }
        if self.transcript.load_newer && self.transcript.loaded_at.is_some() {
            self.transcript.load_newer = false;
            if let Some(cursor) = self
                .transcript
                .document
                .as_ref()
                .and_then(|d| d.newer.clone())
            {
                self.transcript.request_cursor = Some(cursor);
                self.transcript.requested = false;
            }
        }
        // Runs can finish or write stderr after the last live snapshot. Poll their file
        // stamps while visible; unchanged files reuse the reader's cached document.
        if matches!(self.selected().map(|r| &r.kind), Some(Kind::Run(..)))
            && self
                .transcript
                .loaded_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(1))
        {
            self.transcript.requested = false;
        }
        if self.transcript.requested
            || (!self.transcript.focused
                && self
                    .transcript
                    .since
                    .is_some_and(|at| at.elapsed() < REST_SPLIT))
        {
            return;
        }
        if self.transcript.reader.is_none() {
            match transcript::Reader::new() {
                Ok(reader) => self.transcript.reader = Some(reader),
                Err(error) => {
                    self.event("error", "transcript.failed", || {
                        json!({
                            "row_id": target.key, "harness": target.harness,
                            "phase": "reader_start", "error": error.to_string(),
                        })
                    });
                    self.transcript.error = Some(error.to_string());
                    self.transcript.requested = true;
                    self.transcript.width = 0;
                    return;
                }
            }
        }
        let operation = self.log.as_ref().map(|_| DiagnosticOperation::new());
        match self
            .transcript
            .reader
            .as_mut()
            .unwrap()
            .request_page(target.clone(), self.transcript.request_cursor.clone())
        {
            Ok(true) => {
                self.transcript.loaded_at = None;
                self.event("debug", "transcript.requested", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "row_id": target.key, "harness": target.harness,
                    })
                });
                self.transcript.operation = operation.map(|o| (o, target));
                self.transcript.requested = true;
            }
            Ok(false) => {}
            Err(error) => {
                self.event("error", "transcript.failed", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "row_id": target.key, "phase": "request", "error": format!("{error:#}"),
                    })
                });
                self.transcript.error = Some(format!("{error:#}"));
                self.transcript.requested = true;
                self.transcript.width = 0;
            }
        }
    }

    /// Poll and queue the separate reader. No discovery or transcript IO runs here.
    fn history_tick(&mut self) {
        self.history.opened.retain(|key, _| {
            self.viewers.iter().any(|o| &o.key == key)
                || self.opening.as_ref().is_some_and(|o| &o.key == key)
        });
        if let Some(result) = self.history.reader.as_mut().and_then(history::Reader::poll) {
            let fetch = self.history.fetch.take();
            let operation = fetch.as_ref().and_then(|f| f.operation.as_ref());
            let current = fetch
                .as_ref()
                .is_some_and(|f| f.revision == self.history.revision)
                && self.history.visible;
            self.event(if result.is_err() { "error" } else { "debug" }, "history.completed", || json!({
                "operation_id": operation.map(|o| &o.id),
                "phase": if fetch.as_ref().is_some_and(|f| f.hydrate) { "hydrate" } else { "index_page" },
                "outcome": if !current { "discarded" } else if result.is_ok() { "loaded" } else { "failed" },
                "duration_ms": operation.map(DiagnosticOperation::elapsed_ms),
                "entries": result.as_ref().ok().map(|p| p.entries.len()),
                "total": result.as_ref().ok().map(|p| p.total),
                "generation": result.as_ref().ok().map(|p| p.generation),
                "stats": result.as_ref().ok().map(|p| &p.stats),
                "search_status": result.as_ref().ok().and_then(|p| p.search_status.as_ref()),
                "search_error": result.as_ref().ok().and_then(|p| p.search_error.as_ref()),
                "error": result.as_ref().err().map(|e| format!("{e:#}")),
            }));
            if let Ok(page) = &result {
                for (phase, ms) in [
                    ("history.index", page.stats.index_ms),
                    ("history.hydrate", page.stats.hydrate_ms),
                    ("history.worker", page.stats.worker_ms),
                ] {
                    if ms > 0.0 {
                        self.measured(
                            phase,
                            ms,
                            || json!({"operation_id": operation.map(|o| &o.id)}),
                        );
                    }
                }
            }
            if let Some(fetch) = fetch
                && fetch.revision == self.history.revision
                && self.history.visible
            {
                match result {
                    Ok(page) => {
                        self.history.homes = page.homes;
                        self.history.search_pending = page.search_pending;
                        self.history.search_status = page.search_status;
                        self.history.updated = Some(Instant::now());
                        if fetch.hydrate {
                            for entry in page.entries.into_iter().filter(|e| e.columns.is_some()) {
                                if let Some(row) = self
                                    .history
                                    .rows
                                    .iter_mut()
                                    .find(|r| r.entry.key == entry.key)
                                {
                                    row.entry = entry;
                                }
                            }
                            // Keep columns near the viewport; metadata remains for scrolling back.
                            let visible = self.history_viewport();
                            let mut count = self
                                .history
                                .rows
                                .iter()
                                .filter(|r| r.entry.columns.is_some())
                                .count();
                            for row in &mut self.history.rows {
                                if count > 128
                                    && !visible.contains(&row.entry.key)
                                    && row.entry.columns.take().is_some()
                                {
                                    count -= 1;
                                }
                            }
                        } else {
                            if fetch.after.is_none() {
                                self.history.rows.clear();
                                self.history.batches.clear();
                            }
                            self.history.batches.push(HistoryBatch {
                                after: fetch.after,
                                keys: page.entries.iter().map(|e| e.key.clone()).collect(),
                            });
                            for entry in page.entries {
                                if !self.history.rows.iter().any(|r| r.entry.key == entry.key) {
                                    self.history.rows.push(HistoryRow {
                                        key: history_key(&entry.key),
                                        entry,
                                    });
                                }
                            }
                            self.history.next = page.next;
                            self.history.ready = true;
                        }
                        self.history.error = None;
                    }
                    Err(error) => self.history.error = Some(format!("{error:#}")),
                }
                self.rebuild_with_reason("history");
                if self.history.select_first
                    && self.focus.is_none()
                    && let Some(i) = self
                        .visible
                        .iter()
                        .position(|&i| matches!(self.rows[i].kind, Kind::History(_)))
                {
                    self.cursor = i;
                    self.settle();
                    self.history.select_first = false;
                }
                self.feedback = Some(("history_to_draw", Instant::now()));
            }
        }
        if !self.history.visible
            || self.jobs_view
            || (self.pane_focused() && !self.split_active())
            || self.history.fetch.is_some()
            || self.history.error.is_some()
        {
            return;
        }
        if (matches!(self.mode, Mode::Filter) || (self.history_selected() && !self.history.refresh))
            && self
                .history
                .filter_rest
                .is_some_and(|at| at.elapsed() < Duration::from_millis(150))
        {
            return;
        }
        if self.history.reader.is_none() {
            match history::Reader::discover(self.claude.clone(), self.state.clone()) {
                Ok(reader) => self.history.reader = Some(reader),
                Err(error) => {
                    self.event("error", "history.failed", || {
                        json!({
                            "phase": "reader_start", "error": error.to_string(),
                        })
                    });
                    self.history.error = Some(error.to_string());
                    self.rebuild_with_reason("history");
                    return;
                }
            }
        }
        let visible = self.history_viewport();
        let last_shown = self
            .visible
            .iter()
            .rev()
            .find_map(|&i| match &self.rows[i].kind {
                Kind::History(key) => self.history.row(key).map(|e| &e.key),
                _ => None,
            });
        let near_end = (!self.history.rows.is_empty() && last_shown.is_none())
            || last_shown.is_some_and(|key| visible.contains(key))
            || self.history.rows.iter().enumerate().any(|(i, r)| {
                visible.contains(&r.entry.key) && i + HISTORY_PREFETCH >= self.history.rows.len()
            });
        let hydrate_keys: HashSet<_> = self
            .history
            .rows
            .iter()
            .filter(|r| r.entry.columns.is_none() && visible.contains(&r.entry.key))
            .map(|r| r.entry.key.clone())
            .collect();
        let update_search = self.history.search_pending
            && self
                .history
                .updated
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(500));
        let (after, hydrate) = if self.history.first || update_search {
            (None, false)
        } else if near_end && self.history.next.is_some() {
            (self.history.next.clone(), false)
        } else if let Some(batch) = self
            .history
            .batches
            .iter()
            .find(|b| !b.keys.is_disjoint(&hydrate_keys))
        {
            (batch.after.clone(), true)
        } else {
            return;
        };
        let query = history::Query {
            after: after.clone(),
            limit: HISTORY_PAGE,
            filter: self.history.filter.clone(),
            excluded: self.history_excluded(),
            refresh: self.history.refresh,
            hydrate,
            hydrate_keys: hydrate.then_some(hydrate_keys),
            include_archived: true,
        };
        let operation = self.log.as_ref().map(|_| DiagnosticOperation::new());
        match self.history.reader.as_mut().unwrap().request(query) {
            Ok(true) => {
                self.event("debug", "history.requested", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "phase": if hydrate { "hydrate" } else { "index_page" },
                        "has_cursor": after.is_some(), "filter_bytes": self.history.filter.len(),
                        "limit": HISTORY_PAGE, "refresh": self.history.refresh,
                    })
                });
                self.history.fetch = Some(HistoryFetch {
                    revision: self.history.revision,
                    after,
                    hydrate,
                    operation,
                });
                self.history.first = false;
                self.history.refresh = false;
                if !hydrate {
                    self.rebuild_with_reason("history");
                    self.feedback = Some(("history_request_to_draw", Instant::now()));
                }
            }
            Ok(false) => {}
            Err(error) => {
                self.event("error", "history.failed", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "phase": "request", "error": format!("{error:#}"),
                    })
                });
                self.history.error = Some(error.to_string());
                self.rebuild_with_reason("history");
            }
        }
    }

    fn poll(&mut self) {
        self.poll_stops();
        let mut launched = false;
        for (id, rx) in std::mem::take(&mut self.started) {
            match rx.try_recv() {
                Ok((message, retry)) => {
                    self.event(if retry.is_some() { "error" } else { "debug" }, "launch.applied", || json!({
                        "operation_id": id,
                        "outcome": if retry.is_some() { "failed" } else { "awaiting_discovery" },
                        "error": retry.as_ref().map(|_| message.as_str()),
                        "duration_ms": self.pending.iter().find(|p| p.session.session_id == id)
                            .map(|p| p.at.elapsed().as_secs_f64() * 1000.0),
                    }));
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
                    self.event("error", "launch.failed", || {
                        json!({
                            "operation_id": id, "error": "worker disconnected",
                        })
                    });
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
            self.event("debug", "refresh.discarded", || json!({
                "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                "reason": "invalidated_during_read",
                "duration_ms": self.loading_operation.as_ref().map(DiagnosticOperation::elapsed_ms),
            }));
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
                self.event("error", "refresh.failed", || {
                    json!({
                        "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                        "error": format!("{e:#}"), "retained_previous_rows": true,
                    })
                });
                self.status = format!("reload failed: {e:#}");
                self.stale = true;
                self.refreshed = Instant::now();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.event("error", "refresh.failed", || {
                    json!({
                        "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                        "error": "worker disconnected", "retained_previous_rows": true,
                    })
                });
                self.status = "reload failed: worker disconnected".into();
                self.stale = true;
                self.refreshed = Instant::now();
            }
        }
        self.loading_operation = None;
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
        if self.stale {
            self.event("debug", "refresh.recovered", || {
                json!({
                    "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                })
            });
        }
        let history_browsing = self.history_selected();
        if self.status.starts_with("reload failed:") {
            self.status.clear();
        }
        self.stale = false;
        self.removed_sessions
            .retain(|id| data.sessions.iter().any(|s| &s.session_id == id));
        data.sessions
            .retain(|s| !self.removed_sessions.contains(&s.session_id));
        data.sessions.retain(|s| s.harness != "terminal");
        data.sessions.extend(self.terminals.iter().cloned());
        let on = self
            .selected()
            .and_then(|r| r.kind.key().map(str::to_owned));
        for open in &self.viewers {
            if open.harness == Some(HarnessKind::Opencode)
                && let Some(report) = open.viewer.opencode_report()
                && let Some(row) = data
                    .sessions
                    .iter_mut()
                    .find(|row| row.harness == "opencode" && row.pid == Some(open.viewer.pid()))
            {
                report.apply(row);
            }
        }
        let mut replaced = self.reconcile_launches(&mut data);
        for key in self.history.opened.keys() {
            if let Some(session) = data
                .sessions
                .iter_mut()
                .find(|s| self.history_matches(key, s))
            {
                if session.title.is_none() {
                    session.title = self.history.opened.get(key).and_then(|e| e.title.clone());
                }
                replaced.insert(key.clone(), session.session_id.clone());
                for previous in self
                    .data
                    .sessions
                    .iter()
                    .filter(|s| self.history_matches(key, s))
                {
                    replaced.insert(previous.session_id.clone(), session.session_id.clone());
                }
            }
        }
        self.pending.retain(|p| {
            !replaced.contains_key(&p.session.session_id)
                && (p.at.elapsed() < PENDING_TTL
                    || self.viewers.iter().any(|o| o.key == p.session.session_id)
                    || self
                        .opening
                        .as_ref()
                        .is_some_and(|o| o.key == p.session.session_id))
        });
        data.sessions
            .extend(self.pending.iter().map(|p| p.session.clone()));
        if let Some(d) = &mut data.diagnostics {
            for p in &self.pending {
                d.sources.insert(
                    format!("{}:{}", p.session.harness, p.session.session_id),
                    json!({
                        "reader": "pending_launch", "operation_id": p.session.session_id,
                    }),
                );
            }
        }
        let arrived = data
            .sessions
            .iter()
            .filter(|s| {
                !replaced.values().any(|id| id == &s.session_id)
                    && !self
                        .data
                        .sessions
                        .iter()
                        .any(|o| o.session_id == s.session_id)
            })
            .max_by_key(|s| s.started)
            .map(|s| s.session_id.clone());
        for (old, new) in &replaced {
            if old != new {
                self.event("debug", "row.reidentified", || {
                    json!({
                        "operation_id": old,
                        "before_id": old,
                        "session_id": new,
                        "reason": "native_identity_reported",
                    })
                });
            }
        }
        self.data = data;
        self.report_load();
        if let Some(log) = &self.log {
            log.trace("refresh.completed", || json!({
                "operation_id": self.loading_operation.as_ref().map(|o| &o.id),
                "duration_ms": self.loading_operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                "sessions": self.data.sessions.len(), "runs": self.data.runs.len(), "jobs": self.data.jobs.len(),
            }));
        }
        self.rebuild_with_reason("refresh");
        if let Some(id) = on
            .as_ref()
            .and_then(|id| replaced.get(id).filter(|next| *next != id))
        {
            // This is the same selected session, even while its viewer has focus.
            if let Some(i) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].kind.key() == Some(id.as_str()))
            {
                self.cursor = i;
                self.settle();
            }
        } else if !history_browsing && let Some(id) = arrived {
            self.select_new(&id);
        }
        self.refreshed = Instant::now();
        self.report_view("refresh");
    }

    /// Preserve row and viewer identity as process rows acquire native session ids.
    fn reconcile_launches(&mut self, data: &mut Data) -> HashMap<String, String> {
        let mut replaced = HashMap::new();
        for p in &self.pending {
            if let Some(s) = data.sessions.iter().find(|s| p.matches(s)) {
                replaced.insert(p.session.session_id.clone(), s.session_id.clone());
            }
        }
        for old in &self.data.sessions {
            if let Some(s) = data
                .sessions
                .iter()
                .find(|s| s.session_id == old.session_id)
                .or_else(|| {
                    data.sessions.iter().find(|s| {
                        harness::by_name(&old.harness).is_some_and(|spec| {
                            spec.launch
                                .as_ref()
                                .is_some_and(|launch| launch.identity.owns_client_pid())
                        }) && s.harness == old.harness
                            && s.cwd == old.cwd
                            && old.pid.is_some_and(|pid| s.pid == Some(pid))
                    })
                })
            {
                replaced.insert(old.session_id.clone(), s.session_id.clone());
            }
        }
        // A new remote Codex client does not report its thread id. Discovery replaces its
        // process row with a daemon row. Pair only a unique new thread and unique launch;
        // simultaneous launches in one folder must not steal each other's viewers.
        let candidates = |open: &Open| -> Vec<&Session> {
            let Some(spec) = open.harness.map(harness::spec).filter(|spec| {
                spec.launch.as_ref().is_some_and(|launch| {
                    launch.identity == harness::spec::LaunchIdentity::ReportedThread
                })
            }) else {
                return vec![];
            };
            let Some((dir, since)) = &open.record else {
                return vec![];
            };
            let Some(prompt) = self
                .data
                .sessions
                .iter()
                .find(|s| s.session_id == open.key)
                .and_then(|s| s.title.as_deref())
            else {
                return vec![];
            };
            let launch = spec.launch.as_ref().expect("filtered launch identity");
            if !launch
                .identity
                .unresolved_key(&spec.name, &open.key, open.viewer.pid())
            {
                return vec![];
            }
            data.sessions
                .iter()
                .filter(|s| {
                    s.harness == spec.name
                        && s.kind == launch.session_kind
                        && s.cwd == *dir
                        && s.started.is_some_and(|at| at >= *since)
                        && !self.viewers.iter().any(|o| o.key == s.session_id)
                        && s.transcript_path
                            .as_deref()
                            .and_then(codex::prompt_of)
                            .as_deref()
                            == Some(prompt)
                })
                .collect()
        };
        for open in &self.viewers {
            let possible = candidates(open);
            if let [s] = possible.as_slice()
                && self
                    .viewers
                    .iter()
                    .filter(|o| candidates(o).iter().any(|p| p.session_id == s.session_id))
                    .count()
                    == 1
            {
                replaced.insert(open.key.clone(), s.session_id.clone());
            }
        }
        for open in &mut self.viewers {
            if let Some(id) = replaced.get(&open.key) {
                let old = self.data.sessions.iter().find(|s| s.session_id == open.key);
                if let Some(s) = data.sessions.iter_mut().find(|s| &s.session_id == id) {
                    let original = old.and_then(|s| s.title.clone());
                    s.title = if s.session_id == format!("{}-{}", s.harness, open.viewer.pid()) {
                        // The original prompt is more precise than ps's flattened argv.
                        original.or(s.title.clone())
                    } else {
                        s.title.clone().or(original)
                    };
                    if harness::by_name(&s.harness).is_some_and(|spec| {
                        spec.launch
                            .as_ref()
                            .is_some_and(|launch| launch.identity.owns_client_pid())
                    }) {
                        s.pid = Some(open.viewer.pid());
                    }
                    if open.record.is_some()
                        && !open.recorded
                        && s.harness == "codex"
                        && s.kind.as_deref() == Some("daemon")
                        && s.state != "-"
                        && let (Some(started), Some(rollout)) = (s.started, &s.transcript_path)
                    {
                        open.recorded = codex::remember(
                            &self.state,
                            codex::Thread {
                                id: s.session_id.clone(),
                                cwd: s.cwd.clone(),
                                started,
                                rollout: rollout.clone(),
                            },
                        )
                        .is_ok();
                    }
                }
                open.key = id.clone();
            }
        }
        // The live client is still our row while discovery has no certain native identity.
        for open in &self.viewers {
            if open.harness.is_some_and(|kind| {
                let spec = harness::spec(kind);
                spec.launch.as_ref().is_some_and(|launch| {
                    launch
                        .identity
                        .unresolved_key(&spec.name, &open.key, open.viewer.pid())
                })
            }) && !self
                .pending
                .iter()
                .any(|p| p.session.session_id == open.key)
                && !data.sessions.iter().any(|s| s.session_id == open.key)
                && let Some(old) = self.data.sessions.iter().find(|s| s.session_id == open.key)
            {
                data.sessions.push(old.clone());
            }
        }
        replaced
    }

    /// Do not change the launch target while typing or move selection away from a focused viewer.
    fn select_new(&mut self, id: &str) {
        if self.focus.is_some() || !self.composer_text().is_empty() {
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
        let reason = if matches!(self.mode, Mode::Filter) {
            "filter"
        } else {
            "view_rebuild"
        };
        self.rebuild_with_reason(reason);
    }

    fn rebuild_with_reason(&mut self, reason: &str) {
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
            self.split_active() && self.data.columns_default,
            &deleting,
            &mut self.widths,
        ));
        self.other = if self.jobs_view { menu_rows() } else { vec![] };
        self.other.extend(self.data.rows_excluding(
            self.by_state,
            !self.jobs_view,
            self.split_active() && self.data.columns_default,
            &deleting,
            &mut self.widths,
        ));
        let excluded = self.history_excluded();
        let history = self.history.table(&self.data, &excluded);
        if self.jobs_view {
            self.other.extend(history);
        } else {
            self.rows.extend(history);
        }
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
        self.report_rows(reason);
    }

    fn filter_changed(&mut self) {
        if self.history.visible && self.history.filter != self.filter.text {
            // Retain history focus while results load or the query has no matches.
            self.history.select_first = self.history_selected();
            self.history.reset(&self.filter.text, false);
            self.rebuild_with_reason("filter");
        }
        self.apply_filter();
        self.settle();
    }

    /// Keep matching rows and their group headers. Exclude other unselectable kinds
    /// when filtering, or they can hide the header above them.
    fn apply_filter(&mut self) {
        let needle = self.filter.text.to_lowercase();
        let rows = &self.rows;
        let matched: Vec<usize> = (0..rows.len())
            .filter(|&i| {
                needle.is_empty()
                    || matches!(rows[i].kind, Kind::History(_) | Kind::HistoryStatus)
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
                    || rows[i].kind == Kind::HistoryStatus
                    || (rows[i].kind == Kind::Header
                        && matched.get(n + 1).is_some_and(|&j| {
                            rows[j].kind.selectable() || rows[j].kind == Kind::HistoryStatus
                        }))
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
        self.column_context = self.columns_tab();
        self.history.select_first = false;
        let n = self.visible.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.cursor as isize;
        for _ in 0..n {
            if self.history.visible && (i + delta >= n || i + delta < 0) {
                // Stay at the end while the next page arrives; history never wraps to the menu.
                break;
            }
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
            Kind::History(key) => self.history.row(key).map(|e| e.cwd.clone()),
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
            Kind::History(key) => Some(key.clone()),
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
        if let Some(i) = self
            .viewers
            .iter()
            .position(|o| self.history_matches(&o.key, s))
        {
            return Some(i);
        }
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

    /// Sessions, runs and history show only their own viewer or preview.
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
        if own.is_some()
            || self.history_selected()
            || matches!(
                self.selected().map(|r| &r.kind),
                Some(Kind::Session(..) | Kind::Run(..) | Kind::History(_) | Kind::HistoryStatus)
            )
        {
            return own;
        }
        self.most_recently_focused()
    }

    /// The selected row's viewer can take focus even when the split pane is hidden.
    fn focusable_viewer(&self) -> Option<usize> {
        self.selected()
            .and_then(|r| self.viewer_of(&r.kind))
            .or_else(|| self.shown())
    }

    fn selected_session(&self) -> Option<&fleet::Session> {
        let Some(Kind::Session(id, _)) = self.selected().map(|r| &r.kind) else {
            return None;
        };
        self.data.sessions.iter().find(|s| &s.session_id == id)
    }

    fn rename_selected(&mut self) {
        match self.selected_session() {
            Some(s)
                if harness::by_name(&s.harness).is_some_and(|spec| spec.operations.rename)
                    && s.transcript_path.is_some() =>
            {
                self.mode = Mode::Rename(Input::new(s.title.clone().unwrap_or_default()));
            }
            Some(s) if harness::by_name(&s.harness).is_some_and(|spec| spec.operations.rename) => {
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
            Mode::Guide(..) => Some("help"),
            Mode::Config(_) => Some("config"),
            Mode::Columns(_) => Some("columns"),
            Mode::Job(_) => Some("jobs"),
            Mode::Folder(_) => Some("folder"),
            _ => self.jobs_view.then_some("jobs"),
        };
        open.or_else(|| {
            (!self.history_selected()
                && matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu)))
            .then(|| MENU[self.menu].0)
        })
    }

    fn panel_shown(&self) -> bool {
        self.split_active() && !self.panel_focused() && self.panel().is_some()
    }

    fn panel_focused(&self) -> bool {
        self.jobs_view
            || matches!(
                self.mode,
                Mode::Guide(..)
                    | Mode::Config(_)
                    | Mode::Columns(_)
                    | Mode::Job(_)
                    | Mode::Folder(_)
            )
    }

    fn pane_focused(&self) -> bool {
        self.focus.is_some() || self.transcript.focused || self.panel_focused()
    }

    fn columns_tab(&self) -> usize {
        if self.jobs_view {
            return 2;
        }
        match self.selected().map(|r| &r.kind) {
            Some(Kind::Session(..)) => 0,
            Some(Kind::Run(..)) => 1,
            Some(Kind::Job(_) | Kind::NewJob) => 2,
            Some(Kind::History(_) | Kind::HistoryStatus) => 3,
            _ => self.column_context,
        }
    }

    fn open_columns(&mut self, return_config: Option<Box<ConfigForm>>) {
        let mut form = ColumnsPicker::new(&self.jobs_path, self.columns_tab());
        form.return_config = return_config;
        self.mode = Mode::Columns(Box::new(form));
        self.needs_clear = true;
    }

    fn column_action(&mut self, action: ColumnAction) {
        match action {
            ColumnAction::Stay => {}
            ColumnAction::Close => {
                let Mode::Columns(mut picker) = std::mem::replace(&mut self.mode, Mode::Normal)
                else {
                    return;
                };
                if let Some(mut form) = picker.return_config.take() {
                    let fresh = self.config_form();
                    for (key, _) in COLUMN_SETS {
                        let i = field_at(key);
                        form.values[i] = fresh.values[i].clone();
                    }
                    self.mode = Mode::Config(form);
                } else if !self.jobs_view {
                    self.select_first_session();
                }
                self.needs_clear = true;
            }
            ColumnAction::Save(before) => {
                let Mode::Columns(picker) = &mut self.mode else {
                    return;
                };
                let tab = picker.tab;
                let form = picker.current();
                let key = COLUMN_SETS[tab].0;
                let chosen = form.chosen();
                let default = form.default;
                if let Err(e) = config::write_column_set(
                    &self.jobs_path,
                    key,
                    (!default).then_some(chosen.as_slice()),
                ) {
                    picker.sets[tab] = before;
                    picker.error = Some(format!("{e:#}"));
                    return;
                }
                match tab {
                    0 => {
                        self.data.columns = chosen;
                        self.data.columns_default = default;
                    }
                    1 => self.data.run_columns = chosen,
                    2 => self.data.job_columns = chosen,
                    _ => self.data.history_columns = chosen,
                }
                self.rebuild();
                self.invalidate();
                self.status = format!("{key} saved");
            }
        }
    }

    fn config_action(&mut self, action: ConfigAction, mut before: Box<ConfigForm>) {
        match action {
            ConfigAction::Columns => {
                let Mode::Config(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    unreachable!()
                };
                self.open_columns(Some(form));
            }
            ConfigAction::Stay => {}
            ConfigAction::Cancel => {
                self.mode = Mode::Normal;
                self.select_first_session();
            }
            ConfigAction::Save(
                policy,
                columns,
                spark,
                pane,
                start,
                mark,
                whole,
                run_columns,
                job_columns,
                history_columns,
            ) => {
                match config::write_config(
                    &self.jobs_path,
                    &policy,
                    columns.as_deref(),
                    spark.as_deref(),
                    pane.as_ref(),
                    start.as_ref(),
                    mark,
                    whole,
                    run_columns.as_deref(),
                    job_columns.as_deref(),
                    history_columns.as_deref(),
                ) {
                    Ok(()) => {
                        self.data.columns_default = columns.is_none();
                        self.data.columns = columns.unwrap_or_else(built_columns);
                        self.data.run_columns = run_columns.unwrap_or_else(built_run_columns);
                        self.data.job_columns =
                            job_columns.unwrap_or_else(|| built_column_set("job_columns"));
                        self.data.history_columns =
                            history_columns.unwrap_or_else(|| built_column_set("history_columns"));
                        self.data.whole_columns = whole.unwrap_or(config::WHOLE_COLUMNS);
                        self.rebuild();
                        self.status = format!("config saved to {}", fleet::tilde(&self.jobs_path));
                        self.invalidate();
                    }
                    Err(e) => {
                        before.error = Some(format!("{e:#}"));
                        self.mode = Mode::Config(before);
                    }
                }
            }
        }
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
            config::file_run_columns(&self.jobs_path).as_deref(),
            config::file_job_columns(&self.jobs_path).as_deref(),
            config::file_history_columns(&self.jobs_path).as_deref(),
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
        self.transcript.focused = false;
        if std::mem::take(&mut self.viewers[i].speculative) {
            let spawned = self.viewers[i].last_focused;
            self.event("debug", "viewer.prespawn_hit", || {
                json!({
                    "row_id": self.viewers[i].key,
                    "viewer_pid": self.viewers[i].viewer.pid(),
                    "operation_id": self.viewers[i].operation.as_ref().map(|o| &o.id),
                    "age_ms": spawned.elapsed().as_secs_f64() * 1000.0,
                })
            });
            while self.live_viewers() > MAX_FOCUSED_VIEWERS {
                let Some(oldest) = self.least_recently_focused(Some(i)) else {
                    break;
                };
                self.close_for(oldest, "focused_viewer_capacity");
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
        let data = json!({
            "row_id": open.key, "harness": open.harness,
            "viewer_pid": open.viewer.pid(),
            "operation_id": open.operation.as_ref().map(|o| &o.id),
        });
        self.event("debug", "viewer.focused", || data);
        self.report_view("viewer_focus");
    }

    fn open(
        &mut self,
        terminal_size: (u16, u16),
        c: Command,
        what: &str,
        key: String,
        record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
    ) -> bool {
        let operation = self.log.as_ref().map(|_| DiagnosticOperation::new());
        self.open_traced(terminal_size, c, what, key, record, operation)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_traced(
        &mut self,
        terminal_size: (u16, u16),
        c: Command,
        what: &str,
        key: String,
        record: Option<(PathBuf, chrono::DateTime<chrono::Utc>)>,
        operation: Option<DiagnosticOperation>,
    ) -> bool {
        self.size = terminal_size;
        self.pane = self.pane(self.frame());
        if let Some(i) = self.viewer_index(&key) {
            self.focus(i);
            return true;
        }
        let context = self.key_context(&key);
        self.event("debug", "viewer.opening", || {
            json!({
                "operation_id": operation.as_ref().map(|o| &o.id),
                "row": context, "row_id": key,
                "harness": self.viewer_harness(&key),
                "program": c.get_program().to_string_lossy(),
                "width": self.pane.width, "height": self.pane.height,
            })
        });
        if let Some(log) = &self.log {
            log.trace("viewer.command", || {
                json!({
                    "operation_id": operation.as_ref().map(|o| &o.id),
                    "command": format!("{c:?}"),
                })
            });
        }
        let spawning = Instant::now();
        let normal = SHELL_TTY.get().and_then(|t| t.as_ref());
        let spawn = if key.starts_with("terminal:") {
            Viewer::spawn_terminal
        } else {
            Viewer::spawn
        };
        match spawn(
            c,
            self.pane.height,
            self.pane.width,
            normal,
            self.colors.clone(),
        ) {
            Ok(viewer) => {
                if let Some(p) = self
                    .pending
                    .iter_mut()
                    .find(|p| p.session.session_id == key)
                {
                    p.session.pid = Some(viewer.pid());
                }
                if let Some(s) = self.data.sessions.iter_mut().find(|s| s.session_id == key)
                    && harness::by_name(&s.harness).is_some_and(|spec| {
                        spec.launch
                            .as_ref()
                            .is_some_and(|launch| launch.identity.owns_client_pid())
                    })
                {
                    s.pid = Some(viewer.pid());
                }
                // Evict only after the new viewer starts successfully.
                while self.live_viewers() >= MAX_FOCUSED_VIEWERS {
                    let Some(oldest) = self.least_recently_focused(None) else {
                        break;
                    };
                    self.close_for(oldest, "focused_viewer_capacity");
                }
                let harness = self.viewer_harness(&key);
                self.viewers.push(Open {
                    key,
                    what: what.to_owned(),
                    harness,
                    viewer,
                    record,
                    recorded: false,
                    first_paint_logged: false,
                    last_focused: Instant::now(),
                    speculative: false,
                    operation,
                });
                let open = self.viewers.last().unwrap();
                self.event("debug", "viewer.opened", || {
                    json!({
                        "operation_id": open.operation.as_ref().map(|o| &o.id),
                        "row": context, "row_id": open.key, "harness": open.harness,
                        "viewer_pid": open.viewer.pid(),
                        "spawn_ms": spawning.elapsed().as_secs_f64() * 1000.0,
                    })
                });
                self.focus(self.viewers.len() - 1);
                true
            }
            Err(e) => {
                self.event("error", "viewer.failed", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "row": context, "row_id": key, "phase": "spawn",
                        "error": e.to_string(),
                        "duration_ms": spawning.elapsed().as_secs_f64() * 1000.0,
                    })
                });
                self.status = format!("{what} failed: {e}");
                false
            }
        }
    }

    fn live_viewers(&self) -> usize {
        self.viewers.iter().filter(|o| !o.speculative).count()
    }

    fn viewer_harness(&self, key: &str) -> Option<HarnessKind> {
        if let Some(session) = self.data.sessions.iter().find(|s| s.session_id == key) {
            return harness::by_name(&session.harness).map(|s| s.kind);
        }
        if let Some(entry) = self.history.opened.get(key) {
            return harness::by_name(&entry.key.harness).map(|s| s.kind);
        }
        let run = key.strip_prefix("run:")?;
        self.data
            .runs
            .iter()
            .find(|r| r.started.run_id == run && r.status() != "started")?
            .started
            .harness
    }

    /// Only a listed session's Claude attach makes room; a Codex client the user entered is theirs,
    /// unsent composer text and all, however cheaply a peek could reopen it. See
    /// `MAX_FOCUSED_VIEWERS`.
    fn least_recently_focused(&self, keep: Option<usize>) -> Option<usize> {
        self.viewers
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                !o.speculative
                    && Some(*i) != keep
                    && o.harness.is_some_and(|kind| {
                        harness::spec(kind).viewer.retention == harness::spec::Retention::EvictLive
                    })
                    && !o.key.starts_with("run:")
                    && !o.key.starts_with("history:")
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

    /// Only pre-open joins of a live session: a Claude attach or a Codex resume against the
    /// daemon that holds the thread. Resuming a finished run or starting a session changes the
    /// fleet, so those still wait for enter. Done background jobs still have a joinable worker.
    #[cfg(test)]
    fn prespawn_target(&self) -> Option<(String, PathBuf)> {
        self.prespawn_decision().ok()
    }

    fn prespawn_decision(&self) -> std::result::Result<(String, PathBuf), &'static str> {
        if !matches!(self.mode, Mode::Normal)
            || self.focus.is_some()
            || self.opening.is_some()
            || self.history.select_first
            || !self.composer_text().trim().is_empty()
        {
            return Err("not_browsing_sessions");
        }
        let (rested, since) = self.rest.as_ref().ok_or("no_viewer_target")?;
        if since.elapsed() < self.rest_for() {
            return Err("cursor_rest");
        }
        if self.prespawned.as_deref() == Some(rested.as_str()) {
            return Err("already_attempted");
        }
        let Some(Kind::Session(id, _)) = self.selected().map(|r| &r.kind) else {
            return Err("explicit_open_required");
        };
        if id != rested {
            return Err("selection_changed");
        }
        if id.starts_with("starting:") {
            return Err("launch_pending");
        }
        if self.viewer_index(id).is_some() {
            return Err("viewer_already_open");
        }
        if self.stopping.iter().any(|a| &a.id == id) {
            return Err("action_pending");
        }
        if self.removed_sessions.contains(id) {
            return Err("hidden");
        }
        let s = self
            .data
            .sessions
            .iter()
            .find(|s| &s.session_id == id)
            .ok_or("session_not_in_snapshot")?;
        if !Self::joinable(s) {
            return Err("native_kind_cannot_peek");
        }
        let spec = harness::by_name(&s.harness).ok_or("unknown_harness")?;
        let home = spec.session_home(&self.claude, s);
        if !harness::can_peek(s, &home) {
            return Err("native_viewer_unavailable");
        }
        Ok((id.clone(), s.cwd.clone()))
    }

    /// Done Claude background jobs still have a worker; failed or stopped jobs do not. A Codex
    /// thread is joinable only behind the daemon, which `own_terminal` already decides.
    fn joinable(s: &Session) -> bool {
        harness::by_name(&s.harness).is_some_and(|spec| spec.permits_peek(s))
    }

    fn prespawn(&mut self, id: String, _cwd: PathBuf) {
        self.prespawned = Some(id.clone());
        let normal = SHELL_TTY.get().and_then(|t| t.as_ref());
        let Some(session) = self.data.sessions.iter().find(|s| s.session_id == id) else {
            return;
        };
        let Some(spec) = harness::by_name(&session.harness) else {
            return;
        };
        let home = spec.session_home(&self.claude, session);
        let what = spec.commands.viewer.clone();
        let operation = self.log.as_ref().map(|_| DiagnosticOperation::new());
        let context = self.key_context(&id);
        self.event("debug", "viewer.prespawn_started", || {
            json!({
                "operation_id": operation.as_ref().map(|o| &o.id),
                "row": context, "row_id": id, "harness": spec.name,
            })
        });
        let viewer = harness::join(session, &home, true).and_then(|c| {
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
                self.event("error", "viewer.failed", || {
                    json!({
                        "operation_id": operation.as_ref().map(|o| &o.id),
                        "row": context, "row_id": id, "phase": "prespawn",
                        "error": format!("{e:#}"),
                        "duration_ms": operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                    })
                });
                return;
            }
        };
        self.viewers.push(Open {
            key: id,
            what,
            harness: Some(spec.kind),
            viewer,
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: true,
            operation,
        });
        self.pool_speculative();
        let open = self.viewers.last().unwrap();
        self.event("debug", "viewer.prespawned", || {
            json!({
                "operation_id": open.operation.as_ref().map(|o| &o.id),
                "row": context, "row_id": open.key, "harness": open.harness,
                "viewer_pid": open.viewer.pid(),
                "duration_ms": open.operation.as_ref().map(DiagnosticOperation::elapsed_ms),
            })
        });
        if let Some(log) = &self.log {
            log.trace("viewer.command", || {
                json!({
                    "operation_id": open.operation.as_ref().map(|o| &o.id), "command": command,
                })
            });
        }
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
            self.close_for(oldest, "speculative_viewer_capacity");
        }
    }

    fn close_orphan_speculative(&mut self) {
        let gone = self.viewers.iter().position(|o| {
            o.speculative && !self.data.sessions.iter().any(|s| s.session_id == o.key)
        });
        if let Some(i) = gone {
            let key = self.viewers[i].key.clone();
            self.event(
                "debug",
                "viewer.orphaned",
                || json!({"row_id": key, "reason": "session_left_discovery"}),
            );
            self.close_for(i, "session_left_discovery");
        }
    }

    /// Once a loop turn, after the reload landed and the viewers were pumped.
    fn prespawn_tick(&mut self) {
        self.close_orphan_speculative();
        self.track_rest();
        match self.prespawn_decision() {
            Ok((id, cwd)) => {
                self.diagnostic_peek = None;
                self.prespawn(id, cwd);
            }
            Err(reason)
                if self.log.is_some()
                    && matches!(
                        reason,
                        "explicit_open_required"
                            | "native_kind_cannot_peek"
                            | "native_viewer_unavailable"
                            | "action_pending"
                            | "hidden"
                            | "unknown_harness"
                    ) =>
            {
                let data = json!({"row": self.selected().map(|r| self.row_context(&r.kind)), "reason": reason});
                if self.diagnostic_peek.as_ref() != Some(&data) {
                    self.event("debug", "viewer.peek_refused", || data.clone());
                    self.diagnostic_peek = Some(data);
                }
            }
            _ => {}
        }
    }

    /// Record new Codex threads before closing their viewers so their rows survive.
    fn close(&mut self, i: usize) {
        self.close_for(i, "requested");
    }

    fn close_for(&mut self, i: usize, reason: &str) {
        let closing = Instant::now();
        let had_frame = !self.split_active();
        let open = self.viewers.remove(i);
        self.remove_launch(&open.key);
        match self.focus {
            Some(f) if f == i => {
                self.focus = None;
                self.needs_clear = had_frame;
            }
            Some(f) if f > i => self.focus = Some(f - 1),
            _ => {}
        }
        if open.is_terminal() {
            self.terminals.retain(|s| s.session_id != open.key);
            self.data.sessions.retain(|s| s.session_id != open.key);
            self.rebuild_with_reason("terminal_closed");
        }
        let mut closed = json!({
            "operation_id": open.operation.as_ref().map(|o| &o.id),
            "row_id": open.key, "harness": open.harness,
            "viewer_pid": open.viewer.pid(), "reason": reason,
        });
        if open.record.is_some() && !open.recorded {
            self.record_codex(&open.key);
        }
        let opencode_pid = (open.harness == Some(HarnessKind::Opencode)).then(|| open.viewer.pid());
        drop(open);
        if let Some(pid) = opencode_pid {
            // This native client has ended with its viewer. Remove its reported
            // identity before history queries can exclude the saved conversation.
            self.data
                .sessions
                .retain(|s| s.harness != "opencode" || s.pid != Some(pid));
            self.rebuild_with_reason("native_client_closed");
            self.invalidate();
        }
        closed["duration_ms"] = json!(closing.elapsed().as_secs_f64() * 1000.0);
        self.event("debug", "viewer.closed", || closed);
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
        if open.record.is_some() && !open.recorded {
            let key = open.key.clone();
            self.viewers[i].recorded = self.record_codex(&key);
        }
        self.invalidate();
        let open = &self.viewers[i];
        self.event("debug", "viewer.left", || {
            json!({
                "operation_id": open.operation.as_ref().map(|o| &o.id),
                "row_id": open.key, "harness": open.harness,
                "viewer_pid": open.viewer.pid(), "title": open.viewer.title(),
            })
        });
        // Drop viewers left in Claude's agent list so the row cannot show or attach another session.
        if !self.viewers[i].is_terminal()
            && self.viewers[i].viewer.title() == Some(AGENT_VIEW_TITLE)
        {
            self.close_for(i, "viewer_showing_agent_list");
        }
        self.report_view("viewer_leave");
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
            let pumping = Instant::now();
            match open.viewer.pump() {
                Ok(changed) => dirty |= changed && on_view,
                Err(e) => failed = Some(format!("{} failed: {e}", open.what)),
            }
            if !open.first_paint_logged
                && let Some(d) = open.viewer.first_paint()
            {
                open.first_paint_logged = true;
                lines.push(json!({
                    "operation_id": open.operation.as_ref().map(|o| &o.id),
                    "row_id": open.key, "harness": open.harness,
                    "viewer_pid": open.viewer.pid(),
                    "spawn_to_first_text_ms": d.as_secs_f64() * 1000.0,
                    "operation_ms": open.operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                    "speculative": open.speculative,
                }));
            }
            let exited = open.viewer.exited();
            let speculative = open.speculative;
            let return_to_list = open.viewer.take_return_to_list() && open.is_terminal() && focused;
            let context = json!({
                "operation_id": open.operation.as_ref().map(|o| &o.id),
                "row_id": open.key, "harness": open.harness, "viewer_pid": open.viewer.pid(),
            });
            self.measured(
                "viewer_pump",
                pumping.elapsed().as_secs_f64() * 1000.0,
                || context.clone(),
            );
            for line in lines {
                self.event("debug", "viewer.first_paint", || line);
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
                self.event("debug", "viewer.exited", || {
                    json!({
                        "viewer": context, "row_id": key, "reason": why, "speculative": true,
                    })
                });
                self.close_for(i, "speculative_viewer_exit");
                continue;
            }
            if let Some(message) = failed {
                if focused {
                    self.feedback = Some(("return_to_draw", Instant::now()));
                }
                self.status = message;
                self.event("error", "viewer.failed", || {
                    json!({
                        "viewer": context, "phase": "pump", "error": self.status,
                    })
                });
                self.close_for(i, "pump_failed");
                self.invalidate();
                continue;
            }
            if return_to_list && exited.is_none() {
                self.unfocus();
                dirty = true;
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
            self.event(
                if status.success() { "debug" } else { "error" },
                "viewer.exited",
                || {
                    json!({
                        "viewer": context, "exit_code": status.code(), "status": status.to_string(),
                        "message": self.status,
                    })
                },
            );
            self.close_for(i, "native_exit");
            self.invalidate();
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
                    .find(|s| s.session_id == open.key || self.history_matches(&open.key, s))
                    .and_then(|s| s.title.clone())
            })
            .or_else(|| {
                self.history
                    .opened
                    .get(&open.key)
                    .and_then(|e| e.title.clone())
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
            .filter(|s| {
                has_id
                    && s.state == "blocked"
                    && s.session_id != open.key
                    && !self.history_matches(&open.key, s)
            })
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
                Span::styled(
                    if open.is_terminal() && open.what == "zsh" {
                        "←/tab on empty · ctrl+z back".to_owned()
                    } else {
                        format!("{} back", open.return_key())
                    },
                    dim(),
                ),
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
        self.split_active()
            || self.focus.is_some()
            || self.history.visible
            || matches!(
                self.mode,
                Mode::Columns(_) | Mode::Config(_) | Mode::Guide(_)
            )
    }

    /// VS Code sends an empty bracketed paste for clipboard images. Forward it as ctrl+v
    /// to a viewer, or read the clipboard for the composer.
    fn paste(&mut self, text: &str) {
        if self.transcript.focused {
            return;
        }
        if self.focus.is_none()
            && let Mode::Guide(guide) = &mut self.mode
        {
            let pasted = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
            let at = snap(&guide.find.text, guide.find.at);
            guide.find.text.insert_str(at, &pasted);
            guide.find.at = at + pasted.len();
            guide.top = 0;
            return;
        }
        if self.focus.is_none()
            && (matches!(self.mode, Mode::Filter)
                || (matches!(self.mode, Mode::Normal) && self.history_selected()))
        {
            if !text.is_empty() {
                let pasted = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
                let at = snap(&self.filter.text, self.filter.at);
                self.filter.text.insert_str(at, &pasted);
                self.filter.at = at + pasted.len();
                self.filter_changed();
            }
            return;
        }
        if text.is_empty() {
            if let Some(open) = self.focused() {
                open.viewer.write(b"\x16");
            } else if matches!(self.mode, Mode::Normal) && !self.terminal_selected() {
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
            let pasted = text.replace("\r\n", "\n").replace('\r', "\n");
            let (text, caret) = self.composer_input_mut();
            let at = snap(text, *caret);
            text.insert_str(at, &pasted);
            *caret = at + pasted.len();
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
        if let Mode::Guide(guide) = &mut self.mode
            && guide.area.contains((ev.column, ev.row).into())
        {
            if let Some(code) = match ev.kind {
                MouseEventKind::ScrollUp => Some(KeyCode::Up),
                MouseEventKind::ScrollDown => Some(KeyCode::Down),
                _ => None,
            } {
                for _ in 0..WHEEL_LINES {
                    guide.key(code, KeyModifiers::NONE);
                }
            }
            return;
        }
        if let Mode::Config(form) = &mut self.mode
            && (form.area.left()..form.area.right()).contains(&ev.column)
            && (form.area.top()..form.area.bottom()).contains(&ev.row)
        {
            let before = form.clone();
            let action = form.mouse(ev);
            self.config_action(action, before);
            return;
        }
        if let Mode::Columns(form) = &mut self.mode
            && (form.area.left()..form.area.right()).contains(&ev.column)
            && (form.area.top()..form.area.bottom()).contains(&ev.row)
        {
            let action = form.mouse(ev);
            self.column_action(action);
            return;
        }
        let list = self.list_area;
        if self.history.visible
            && !self.jobs_view
            && matches!(self.mode, Mode::Normal | Mode::Filter)
            && (self.split_active() || !self.pane_focused())
            && (list.left()..list.right()).contains(&ev.column)
            && (list.top()..list.bottom()).contains(&ev.row)
        {
            if matches!(
                ev.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                if self.focus.is_some() {
                    self.unfocus();
                }
                self.leave_transcript();
                let delta = if ev.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
                for _ in 0..WHEEL_LINES {
                    self.step(delta);
                }
                return;
            }
            if ev.kind == MouseEventKind::Down(MouseButton::Left) {
                self.click(ev);
                return;
            }
        }
        if self.transcript_shown()
            && (self.pane.left()..self.pane.right()).contains(&ev.column)
            && (self.pane.top()..self.pane.bottom()).contains(&ev.row)
        {
            match ev.kind {
                MouseEventKind::Down(MouseButton::Left) => self.focus_transcript(),
                MouseEventKind::ScrollUp => self.transcript.scroll(-(WHEEL_LINES as isize)),
                MouseEventKind::ScrollDown => self.transcript.scroll(WHEEL_LINES as isize),
                _ => {}
            }
            return;
        }
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
        self.column_context = self.columns_tab();
        if ev.kind != MouseEventKind::Down(MouseButton::Left) {
            return true;
        }
        self.history.select_first = false;
        let p = self.pane;
        let on_pane = (self.split_active() || self.pane_focused())
            && (p.left()..p.right()).contains(&ev.column)
            && (p.top()..p.bottom()).contains(&ev.row);
        if on_pane {
            if self.transcript_target().is_some() {
                self.focus_transcript();
                return false;
            }
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
        self.leave_transcript();
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
        let operation = self.log.as_ref().map(|_| DiagnosticOperation {
            id: if self.pending.iter().any(|p| p.session.session_id == key) {
                key.clone()
            } else {
                uuid::Uuid::new_v4().to_string()
            },
            started: Instant::now(),
        });
        let context = self.key_context(&key);
        self.event("debug", "viewer.preparing", || {
            json!({
                "operation_id": operation.as_ref().map(|o| &o.id),
                "parent_operation_id": self.input_operation.as_ref().map(|(o, _)| &o.id),
                "row": context, "row_id": key, "viewer": what,
            })
        });
        self.status = format!("opening {what} · esc cancels");
        let log = self.log.clone();
        let pending_operation = operation.clone();
        let pending_key = key.clone();
        std::thread::spawn(move || {
            let result = prepare();
            if let Some(log) = log {
                log.event(if result.is_err() { "error" } else { "debug" }, "viewer.prepared", json!({
                    "operation_id": pending_operation.as_ref().map(|o| &o.id),
                    "row": context, "row_id": pending_key,
                    "duration_ms": pending_operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                    "outcome": if result.is_ok() { "ready" } else { "failed" },
                    "error": result.as_ref().err().map(|e| format!("{e:#}")),
                }));
            }
            let _ = tx.send(result);
        });
        self.opening = Some(Opening {
            what,
            key,
            command: rx,
            record,
            prompt,
            operation,
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
        let key = opening.key.clone();
        let launching = self.pending.iter().any(|p| p.session.session_id == key);
        let previous_focus = self.focus.map(|i| self.viewers[i].key.clone());
        match command {
            Ok(command) => {
                if self.open_traced(
                    self.size,
                    command,
                    &opening.what,
                    opening.key,
                    opening.record,
                    opening.operation,
                ) {
                    if launching {
                        // Starting a session leaves the list selected, as Claude does.
                        self.focus = previous_focus
                            .as_deref()
                            .and_then(|key| self.viewer_index(key));
                    }
                    self.status.clear();
                    self.invalidate();
                } else {
                    self.remove_launch(&key);
                    if self.text.is_empty()
                        && let Some(prompt) = opening.prompt
                    {
                        self.fill(prompt);
                    }
                }
            }
            Err(error) => {
                self.remove_launch(&key);
                // A start that never opens leaves no row, so the log is the only record of why.
                let failed = format!("{} failed: {error:#}", opening.what);
                self.event("error", "viewer.failed", || json!({
                    "operation_id": opening.operation.as_ref().map(|o| &o.id),
                    "row_id": key, "phase": "prepare", "error": format!("{error:#}"),
                    "duration_ms": opening.operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                }));
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
        self.event("debug", "viewer.cancelled", || json!({
            "operation_id": opening.operation.as_ref().map(|o| &o.id),
            "row_id": opening.key, "duration_ms": opening.operation.as_ref().map(DiagnosticOperation::elapsed_ms),
        }));
        self.remove_launch(&opening.key);
        if self.text.is_empty()
            && let Some(prompt) = opening.prompt
        {
            self.fill(prompt);
        }
        self.status = "opening cancelled".into();
        true
    }

    fn remove_launch(&mut self, id: &str) {
        if self.pending.iter().any(|p| p.session.session_id == id) {
            self.pending.retain(|p| p.session.session_id != id);
            self.data.sessions.retain(|s| s.session_id != id);
            self.rebuild();
        }
    }

    /// Record only this viewer's identified thread after a reported turn.
    fn record_codex(&mut self, id: &str) -> bool {
        let Some(s) = self.data.sessions.iter().find(|s| {
            (s.session_id == id || self.history_matches(id, s))
                && s.harness == "codex"
                && s.state != "-"
        }) else {
            return false;
        };
        let (Some(started), Some(rollout)) = (s.started, &s.transcript_path) else {
            return false;
        };
        match codex::remember(
            &self.state,
            codex::Thread {
                id: s.session_id.clone(),
                cwd: s.cwd.clone(),
                started,
                rollout: rollout.clone(),
            },
        ) {
            Ok(()) => true,
            Err(e) => {
                self.status = format!(
                    "could not record codex thread {}: {e}",
                    &id[..id.len().min(8)]
                );
                false
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
        if row
            .and_then(|r| r.kind.key())
            .is_some_and(|id| self.pending.iter().any(|p| p.session.session_id == id))
        {
            return "starting";
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
        self.leave_transcript();
        self.history.select_first = false;
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
        self.event("debug", "row.entered", || {
            json!({
                "operation_id": self.input_operation.as_ref().map(|(o, _)| &o.id),
                "row": self.row_context(&kind), "action": enter_verb(Some(&kind), self.menu),
            })
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
            Kind::Session(id, _) if self.pending.iter().any(|p| p.session.session_id == id) => {
                self.event(
                    "debug",
                    "action.refused",
                    || json!({"row_id": id, "reason": "launch_pending"}),
                );
                self.status =
                    "still starting · its row fills in when the harness reports it".into();
            }
            Kind::Session(id, _) => {
                let Some(s) = self.data.sessions.iter().find(|s| s.session_id == id) else {
                    return Ok(());
                };
                let (harness, own_terminal) = (s.harness.clone(), s.own_terminal());
                // Interactive clients in other terminals cannot be joined.
                if own_terminal {
                    self.event("debug", "action.refused", || {
                        json!({
                            "row_id": id, "harness": harness, "reason": "own_terminal",
                            "operation_id": self.input_operation.as_ref().map(|(o, _)| &o.id),
                        })
                    });
                    self.status = format!(
                        "{harness} runs in its own terminal and cannot be joined from here"
                    );
                    return Ok(());
                }
                let spec = harness::by_name(&harness).context("unknown session harness")?;
                let home = spec.session_home(&self.claude, s);
                if spec.session(s.kind.as_deref()).join == harness::spec::Join::CodexRemote {
                    let session = s.clone();
                    self.prepare_viewer(spec.commands.viewer.clone(), id, None, None, move || {
                        harness::join(&session, &home, false)
                    });
                    return Ok(());
                }
                match harness::join(s, &home, false) {
                    Ok(c) => {
                        self.open(self.size, c, &spec.commands.viewer, id, None);
                    }
                    Err(e) => {
                        self.event("error", "viewer.failed", || json!({
                            "row_id": id, "harness": harness, "phase": "join", "error": format!("{e:#}"),
                        }));
                        self.status = format!("attach failed: {e:#}");
                    }
                }
            }
            Kind::Run(id, _) => {
                let mut c = self.me();
                c.args(["__attach", &id]);
                self.open(self.size, c, "attach", format!("run:{id}"), None);
            }
            Kind::History(key) => {
                let Some(entry) = self.history.row(&key).cloned() else {
                    return Ok(());
                };
                if self.history_excluded().contains(&entry.key) {
                    self.event("debug", "action.refused", || json!({
                        "row_id": key, "harness": entry.key.harness, "session_id": entry.key.session_id,
                        "reason": "already_in_main_list",
                    }));
                    self.status = "session is already in the main list".into();
                    self.rebuild();
                    return Ok(());
                }
                let what = harness::by_name(&entry.key.harness)
                    .context("unknown history harness")?
                    .commands
                    .viewer
                    .clone();
                let record = (entry.key.harness == "codex")
                    .then_some(entry.started)
                    .flatten()
                    .map(|at| (entry.cwd.clone(), at));
                // A forgotten row is hidden by id, which would keep the revived session out of the
                // list: the client defers its row to the daemon and the daemon's row stays hidden.
                if let Err(e) =
                    Ledger::new(&self.state).and_then(|l| l.unhide(&entry.key.session_id))
                {
                    self.status = format!("could not restore this session's row: {e:#}");
                }
                self.history.opened.insert(key.clone(), entry.clone());
                self.prepare_viewer(what, key, record, None, move || history_command(&entry));
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
            "columns" => self.open_columns(None),
            _ => self.mode = Mode::Guide(Guide::default()),
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

    /// The composer's slot for a harness, or the first one config still offers when that
    /// harness is turned off. With none offered the composer comes up on the terminal.
    fn harness_at(kind: Option<HarnessKind>, policy: &config::Policy) -> usize {
        let all = harness::launchable();
        all.iter()
            .position(|k| Some(*k) == kind && policy.enabled_for(*k))
            .or_else(|| all.iter().position(|k| policy.enabled_for(*k)))
            .unwrap_or(all.len())
    }

    /// shift+tab visits the harnesses config offers and then the terminal, which is always
    /// reachable even with every harness turned off.
    fn cycle_harness(&mut self) {
        let policy = self.session_policy();
        let all = harness::launchable();
        self.harness = (1..=all.len() + 1)
            .map(|step| (self.harness + step) % (all.len() + 1))
            .find(|&i| i == all.len() || policy.enabled_for(all[i]))
            .unwrap_or(all.len());
    }

    fn session_policy(&self) -> config::Policy {
        config::defaults(&self.jobs_path)
    }

    fn start(&mut self) {
        if self.terminal_selected() {
            self.start_terminal();
            return;
        }
        if self.menu_is("jobs") || self.on_new_job() {
            self.new_job();
            return;
        }
        let dir = self.target_dir();
        let kind = harness::launchable()[self.harness];
        let policy = self.session_policy();
        let prompt = self.take_prompt();
        let what = format!("{kind} in {}", fleet::tilde(&dir));
        let since = chrono::Utc::now();
        let id = self.launch_row(kind, &dir, &prompt);
        // Foreground harnesses run as the dashboard's own client.
        let launch = harness::spec(kind)
            .launch
            .as_ref()
            .expect("composer launch operation");
        if launch.handler != harness::spec::LaunchHandler::ClaudeBackground {
            let record = (launch.identity == harness::spec::LaunchIdentity::ReportedThread)
                .then(|| (dir.clone(), since));
            let retry = Some(prompt.clone());
            // Use a temporary launch key until the harness reports the session's own id.
            self.prepare_viewer(what, id, record, retry, move || {
                match harness::start(kind, &dir, prompt.trim(), &policy)? {
                    Start::Foreground(command) => Ok(command),
                    Start::Background(_) => anyhow::bail!("expected a {kind} viewer"),
                }
            });
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.status = format!("starting {what}");
        let log = self.log.clone();
        let operation_id = id.clone();
        let started = Instant::now();
        std::thread::spawn(move || {
            let mut preparation_ms = None;
            let mut command_ms = None;
            // Capability checks and the command both run off the input thread.
            let result = (|| -> Result<String> {
                let Start::Background(mut command) =
                    harness::start(kind, &dir, prompt.trim(), &policy)?
                else {
                    anyhow::bail!("expected a background Claude session");
                };
                preparation_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                let executing = Instant::now();
                let output = command.stdin(Stdio::null()).output();
                command_ms = Some(executing.elapsed().as_secs_f64() * 1000.0);
                let output = output?;
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
            if let Some(log) = log {
                log.event(
                    if result.is_err() { "error" } else { "debug" },
                    "launch.result",
                    json!({
                        "operation_id": operation_id, "harness": kind,
                        "outcome": if result.is_ok() { "started" } else { "failed" },
                        "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
                        "preparation_ms": preparation_ms, "command_ms": command_ms,
                        "native_id_prefix": result.as_ref().ok().and_then(|s| short_id(s)),
                        "error": result.as_ref().err().map(|e| format!("{e:#}")),
                    }),
                );
            }
            let feedback = match result {
                Ok(message) => (message, None),
                Err(error) => (format!("{what} failed: {error:#}"), Some(prompt)),
            };
            let _ = tx.send(feedback);
        });
        self.started.push((id, rx));
    }

    fn terminal_selected(&self) -> bool {
        self.harness == harness::launchable().len()
    }

    fn composer_text(&self) -> &str {
        if self.terminal_selected() {
            &self.terminal_input.text
        } else {
            &self.text
        }
    }

    fn composer_input_mut(&mut self) -> (&mut String, &mut usize) {
        if self.terminal_selected() {
            (&mut self.terminal_input.text, &mut self.terminal_input.at)
        } else {
            (&mut self.text, &mut self.caret)
        }
    }

    fn launch_name(&self) -> String {
        if self.terminal_selected() {
            "terminal".into()
        } else {
            harness::launchable()[self.harness].to_string()
        }
    }

    fn start_terminal(&mut self) {
        let dir = self.target_dir();
        let command = match terminal::command(&self.shell, &dir, &mut self.shell_startup) {
            Ok(command) => command,
            Err(e) => {
                self.status = format!("terminal failed: {e}");
                return;
            }
        };
        let name = self
            .shell
            .file_name()
            .unwrap_or(self.shell.as_os_str())
            .to_string_lossy()
            .into_owned();
        let id = format!("terminal:{}", uuid::Uuid::new_v4());
        if !self.open(self.size, command, &name, id.clone(), None) {
            return;
        }
        let input = std::mem::take(&mut self.terminal_input);
        if !input.text.trim().is_empty() {
            let mut bytes = input.text.replace('\n', "\r").into_bytes();
            bytes.push(b'\r');
            self.focused().unwrap().viewer.write(&bytes);
        }
        let session = Session {
            session_id: id.clone(),
            harness: "terminal".into(),
            kind: None,
            cwd: dir,
            state: "-".into(),
            started: Some(chrono::Utc::now()),
            last_activity: None,
            model: None,
            pid: self.focus.map(|i| self.viewers[i].viewer.pid()),
            transcript_path: None,
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            context_window: None,
            cost_usd: None,
            cost_info: None,
            title: Some(name),
            last: None,
            coordinator: false,
            activity: Vec::new(),
        };
        self.terminals.push(session.clone());
        self.data.sessions.push(session);
        self.jobs_view = false;
        self.history.select_first = false;
        self.rebuild();
        if let Some(i) = self
            .visible
            .iter()
            .position(|&i| self.rows[i].kind.key() == Some(id.as_str()))
        {
            self.cursor = i;
            self.settle();
        }
        self.status.clear();
    }

    fn launch_row(&mut self, kind: HarnessKind, dir: &Path, prompt: &str) -> String {
        self.history.select_first = false;
        let nonce = uuid::Uuid::new_v4();
        let id = match kind {
            HarnessKind::Claude => format!("starting:{nonce}"),
            _ => format!("{kind}:start:{nonce}"),
        };
        let recovery = self.state.join("launches.jsonl");
        if let Err(error) = debug_line(
            &recovery,
            json!({
                "v": 1, "timestamp": chrono::Utc::now().to_rfc3339(),
                "pid": std::process::id(), "dashboard_id": self.dashboard_id,
                "event": "launch.submitted", "level": "recovery",
                "data": {"operation_id": id, "harness": kind, "cwd": dir.to_string_lossy(), "prompt": prompt},
            }),
        ) {
            self.event("error", "recovery.failed", || {
                json!({
                    "operation_id": id, "path": recovery.to_string_lossy(), "error": error.to_string(),
                })
            });
        }
        self.event("debug", "launch.started", || json!({
            "operation_id": id,
            "parent_operation_id": self.input_operation.as_ref().map(|(o, _)| &o.id),
            "harness": kind, "cwd": dir.to_string_lossy(), "prompt_bytes": prompt.len(), "recovery_file": recovery.to_string_lossy(),
        }));
        let session = placeholder(kind, &id, dir, prompt);
        if let Some(d) = &mut self.data.diagnostics {
            d.sources.insert(
                format!("{kind}:{id}"),
                json!({"reader": "pending_launch", "operation_id": id}),
            );
        }
        self.data.sessions.push(session.clone());
        self.pending.push(Pending {
            session,
            short: None,
            at: Instant::now(),
        });
        self.rebuild_with_reason("launch_placeholder");
        self.select_new(&id);
        id
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
            .and_then(|s| {
                harness::by_name(&s.harness).map(|spec| spec.session(s.kind.as_deref()).stop)
            }) {
            Some(harness::spec::Stop::ForgetClient) => "forget",
            Some(harness::spec::Stop::Remove) => "delete",
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
            Kind::Session(id, _)
                if self.pending.iter().any(|p| p.session.session_id == *id)
                    && self.viewer_index(id).is_none() =>
            {
                None
            }
            Kind::Session(id, _) => Some(self.session_verb(id)),
            Kind::Folder(_) => Some("remove"),
            _ => None,
        }
    }

    fn composer(&self) -> Line<'static> {
        if self.terminal_selected() {
            let shell = self.shell.file_name().unwrap_or(self.shell.as_os_str());
            let mut spans = vec![Span::styled(
                format!("terminal ({}) › ", shell.to_string_lossy()),
                bold(),
            )];
            let input = &self.terminal_input;
            let shown = input.text.replace('\n', "⏎");
            let caret = input.text[..snap(&input.text, input.at)]
                .replace('\n', "⏎")
                .len();
            spans.extend(typed(&shown, caret, "Type a command, or Enter to open"));
            return Line::from(spans);
        }
        if self.on_button() {
            return Line::default();
        }
        let kind = harness::launchable()[self.harness].to_string();
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
        if self.transcript.focused {
            return self.transcript_hints();
        }
        let prefix = (!self.filter.text.is_empty()).then(|| {
            let label = if self.history.visible {
                "search"
            } else {
                "filter"
            };
            Span::styled(format!("{label}: {}  ", self.filter.text), dim())
        });
        let mut line = if self.focus.is_some_and(|i| self.viewers[i].is_terminal()) {
            let mut keys = vec![];
            if self.viewers[self.focus.unwrap()].what == "zsh" {
                keys.push(("←/tab", "back on empty"));
            }
            keys.extend([
                ("ctrl+z", "back"),
                ("ctrl+c", "interrupt"),
                ("ctrl+\\", "full screen"),
            ]);
            hints(&keys)
        } else if let Some(i) = self.focus {
            hints(&[
                (self.viewers[i].return_key(), "back"),
                ("ctrl+\\", "full screen"),
            ])
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
        let start = if !self.terminal_selected() && (self.menu_is("jobs") || self.on_new_job()) {
            "new job with it".to_owned()
        } else {
            format!(
                "start {} in {}",
                self.launch_name(),
                fleet::tilde(&self.target_dir())
            )
        };
        match &self.mode {
            Mode::Filter => hints(&[
                (
                    "enter",
                    if self.history.visible {
                        "keep the search"
                    } else {
                        "keep the filter"
                    },
                ),
                ("esc", "clear it"),
            ]),
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
            Mode::Columns(form) => form.hints(),
            Mode::Config(form) => form.hints(),
            Mode::Guide(guide) => guide.hints(),
            Mode::Folder(_) => hints(&[
                ("enter", "add"),
                ("tab", "complete"),
                ("↑ ↓", "recent"),
                ("esc", "cancel"),
            ]),
            Mode::Rename(_) => hints(&[("enter", "rename"), ("esc", "cancel")]),
            Mode::Normal if self.history_selected() => {
                let mut keys = vec![("↑ ↓", "select")];
                if matches!(self.selected().map(|r| &r.kind), Some(Kind::History(_))) {
                    keys.push(("enter", self.enter_label()));
                    keys.push(("tab", "pane"));
                }
                if !self.filter.text.is_empty() {
                    keys.push(("esc", "clear filter"));
                }
                keys.push(("ctrl+h", "hide history"));
                hints(&keys)
            }
            Mode::Normal if self.terminal_selected() => {
                let mut keys = vec![("enter", start.as_str())];
                if self.focusable_viewer().is_some() {
                    keys.push(("tab", "pane"));
                }
                if let Some(verb) = self.stop_verb() {
                    keys.push(("ctrl+x", verb));
                }
                keys.push(("shift+tab", "session"));
                keys.push(("esc", "back"));
                hints(&keys)
            }
            Mode::Normal if !self.text.is_empty() => {
                hints(&[("enter", &start), ("shift+tab", "session")])
            }
            Mode::Normal => {
                let mut keys = vec![];
                if self.selected().is_some() {
                    keys.push(("enter", self.enter_label()));
                }
                if !self.jobs_view {
                    keys.push((
                        "ctrl+h",
                        if self.history.visible {
                            "hide history"
                        } else {
                            "history"
                        },
                    ));
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
                if self.focusable_viewer().is_some()
                    || self.panel_shown()
                    || self.transcript_target().is_some()
                {
                    keys.push(("tab", "pane"));
                    keys.push(("ctrl+\\", "layout"));
                }
                keys.push(("shift+tab", "session"));
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
            Some(Kind::Session(id, _))
                if self.pending.iter().any(|p| p.session.session_id == id)
                    && self.viewer_index(&id).is_none() =>
            {
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
                let local = self.selected().and_then(|r| self.viewer_of(&r.kind));
                if let Some(i) = local.filter(|&i| self.viewers[i].is_terminal()) {
                    self.close(i);
                    self.status = "terminal closed".into();
                    return;
                }
                let ended_with_viewer = local.is_some()
                    && self.data.sessions.iter().any(|s| {
                        s.session_id == id
                            && harness::by_name(&s.harness).is_some_and(|spec| {
                                spec.launch
                                    .as_ref()
                                    .is_some_and(|launch| launch.identity.owns_client_pid())
                            })
                    });
                // Capture the native pid before closing can remove a launch placeholder.
                let client = self
                    .data
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .and_then(|s| s.pid);
                if let Some(i) = local {
                    self.close(i);
                }
                for key in [id.clone(), format!("run:{id}")] {
                    if let Some(i) = self.viewer_index(&key) {
                        self.close(i);
                    }
                }
                let (state, claude, target) = (self.state.clone(), self.claude.clone(), id.clone());
                // Terminate the attached client to release the daemon-held thread; it remains resumable.
                // No registry lists a Codex client, so signal its pid rather than looking it up.
                self.queue_stop(id, verb, move || {
                    if verb == "forget" {
                        codex::forget(&state, &target)?;
                        // The daemon keeps the thread's writer lock for minutes after the client
                        // goes, so dropping the record alone lets the row return on the next start.
                        Ledger::new(&state)?.hide(&target)?;
                        if !ended_with_viewer && let Some(pid) = client {
                            fleet::terminate(pid, "codex")?;
                        }
                        Ok(true)
                    } else if ended_with_viewer {
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
        let operation = self.log.as_ref().map(|_| DiagnosticOperation::new());
        let context = self.key_context(&id);
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
            operation: operation.clone(),
            context: context.clone(),
        };
        self.status = action.message();
        self.event("debug", "action.started", || {
            json!({
                "operation_id": operation.as_ref().map(|o| &o.id),
                "parent_operation_id": self.input_operation.as_ref().map(|(o, _)| &o.id),
                "action": verb, "row": context, "row_id": action.id,
            })
        });
        let log = self.log.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = work();
            if let Some(log) = log {
                log.event(if result.is_err() { "error" } else { "debug" }, "action.executed", json!({
                    "operation_id": operation.as_ref().map(|o| &o.id),
                    "action": verb, "row": context,
                    "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
                    "outcome": match &result { Ok(true) => "applied", Ok(false) => "already_finished", Err(_) => "failed" },
                    "error": result.as_ref().err().map(|e| format!("{e:#}")),
                }));
            }
            let _ = tx.send(result);
        });
        self.stopping.push(action);
        self.rebuild_with_reason("action_pending");
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
            self.event(if result.is_err() { "error" } else { "debug" }, "action.completed", || json!({
                "operation_id": action.operation.as_ref().map(|o| &o.id),
                "action": action.verb, "row": action.context, "row_id": action.id,
                "duration_ms": action.operation.as_ref().map(DiagnosticOperation::elapsed_ms),
                "outcome": match &result { Ok(true) => "applied", Ok(false) => "already_finished", Err(_) => "failed" },
                "error": result.as_ref().err().map(|e| format!("{e:#}")),
            }));
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
        }
        if finished {
            self.rebuild_with_reason("action_result");
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
            let terminal = open.is_terminal();
            if open.returns_to_list(code, mods) {
                self.unfocus();
                return Ok(false);
            }
            // ctrl+\ arrives as the byte 0x1c, which crossterm reports as ctrl+4.
            if ctrl && matches!(code, KeyCode::Char('\\' | '4')) {
                self.toggle_split();
                return Ok(false);
            }
            // ctrl+c never reaches agent clients: Claude Code, Codex and pi all quit on two of
            // them, and Claude Code's first one drops to the agents list. It is the
            // dashboard's quit key here as it is from the list; esc interrupts the client.
            if !terminal && ctrl && code == KeyCode::Char('c') {
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
        if self.transcript.focused {
            if ctrl && code == KeyCode::Char('c') {
                return Ok(self.quit_press());
            }
            match code {
                KeyCode::Tab | KeyCode::Esc | KeyCode::Left => self.leave_transcript(),
                KeyCode::Char('z') if ctrl => self.leave_transcript(),
                KeyCode::Char('\\' | '4') if ctrl => self.toggle_split(),
                KeyCode::Char('h') if ctrl => {
                    self.leave_transcript();
                    self.toggle_history();
                }
                KeyCode::Char('r') if ctrl => {
                    self.transcript.requested = false;
                    self.transcript.document = None;
                    self.transcript.error = None;
                    self.transcript.width = 0;
                    self.transcript.request_cursor = None;
                    self.transcript.load_older = false;
                    self.transcript.prepend_lines = None;
                    self.transcript.bottom = true;
                }
                KeyCode::Up => self.transcript.scroll(-1),
                KeyCode::Down => self.transcript.scroll(1),
                KeyCode::PageUp => self
                    .transcript
                    .scroll(-(self.transcript.height.max(1) as isize)),
                KeyCode::PageDown => self
                    .transcript
                    .scroll(self.transcript.height.max(1) as isize),
                KeyCode::Home => {
                    self.transcript.scroll = 0;
                    self.transcript.bottom = false;
                    self.transcript.load_older = true;
                }
                KeyCode::End => {
                    self.transcript.scroll = self.transcript.max_scroll();
                    self.transcript.bottom = true;
                    self.transcript.load_newer = true;
                }
                KeyCode::Enter => {
                    self.leave_transcript();
                    self.full = mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                    self.enter()?;
                }
                _ => {}
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
                Mode::Guide(..) | Mode::Columns(_) => true,
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
                    // Nothing to the left of an empty prompt, so ← leaves it.
                    KeyCode::Left if self.filter.text.is_empty() => self.mode = Mode::Normal,
                    _ => {
                        self.filter.key(code, mods);
                    }
                }
                self.filter_changed();
            }
            Mode::Guide(guide) => {
                if guide.key(code, mods) {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Folder(input) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Left if input.text.is_empty() => self.mode = Mode::Normal,
                KeyCode::Up | KeyCode::Down if !self.data.recent.is_empty() => {
                    let recent: Vec<String> =
                        self.data.recent.iter().map(|p| fleet::tilde(p)).collect();
                    let at = recent.iter().position(|r| *r == input.text);
                    let n = recent.len();
                    // The list is on screen newest first, so the keys follow its rows and
                    // both start at the newest.
                    let next = match (code, at) {
                        (_, None) => 0,
                        (KeyCode::Up, Some(i)) => (i + n - 1) % n,
                        (_, Some(i)) => (i + 1) % n,
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
            Mode::Columns(form) => {
                if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    let action = form.key(code);
                    self.column_action(action);
                }
            }
            Mode::Config(form) => {
                let before = form.clone();
                let action = form.key(code, mods);
                self.config_action(action, before);
            }
            Mode::Normal => {
                let armed = self.armed.take();
                let searching_history = self.history_selected();
                if self.on_button() && matches!(code, KeyCode::Left | KeyCode::Right) {
                    let n = MENU.len();
                    self.menu = (self.menu + if code == KeyCode::Right { 1 } else { n - 1 }) % n;
                    return Ok(false);
                }
                // Right on a row with nothing typed goes to the agent: into the pane when it is
                // open, over the whole frame when it is closed.
                if !self.on_button()
                    && code == KeyCode::Right
                    && mods.is_empty()
                    && if searching_history {
                        self.filter.text.is_empty()
                    } else {
                        self.composer_text().is_empty()
                    }
                {
                    match self.focusable_viewer() {
                        Some(i) => self.focus(i),
                        None if self.transcript_target().is_some() => self.focus_transcript(),
                        None if self.panel_shown() => self.open_menu(),
                        // ponytail: only a session row attaches on its own; right never launches.
                        None if matches!(
                            self.selected().map(|r| &r.kind),
                            Some(Kind::Session(..))
                        ) =>
                        {
                            self.enter()?
                        }
                        None => self.status = "nothing in the pane".into(),
                    }
                    return Ok(false);
                }
                // ← leaves the jobs screen the way tab and ctrl+z do, with nothing typed.
                if self.jobs_view
                    && code == KeyCode::Left
                    && mods.is_empty()
                    && !self.on_button()
                    && self.composer_text().is_empty()
                {
                    self.leave_jobs();
                    self.needs_clear = true;
                    return Ok(false);
                }
                if searching_history {
                    if self.filter.key(code, mods) {
                        self.filter_changed();
                        return Ok(false);
                    }
                } else if !self.on_button() {
                    let (text, caret) = self.composer_input_mut();
                    if let Some(at) = edit(text, *caret, code, mods) {
                        *caret = at;
                        return Ok(false);
                    }
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
                        } else if searching_history && !self.filter.text.is_empty() {
                            self.filter = Input::default();
                            self.filter_changed();
                        } else if searching_history {
                            self.toggle_history();
                        } else if self.terminal_selected() && !self.terminal_input.text.is_empty() {
                            self.terminal_input = Input::default();
                        } else if self.terminal_selected() {
                            self.harness = Self::harness_at(
                                Some(self.data.start.harness),
                                &self.session_policy(),
                            );
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
                    KeyCode::PageUp | KeyCode::PageDown if self.history.visible => {
                        let delta = if code == KeyCode::PageDown { 1 } else { -1 };
                        for _ in 0..self.list_area.height.saturating_sub(1).max(1) {
                            self.step(delta);
                        }
                    }
                    KeyCode::Tab => match self.focusable_viewer() {
                        Some(i) => self.focus(i),
                        None if self.transcript_target().is_some() => self.focus_transcript(),
                        None if self.jobs_view => self.leave_jobs(),
                        None if self.panel_shown() => self.open_menu(),
                        None => self.status = "nothing in the pane".into(),
                    },
                    KeyCode::BackTab => self.cycle_harness(),
                    KeyCode::Enter if searching_history => {
                        if matches!(self.selected().map(|r| &r.kind), Some(Kind::History(_))) {
                            self.full = mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                            self.enter()?;
                        }
                    }
                    // With a draft, shift+enter adds a line for either launch type.
                    KeyCode::Enter
                        if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && !self.composer_text().trim().is_empty() =>
                    {
                        let (text, caret) = self.composer_input_mut();
                        let at = snap(text, *caret);
                        text.insert(at, '\n');
                        *caret = at + 1;
                    }
                    KeyCode::Enter if self.terminal_selected() => {
                        self.full = mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                        self.start();
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
                    KeyCode::Enter => self.start(),
                    KeyCode::Char('s') if ctrl => {
                        self.by_state = !self.by_state;
                        self.rebuild();
                    }
                    KeyCode::Char('p') if ctrl => self.pin_selected(),
                    KeyCode::Char('\\' | '4') if ctrl => self.toggle_split(),
                    KeyCode::Char('e') if ctrl => self.edit_job(),
                    KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
                    KeyCode::Char('g') if ctrl => self.mode = Mode::Guide(Guide::default()),
                    KeyCode::Char('h') if ctrl && !self.jobs_view => self.toggle_history(),
                    KeyCode::Char('n') if ctrl => self.rename_selected(),
                    KeyCode::Char('r') if ctrl => {
                        if self.history.visible {
                            self.history.select_first = searching_history;
                            self.history.reset(&self.filter.text, true);
                            self.rebuild();
                        }
                        self.invalidate();
                        self.status = "refresh requested".into();
                    }
                    KeyCode::Char('v')
                        if ctrl && !self.terminal_selected() && !searching_history =>
                    {
                        self.attach_image()
                    }
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
                None if self.transcript_shown() => self.draw_transcript(frame, inner),
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
        if self.transcript.focused && self.transcript_target().is_some() {
            self.draw_transcript(frame, self.pane);
            if area.height >= 2 {
                let foot = Rect {
                    y: area.bottom() - 1,
                    height: 1,
                    ..area
                };
                frame.render_widget(Paragraph::new(self.transcript_hints()), foot);
            }
            return;
        }
        self.draw_dashboard(frame, area);
    }

    fn transcript_hints(&self) -> Line<'static> {
        if self.quitting() {
            return Line::styled(QUIT_HINT, Style::default().fg(Color::Red));
        }
        hints(&[
            ("↑ ↓", "scroll"),
            ("enter", self.enter_label()),
            ("tab", "list"),
            ("ctrl+\\", "layout"),
        ])
    }

    fn draw_transcript(&mut self, frame: &mut Frame, pane: Rect) {
        let Some(target) = self.transcript_target() else {
            return;
        };
        let pane = if pane.width > 2 {
            Rect {
                x: pane.x + 1,
                width: pane.width - 2,
                ..pane
            }
        } else {
            pane
        };
        let run = matches!(self.selected().map(|r| &r.kind), Some(Kind::Run(..)));
        let (title, subtitle) = match self.selected().map(|r| &r.kind) {
            Some(Kind::Run(id, _)) => {
                let Some(run) = self.data.runs.iter().find(|r| &r.started.run_id == id) else {
                    return;
                };
                let last = run.terminal.as_ref().unwrap_or(&run.started);
                let title = format!(
                    "{} · {}",
                    run.started.job.as_deref().unwrap_or("run"),
                    run.status()
                );
                let mut subtitle = "output · read only".to_owned();
                if let Some(reason) = &last.reason {
                    subtitle.push_str(&format!(" · {reason}"));
                }
                (title, subtitle)
            }
            _ => (
                format!("{} · history · read only", logo(&target.harness)),
                String::new(),
            ),
        };
        let header = Line::styled(
            clip(&transcript::plain(&title), pane.width as usize),
            if run { bold() } else { dim() },
        );
        frame.render_widget(
            Paragraph::new(header),
            Rect {
                height: pane.height.min(1),
                ..pane
            },
        );
        if pane.height < 2 {
            return;
        }
        if run {
            frame.render_widget(
                Paragraph::new(Line::styled(transcript::plain(&subtitle), dim())),
                Rect {
                    y: pane.y + 1,
                    height: 1,
                    ..pane
                },
            );
        }
        let header_rows = if run { 2 } else { 1 };
        let body = Rect {
            y: pane.y + header_rows,
            height: pane.height.saturating_sub(header_rows),
            ..pane
        };
        // Drawing may precede the next tick after a key or click. Never reuse another row's text.
        if self.transcript.target.as_ref() != Some(&target) {
            return;
        }
        self.transcript
            .layout(body.width, body.height, &self.colors);
        let lines: Vec<_> = self
            .transcript
            .lines
            .iter()
            .skip(self.transcript.scroll)
            .take(usize::from(body.height))
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(lines), body);
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
            Mode::Normal if self.history_selected() => {
                let mut spans = vec![Span::styled("history / ", bold())];
                spans.extend(self.filter.spans("Type to search history"));
                Line::from(spans)
            }
            Mode::Filter => {
                let mut spans = vec![Span::styled("/ ", bold())];
                spans.extend(self.filter.spans(if self.history.visible {
                    "Search past conversations"
                } else {
                    "text a row must contain"
                }));
                Line::from(spans)
            }
            Mode::Job(f) => f.line(),
            Mode::Config(f) => f.line(),
            Mode::Columns(f) => f.line(),
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
            Mode::Guide(guide) => {
                let mut spans = vec![Span::styled("search › ", Style::default().fg(ORANGE))];
                spans.extend(guide.find.spans("Type a key or topic"));
                Line::from(spans)
            }
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
        let mut rows = input.line_count(width).clamp(3, 10) as u16;
        if let Mode::Columns(form) = &self.mode {
            // Keep the table's height when a checkbox or its description changes.
            rows = rows.max(form.prompt_rows(width));
        }
        if let Mode::Config(form) = &self.mode {
            rows = form.prompt_rows(width);
        }
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
        match (&mut self.mode, name) {
            (Mode::Columns(form), _) => form.draw(frame, body),
            (Mode::Job(form), _) => frame.render_widget(form.paragraph(body), body),
            (Mode::Config(form), _) => form.draw(frame, body),
            (Mode::Guide(guide), _) => guide.draw(frame, body, true),
            (_, "help") => Guide::default().draw(frame, body, false),
            // Config previews reread jobs.yaml every frame. Cache the form in rebuild
            // if profiling shows this cost.
            (_, "config") => self.config_form().draw(frame, body),
            (_, "columns") => {
                ColumnsPicker::new(&self.jobs_path, self.columns_tab()).draw(frame, body)
            }
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

    /// Align with Claude and pi's lower input rule, even while viewing history.
    /// Without a visible rule, reserve only the hint row.
    fn foot_rows(&self) -> u16 {
        if self.data.pane.at == "bottom" {
            return 1;
        }
        let Some(i) = self.shown() else { return 1 };
        let open = &self.viewers[i];
        // Codex's composer is not a bottom-anchored input box; logs have no input box.
        if open.harness.is_none_or(|kind| {
            harness::spec(kind).viewer.input_alignment != harness::spec::InputAlignment::BottomRule
        }) {
            return 1;
        }
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
        // pi's regular renderer can begin near the top before its transcript fills
        // the terminal. A distant rule must not pull the dashboard composer upward.
        (rows.saturating_sub(7)..rows)
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
        if self.focus.is_some() || self.transcript.focused || (in_pane && self.panel_focused()) {
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
        } else if let Mode::Guide(guide) = &mut self.mode {
            guide.draw(frame, list, true);
        } else if let Mode::Job(form) = &self.mode {
            frame.render_widget(form.paragraph(list), list);
        } else if let Mode::Config(form) = &mut self.mode {
            form.draw(frame, list);
        } else if let Mode::Columns(form) = &mut self.mode {
            form.draw(frame, list);
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
        self.on_menu()
            && !self.history_selected()
            && !self.terminal_selected()
            && self.text.is_empty()
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
                    Kind::Session(..)
                        | Kind::History(_)
                        | Kind::Job(_)
                        | Kind::Run(..)
                        | Kind::Columns
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

pub fn run(
    exe: &Path,
    jobs_path: &Path,
    state: &Path,
    claude: &Path,
    debug: bool,
    trace: bool,
) -> Result<i32> {
    let started = Instant::now();
    crate::cost::init(state, true);
    let log = if debug || trace {
        std::fs::create_dir_all(state)?;
        let log = Diagnostics::new(state.join("tui-debug.log"), trace);
        log.event(
            "debug",
            "dashboard.started",
            json!({
                "build": executable_identity(exe), "jobs_path": jobs_path.to_string_lossy(),
                "state_dir": state.to_string_lossy(), "native_home": claude.to_string_lossy(),
                "terminal": term_state(), "trace": trace,
                "terminal_size": ratatui::crossterm::terminal::size().ok(),
            }),
        );
        Some(log)
    } else {
        None
    };
    let mut app = App::new_logged(exe, jobs_path, state, claude, log)?;
    app.report_load();
    app.timing("startup_load", started);
    app.feedback = Some(("startup_to_draw", started));
    app.rebuild();
    app.loading_operation = None;
    app.report_view("startup");
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
    app.event(
        "debug",
        "terminal.colors",
        || json!({"foreground": app.colors.fg, "background": app.colors.bg}),
    );
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
            app.history_tick();
            let dirty = app.pump();
            app.prespawn_tick();
            app.transcript_tick();
            app.expire();
            app.report_view("event_loop");
            app.summarize_timings(false);
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
                let drawn_at = Instant::now();
                app.measured(
                    "draw",
                    drawn_at.duration_since(drawing).as_secs_f64() * 1000.0,
                    || {
                        json!({
                            "width": app.size.1, "height": app.size.0,
                        })
                    },
                );
                if let Some((phase, started)) = app.feedback.take()
                    && phase != "input_to_draw"
                {
                    app.measured(
                        phase,
                        drawn_at.duration_since(started).as_secs_f64() * 1000.0,
                        || json!({}),
                    );
                }
                if let Some((operation, route)) = app.input_operation.take() {
                    app.measured(
                        "input_to_draw",
                        drawn_at.duration_since(operation.started).as_secs_f64() * 1000.0,
                        || {
                            json!({
                                "operation_id": operation.id, "route": route,
                            })
                        },
                    );
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
                app.log_input(&e);
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
                app.report_view("input");
            }
        }
    })();
    // Hand the terminal back before reaping. Viewers draw on their own ptys, so a slow
    // reap has nothing left to say to this screen, and quitting feels immediate.
    ratatui::restore();
    hand_back_tty();
    // Viewers die with the dashboard: their process groups, never the agents behind them.
    let reaping = Instant::now();
    for open in &app.viewers {
        app.event("debug", "viewer.closed", || {
            json!({
                "operation_id": open.operation.as_ref().map(|o| &o.id),
                "row_id": open.key, "viewer_pid": open.viewer.pid(), "reason": "dashboard_exit",
            })
        });
    }
    app.viewers.clear();
    app.timing("reap", reaping);
    app.summarize_timings(true);
    app.event(if result.is_err() { "error" } else { "debug" }, "dashboard.stopped", || json!({
        "reason": if signalled.load(Ordering::Relaxed) { "signal" } else if result.is_err() { "error" } else { "quit" },
        "error": result.as_ref().err().map(|e| format!("{e:#}")),
        "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
    }));
    result.context("dashboard")?;
    Ok(0)
}

fn executable_identity(exe: &Path) -> Value {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let fingerprint = (|| -> std::io::Result<String> {
        let mut file = std::fs::File::open(exe)?;
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
        }
        Ok(format!("{:x}", hash.finalize()))
    })();
    json!({
        "version": env!("CARGO_PKG_VERSION"), "executable": exe.to_string_lossy(),
        "sha256": fingerprint.as_ref().ok(),
        "fingerprint_error": fingerprint.as_ref().err().map(ToString::to_string),
    })
}

fn debug_line(path: &Path, mut record: Value) -> std::io::Result<()> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

    const MAX_BYTES: usize = 10 * 1024 * 1024;
    let mut bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    if bytes.len() >= MAX_BYTES {
        let mut preview = record["data"].to_string();
        let mut end = preview.len().min(64 * 1024);
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
        let mut truncated = json!({
            "truncated": true,
            "original_bytes": bytes.len(),
            "preview": preview,
        });
        for field in [
            "operation_id",
            "parent_operation_id",
            "row_id",
            "row_kind",
            "harness",
            "session_id",
        ] {
            if let Some(value) = record["data"].get(field)
                && value.to_string().len() < 4096
            {
                truncated[field] = value.clone();
            }
        }
        record["data"] = truncated;
        bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    }
    if bytes.len() >= MAX_BYTES {
        return Err(std::io::Error::other(
            "diagnostic metadata exceeds the log bound",
        ));
    }
    bytes.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    // Append needs one buffer; compaction additionally needs a lock shared by all writers.
    // Keep the inode so another dashboard cannot append to a file we just rotated away.
    f.lock_exclusive()?;
    let mut len = f.metadata()?.len();
    if len.saturating_add(bytes.len() as u64) > MAX_BYTES as u64 {
        let keep = (MAX_BYTES / 2).min(MAX_BYTES - bytes.len()) as u64;
        f.seek(SeekFrom::Start(len.saturating_sub(keep)))?;
        let mut tail = Vec::with_capacity(keep as usize);
        (&mut f).take(keep).read_to_end(&mut tail)?;
        let start = tail
            .iter()
            .position(|&b| b == b'\n')
            .map_or(tail.len(), |i| i + 1);
        f.set_len(0)?;
        f.write_all(&tail[start..])?;
        len = (tail.len() - start) as u64;
    }
    if let Err(error) = f.write_all(&bytes) {
        let _ = f.set_len(len);
        return Err(error);
    }
    Ok(())
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
    fn config_tabs_remember_selection_and_keep_navigation_in_the_group() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let none = KeyModifiers::NONE;
        c.key(KeyCode::End, none);
        assert_eq!(c.field().name, "activity.bound");
        c.key(KeyCode::Down, none);
        assert_eq!(c.field().name, "activity.bound");
        c.key(KeyCode::Char(']'), none);
        assert_eq!(c.field().name, "check");
        c.key(KeyCode::Down, none);
        c.key(KeyCode::Char('['), none);
        assert_eq!(c.field().name, "activity.bound");
        c.key(KeyCode::Char(']'), none);
        assert_eq!(c.field().name, "claude_enabled");
        c.key(KeyCode::Char(']'), none);
        assert_eq!(c.field().name, "harness");
        // ↑ past the first field lands on the tab row, where ←→ pick a group and ↓ enters it.
        c.key(KeyCode::Up, none);
        assert!(c.tabs);
        c.key(KeyCode::Right, none);
        assert_eq!(
            GROUPS[c.tab()].0,
            "cones",
            "the row wraps like the menu buttons"
        );
        assert!(c.tabs, "picking a group keeps the tab row");
        c.key(KeyCode::Left, none);
        assert_eq!(GROUPS[c.tab()].0, "runs");
        c.key(KeyCode::Down, none);
        assert!(!c.tabs, "↓ goes back to the fields");
        assert_eq!(c.field().name, "harness", "the group keeps its field");
        c.key(KeyCode::Right, none);
        assert!(
            !c.values[c.row].is_empty(),
            "→ in the fields changes a value"
        );
        c.values[c.row].clear();
        assert!(
            c.values.iter().all(String::is_empty),
            "navigation saves nothing"
        );

        let p = config::Policy {
            env: Some(vec!["FOO".to_owned(), "BAR".to_owned()]),
            archive_transcript: Some(true),
            ..Default::default()
        };
        let c = ConfigForm::new(&p, None, None, None, None, None, None, None, None, None);
        assert_eq!(c.values[field_at("env")], "FOO, BAR");
        assert_eq!(c.values[field_at("archive_transcript")], "true");
        let saved = c.config().unwrap().0;
        assert_eq!(
            (saved.env, saved.archive_transcript),
            (p.env.clone(), p.archive_transcript),
            "a run field the file names comes back from its row unchanged"
        );

        let mut c = ConfigForm::new(&p, None, None, None, None, None, None, None, None, None);
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
        assert!(c.choice.is_some(), "enter exposes the complete choice list");
        assert_eq!(c.row, field_at("model"));
        c.key(KeyCode::Esc, none);

        for (name, want) in [
            ("model", "harness default"),
            ("bedrock", "harness default"),
            ("aws_profile", "AWS default"),
            ("aws_region", "AWS default"),
        ] {
            c.go(field_at(name));
            assert_eq!(
                c.default_label(),
                want,
                "{name} names who answers it when cones passes nothing"
            );
        }

        c.go(field_at("columns"));
        let before = c.values.clone();
        assert_eq!(c.key(KeyCode::Right, none), ConfigAction::Columns);
        assert_eq!(c.values, before, "the link changes no settings");
        c.key(KeyCode::Down, none);
        assert_eq!(c.field().name, "whole_columns");
        c.key(KeyCode::Up, none);
        assert_eq!(c.field().name, "columns");
        let text = c
            .lines(60)
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("open picker"));
        for old in [
            "session columns",
            "run columns",
            "job columns",
            "history columns",
        ] {
            assert!(!text.contains(old), "{old} moved into the picker");
        }

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
    fn config_rows_stay_compact_and_the_selected_help_is_outside_the_list() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        for width in [12, 30, 48, 60, 120] {
            for tab in 0..3 {
                c.switch(tab);
                let (lines, _) = c.lines(width);
                let headings = c
                    .fields()
                    .iter()
                    .map(|&i| FIELDS[i].sub)
                    .filter(|s| !s.is_empty())
                    .collect::<HashSet<_>>()
                    .len();
                assert_eq!(
                    lines
                        .iter()
                        .filter(|l| !l.to_string().trim().is_empty())
                        .count(),
                    c.fields().len() + headings
                );
                assert!(lines.iter().all(|l| l.width() <= width as usize));
                assert!(!lines.iter().any(|l| l.to_string().contains(c.field().long)));
                assert!(c.line().to_string().contains(c.field().hint));
                assert_eq!(c.prompt_rows(width), 4);
            }
        }
        c.go(field_at("model"));
        let (lines, at) = c.lines(48);
        assert!(lines[at].to_string().contains("model"));
        assert!(!lines[at].to_string().contains("sonnet"));
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        let (lines, _) = c.lines(48);
        for name in ["opus", "sonnet[1m]", "haiku", "type a custom value"] {
            assert!(lines.iter().any(|l| l.to_string().contains(name)), "{name}");
        }
    }

    #[test]
    fn config_choice_browsing_and_custom_editing_are_reversible() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let none = KeyModifiers::NONE;
        c.go(field_at("model"));
        c.key(KeyCode::Enter, none);
        c.key(KeyCode::Down, none);
        assert!(c.values[c.row].is_empty());
        c.key(KeyCode::Esc, none);
        assert!(c.values[c.row].is_empty() && c.choice.is_none());
        c.key(KeyCode::Enter, none);
        c.key(KeyCode::Down, none);
        assert!(matches!(
            c.key(KeyCode::Enter, none),
            ConfigAction::Save(..)
        ));
        assert_eq!(c.values[c.row], "fable");
        c.key(KeyCode::Enter, none);
        c.key(KeyCode::End, none);
        c.key(KeyCode::Enter, none);
        assert!(c.open && c.values[c.row].is_empty());
        c.key(KeyCode::Char('x'), none);
        c.key(KeyCode::Esc, none);
        assert_eq!(c.values[c.row], "fable");
        let custom = "provider/long-custom-model-模型-with-a-visible-cursor";
        c.values[c.row] = custom.into();
        c.key(KeyCode::Enter, none);
        assert_eq!(c.choice, Some(c.choices().len() - 1));
        c.key(KeyCode::Enter, none);
        let (lines, at) = c.lines(40);
        assert!(
            lines[at]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
        );
        assert!(lines[at].width() <= 40);
        assert_eq!(c.values[c.row], custom);
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
            None,
            None,
            None,
        );
        c.go(field_at("model"));
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
    fn config_display_labels_preserve_values_and_help_preserves_edits() {
        let d = dir();
        let mut app = app(d.path());
        let mut form = app.config_form();
        form.go(field_at("codex_full_access"));
        app.mode = Mode::Config(form);
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert_eq!(
            config::defaults(&app.jobs_path).codex_full_access,
            Some(true)
        );
        let Mode::Config(form) = &app.mode else {
            unreachable!()
        };
        let control = Line::from(form.control(form.row, 40)).to_string();
        assert!(
            control.contains("on") && control.ends_with('*'),
            "{control}"
        );
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert_eq!(config::defaults(&app.jobs_path).codex_full_access, None);
        let Mode::Config(form) = &mut app.mode else {
            unreachable!()
        };
        let control = Line::from(form.control(form.row, 40)).to_string();
        assert!(
            control.contains("off") && !control.contains('*'),
            "{control}"
        );
        form.go(field_at("model"));
        form.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(form.open);
        let before = form.values.clone();
        let cursor = form.cursor;
        form.key(KeyCode::F(1), KeyModifiers::NONE);
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(24, 7)).unwrap();
        t.draw(|f| form.draw(f, f.area())).unwrap();
        form.key(KeyCode::End, KeyModifiers::NONE);
        assert!(form.help.is_some_and(|top| top > 0));
        form.key(KeyCode::Backspace, KeyModifiers::NONE);
        form.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(form.open && form.help.is_none());
        assert_eq!((form.values.clone(), form.cursor), (before, cursor));
        form.key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(form.values[form.row], "x?");
    }

    #[test]
    fn guide_search_matches_topics_and_scrolls_to_the_last_wrapped_line() {
        let matches = guide_rows("CONFIG reset");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].1, "Config");
        assert_eq!(matches[1].0, "backspace");
        assert!(guide_rows("config clipboard").is_empty());
        for width in [12, 32, 60, 120] {
            let mut guide = Guide::default();
            let mut t = Terminal::new(ratatui::backend::TestBackend::new(width, 10)).unwrap();
            t.draw(|f| guide.draw(f, f.area(), true)).unwrap();
            guide.key(KeyCode::End, KeyModifiers::NONE);
            t.draw(|f| guide.draw(f, f.area(), true)).unwrap();
            let lines = guide_lines(width, "");
            assert!(
                lines
                    .iter()
                    .all(|line| line.to_string().trim_end().chars().count() <= width as usize),
                "{width}"
            );
            assert_eq!(guide.top + guide.body_height(), lines.len(), "{width}");
            let text = rows(&t, width as usize).join("\n");
            assert!(!text.trim().is_empty(), "{width}");
            guide.key(KeyCode::PageUp, KeyModifiers::NONE);
            assert!(guide.top < guide.max_scroll());
            guide.key(KeyCode::Home, KeyModifiers::NONE);
            assert_eq!(guide.top, 0);
        }
    }

    #[test]
    fn guide_search_accepts_paste_and_handles_empty_results() {
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
        app.mode = Mode::Guide(Guide::default());
        app.paste("config\r\nreset");
        let Mode::Guide(guide) = &app.mode else {
            unreachable!()
        };
        assert_eq!(guide.find.text, "config reset");
        assert_eq!(guide_rows(&guide.find.text).len(), 2);
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        app.paste("no-such-設定");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let text = rows(&t, 60).join("\n");
        assert!(
            text.contains("0 matches") && text.contains("No shortcuts match"),
            "{text}"
        );
        app.key(KeyCode::Char('u'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(&app.mode, Mode::Guide(g) if g.find.text.is_empty() && g.top == 0));
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 6,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(&app.mode, Mode::Guide(g) if g.top > 0));
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
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 0, .. })));
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
            text.contains("Viewers") && text.contains("Type to search"),
            "{text}"
        );
        assert!(
            text.contains("↑↓ scroll · pgup/dn page · esc back"),
            "{text}"
        );
        assert!(!app.key(KeyCode::Down, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 1, .. })));
        assert!(!app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn guide_search_keeps_cursor_editing_and_escape_clears_before_leaving() {
        let d = dir();
        let mut app = app(d.path());
        app.split = false;
        assert!(!app.key(KeyCode::Char('g'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.key(KeyCode::Down, KeyModifiers::NONE).unwrap());
        for c in "clipboard".chars() {
            assert!(!app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap());
        }
        assert!(
            matches!(app.mode, Mode::Guide(Guide { top: 0, .. })),
            "typing rewinds it"
        );
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let text = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("clipboard"), "{text}");
        assert!(!text.contains("Move between rows"), "no other key: {text}");
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
        assert!(matches!(&app.mode, Mode::Guide(g) if g.find.at == "clipboard".len() - 1));
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Guide(g) if g.find.text == "clipboard"));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Guide(g) if g.find.text.is_empty()));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn folder_recall_follows_the_recent_list_on_screen() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.data.recent = ["/src/new", "/src/mid", "/src/old"]
            .iter()
            .map(PathBuf::from)
            .collect();
        app.mode = Mode::Folder(Input::default());
        for (code, want) in [
            (KeyCode::Down, "/src/new"),
            (KeyCode::Down, "/src/mid"),
            (KeyCode::Down, "/src/old"),
            (KeyCode::Down, "/src/new"),
            (KeyCode::Up, "/src/old"),
            (KeyCode::Up, "/src/mid"),
            (KeyCode::Up, "/src/new"),
        ] {
            app.key(code, KeyModifiers::NONE).unwrap();
            let held = match &app.mode {
                Mode::Folder(input) => input.text.clone(),
                _ => String::new(),
            };
            assert_eq!(held, want, "{code:?} from the row above it");
        }
    }

    #[test]
    fn left_leaves_a_screen_or_prompt_with_nothing_to_its_left() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        app.show_jobs();
        assert!(!app.on_button(), "the cursor is on a job row");
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
        assert!(!app.jobs_view, "the jobs screen returns to the list");
        app.show_jobs();
        app.text = "x".into();
        app.caret = 1;
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        assert!(
            app.jobs_view && app.caret == 0,
            "typed text keeps the arrow"
        );
        app.text.clear();
        app.leave_jobs();
        app.mode = Mode::Folder(Input::default());
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Normal));
        app.mode = Mode::Folder(Input::new("/src"));
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Folder(t) if t.at == 3),
            "a typed path keeps the arrow"
        );
        app.mode = Mode::Filter;
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
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
        app.log = Some(Diagnostics::new(path.clone(), false));
        app.debug(|| "one".into());
        app.debug(|| "two".into());
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let one: Value = serde_json::from_str(lines[0]).unwrap();
        let two: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(one["data"]["message"], "one");
        assert_eq!(two["data"]["message"], "two");
        assert_eq!(one["dashboard_id"], two["dashboard_id"]);
        assert!(term_state().contains("pgrp="));
    }

    fn diagnostic_records(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn debug_keeps_shortcuts_and_paste_shape_while_trace_keeps_input_text() {
        use ratatui::crossterm::event::KeyEvent;
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut app = app(d.path());
        app.log = Some(Diagnostics::new(path.clone(), false));
        app.log_input(&Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert!(!path.exists(), "ordinary typing is trace-only");
        app.log_input(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )));
        app.log_input(&Event::Paste("private paste".into()));
        let rows = diagnostic_records(&path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["event"], "input.key");
        assert_eq!(rows[0]["data"]["key"], "Enter");
        assert!(
            rows[0]["data"]["modifiers"]
                .as_str()
                .unwrap()
                .contains("ALT")
        );
        assert!(rows[0]["data"]["operation_id"].is_string());
        assert_eq!(rows[0]["data"]["route"]["mode"], "normal");
        assert_eq!(rows[1]["data"]["bytes"], 13);
        assert!(rows[1]["data"].get("text").is_none());
        app.log.as_mut().unwrap().trace = true;
        app.log_input(&Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        app.log_input(&Event::Paste("private paste".into()));
        let rows = diagnostic_records(&path);
        assert_eq!(rows[2]["level"], "trace");
        assert_eq!(rows[3]["data"]["text"], "private paste");
    }

    #[test]
    fn routine_timings_are_summarized_and_slow_samples_keep_their_context() {
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut app = app(d.path());
        app.log = Some(Diagnostics::new(path.clone(), false));
        for _ in 0..100 {
            app.measured("draw", 1.0, || panic!("formatted a suppressed sample"));
        }
        assert!(!path.exists());
        app.summarize_timings(false);
        assert!(!path.exists(), "the interval has not elapsed");
        app.summarize_timings(true);
        let rows = diagnostic_records(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "timing.summary");
        assert_eq!(rows[0]["data"]["phases"]["draw"]["count"], 100);
        assert_eq!(rows[0]["data"]["phases"]["draw"]["mean_ms"], 1.0);
        app.measured("draw", 20.0, || json!({"operation_id": "slow-operation"}));
        let rows = diagnostic_records(&path);
        assert_eq!(rows[1]["data"]["operation_id"], "slow-operation");
        assert_eq!(rows[1]["data"]["slow"], true);
        app.log.as_mut().unwrap().trace = true;
        app.measured("draw", 1.0, || json!({}));
        assert_eq!(diagnostic_records(&path)[2]["level"], "trace");
    }

    #[test]
    fn launch_recovery_keeps_one_submitted_prompt_with_debug_off() {
        let d = dir();
        let mut app = app(d.path());
        let prompt = "first line\nsecond line 🚦";
        let id = app.launch_row(HarnessKind::Claude, d.path(), prompt);
        let rows = diagnostic_records(&d.path().join("launches.jsonl"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "launch.submitted");
        assert_eq!(rows[0]["data"]["operation_id"], id);
        assert_eq!(rows[0]["data"]["prompt"], prompt);
        assert_eq!(rows[0]["data"]["harness"], "claude");
        assert!(!d.path().join("tui-debug.log").exists());
    }

    #[test]
    fn recovery_does_not_break_a_launch_from_a_non_utf8_folder() {
        use std::os::unix::ffi::OsStringExt;
        let d = dir();
        let folder = d
            .path()
            .join(std::ffi::OsString::from_vec(b"folder-\xff".to_vec()));
        let mut app = app(d.path());
        let id = app.launch_row(HarnessKind::Claude, &folder, "recover me");
        let rows = diagnostic_records(&d.path().join("launches.jsonl"));
        assert_eq!(rows[0]["data"]["operation_id"], id);
        assert_eq!(rows[0]["data"]["cwd"], folder.to_string_lossy().as_ref());
        assert_eq!(
            app.pending[0].session.cwd, folder,
            "diagnostics do not change the native path"
        );
    }

    #[test]
    fn row_diagnostics_report_native_source_state_changes_and_suppression() {
        let d = dir();
        registry(d.path(), A, "/fixture", "idle", 1);
        let path = d.path().join("diagnostics.log");
        let mut app = App::new_logged(
            Path::new("cones"),
            &d.path().join("none.yaml"),
            d.path(),
            d.path(),
            Some(Diagnostics::new(path.clone(), false)),
        )
        .unwrap();
        app.rebuild();
        let rows = diagnostic_records(&path);
        assert!(rows.iter().any(|r| r["event"] == "row.added"
            && r["data"]["after"]["session_id"] == A
            && r["data"]["after"]["source"]["reader"] == "registry"));
        let before = rows.len();
        app.rebuild();
        assert_eq!(
            diagnostic_records(&path).len(),
            before,
            "unchanged rows do not repeat"
        );
        registry(d.path(), A, "/fixture", "busy", 1);
        let data = Data::load_observed(
            &app.jobs_path,
            &app.state,
            &app.claude,
            app.log.as_ref(),
            None,
        )
        .unwrap();
        app.apply(data);
        let rows = diagnostic_records(&path);
        assert!(rows.iter().any(|r| r["event"] == "row.changed"
            && r["data"]["before"]["state"] == "idle"
            && r["data"]["after"]["state"] == "active"));
        fs::remove_file(d.path().join("sessions").join(format!("{A}.json"))).unwrap();
        let mut data = Data::load_observed(
            &app.jobs_path,
            &app.state,
            &app.claude,
            app.log.as_ref(),
            None,
        )
        .unwrap();
        data.diagnostics
            .as_mut()
            .unwrap()
            .excluded
            .insert(A.into(), "hidden");
        app.apply(data);
        assert!(
            diagnostic_records(&path)
                .iter()
                .any(|r| r["event"] == "row.removed"
                    && r["data"]["before"]["session_id"] == A
                    && r["data"]["reason"] == "hidden")
        );
    }

    #[test]
    fn refresh_failure_is_logged_before_a_status_line_can_replace_it() {
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut app = app(d.path());
        app.log = Some(Diagnostics::new(path.clone(), false));
        let operation = DiagnosticOperation::new();
        let id = operation.id.clone();
        app.loading_operation = Some(operation);
        let (tx, rx) = mpsc::channel();
        app.loading = Some(rx);
        tx.send(Err(anyhow::anyhow!("unreadable registry")))
            .unwrap();
        app.poll();
        app.status = "another key".into();
        let rows = diagnostic_records(&path);
        assert!(rows.iter().any(|r| r["event"] == "refresh.failed"
            && r["data"]["operation_id"] == id
            && r["data"]["error"] == "unreadable registry"
            && r["data"]["retained_previous_rows"] == true));
    }

    #[test]
    fn action_results_keep_the_same_operation_and_actual_failure() {
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut app = app(d.path());
        app.log = Some(Diagnostics::new(path.clone(), false));
        app.queue_stop(A.into(), "delete", || anyhow::bail!("native refusal"));
        poll_until(&mut app, |a| a.stopping.is_empty());
        let rows = diagnostic_records(&path);
        let start = rows
            .iter()
            .find(|r| r["event"] == "action.started")
            .unwrap();
        let done = rows
            .iter()
            .find(|r| r["event"] == "action.completed")
            .unwrap();
        assert_eq!(start["data"]["operation_id"], done["data"]["operation_id"]);
        assert_eq!(done["data"]["outcome"], "failed");
        assert_eq!(done["data"]["error"], "native refusal");
        assert_eq!(done["data"]["row_id"], A);
    }

    #[test]
    fn executable_diagnostics_identify_the_binary_bytes() {
        let d = dir();
        let path = d.path().join("binary");
        fs::write(&path, b"first build").unwrap();
        let first = executable_identity(&path);
        fs::write(&path, b"second build").unwrap();
        let second = executable_identity(&path);
        assert_eq!(first["executable"], path.to_str().unwrap());
        assert_eq!(first["sha256"].as_str().unwrap().len(), 64);
        assert_ne!(first["sha256"], second["sha256"]);
        assert_eq!(first["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn diagnostic_records_stay_whole_across_threads() {
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let path = &path;
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for sequence in 0..250 {
                        debug_line(path, json!({"event":"fixture","data":{"worker":worker,"sequence":sequence,"payload":"x".repeat(256)}})).unwrap();
                    }
                });
            }
        });
        let rows = diagnostic_records(&path);
        assert_eq!(rows.len(), 2000);
        let identities: HashSet<_> = rows
            .iter()
            .map(|r| {
                (
                    r["data"]["worker"].as_u64().unwrap(),
                    r["data"]["sequence"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(identities.len(), 2000);
    }

    #[test]
    fn diagnostic_compaction_is_bounded_across_processes() {
        const CAP: u64 = 10 * 1024 * 1024;
        if let Some(path) = std::env::var_os("CONES_DIAGNOSTIC_TEST_LOG") {
            let worker: u64 = std::env::var("CONES_DIAGNOSTIC_TEST_WORKER")
                .unwrap()
                .parse()
                .unwrap();
            for sequence in 0..600 {
                debug_line(Path::new(&path), json!({
                    "event":"fixture","data":{"worker":worker,"sequence":sequence,"payload":"x".repeat(4096)},
                })).unwrap();
            }
            return;
        }
        struct Writer(std::process::Child);
        impl Drop for Writer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut writers: Vec<_> = (0..6)
            .map(|worker| {
                Writer(
                    Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "tui::tests::diagnostic_compaction_is_bounded_across_processes",
                        ])
                        .env("CONES_DIAGNOSTIC_TEST_LOG", &path)
                        .env("CONES_DIAGNOSTIC_TEST_WORKER", worker.to_string())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap(),
                )
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "diagnostic writer stalled");
            if let Ok(metadata) = fs::metadata(&path) {
                assert!(metadata.len() <= CAP);
            }
            let mut done = true;
            for writer in &mut writers {
                match writer.0.try_wait().unwrap() {
                    Some(status) => assert!(status.success()),
                    None => done = false,
                }
            }
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let rows = diagnostic_records(&path);
        assert!(!rows.is_empty());
        assert!(fs::metadata(&path).unwrap().len() <= CAP);
        let ids: HashSet<_> = rows
            .iter()
            .map(|r| {
                (
                    r["data"]["worker"].as_u64().unwrap(),
                    r["data"]["sequence"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(ids.len(), rows.len());
    }

    #[test]
    fn oversized_logs_keep_recent_lines_and_oversized_records_remain_json() {
        use std::io::{Seek, SeekFrom, Write};
        let d = dir();
        let path = d.path().join("diagnostics.log");
        let mut file = fs::File::create(&path).unwrap();
        file.set_len(72 * 1024 * 1024).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        writeln!(file, "\n{}", json!({"event":"recent"})).unwrap();
        drop(file);
        debug_line(&path, json!({"event":"next","data":{}})).unwrap();
        let rows = diagnostic_records(&path);
        assert_eq!(rows[0]["event"], "recent");
        assert_eq!(rows[1]["event"], "next");
        assert!(fs::metadata(&path).unwrap().len() <= 10 * 1024 * 1024);
        let large = d.path().join("large.log");
        debug_line(
            &large,
            json!({"event":"large","data":{"operation_id":"large-operation","harness":"claude","prompt":"🚦".repeat(3 * 1024 * 1024)}}),
        )
        .unwrap();
        let rows = diagnostic_records(&large);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "large");
        assert_eq!(rows[0]["data"]["truncated"], true);
        assert_eq!(rows[0]["data"]["operation_id"], "large-operation");
        assert!(fs::metadata(&large).unwrap().len() <= 10 * 1024 * 1024);
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
            cost_info: None,
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
            cost_info: None,
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
            cost_info: None,
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
        assert_eq!(row(&data, "aaaa-worker").cells[1].0.trim(), "✻  claude");
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
            cost_info: None,
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
            cost_info: None,
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
        let ids: Vec<String> = fleet_rows(&claude, &state, &[], &config::Policy::default())
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, ["dddd", A], "the older thread comes first");
        // Forgetting drops the record, and the row goes with it; a thread the daemon still
        // holds keeps its row, because a live thread is worth seeing.
        codex::forget(&state, "dddd").unwrap();
        let ids: Vec<String> = fleet_rows(&claude, &state, &[], &config::Policy::default())
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, [A], "a forgotten thread has no row");
    }

    #[test]
    fn a_harness_config_does_not_offer_is_left_out_of_discovery() {
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
        let ids = |offered: &config::Policy| -> Vec<String> {
            fleet_rows(&claude, &state, &[], offered)
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .collect()
        };
        assert_eq!(ids(&config::Policy::default()), ["dddd", A]);
        let off = |policy: config::Policy| ids(&policy);
        assert_eq!(
            off(config::Policy {
                claude_enabled: Some(false),
                ..Default::default()
            }),
            ["dddd"],
            "the registry of a harness config does not offer is never read"
        );
        assert_eq!(
            off(config::Policy {
                codex_enabled: Some(false),
                ..Default::default()
            }),
            [A],
            "neither are its saved threads"
        );
        assert!(
            off(config::Policy {
                claude_enabled: Some(false),
                codex_enabled: Some(false),
                ..Default::default()
            })
            .is_empty()
        );
    }

    /// Forgetting drops the saved record, but the daemon keeps the thread's writer lock for minutes
    /// after the client goes, and that lock is a second source of rows. Hiding the id is what makes
    /// the removal outlive the process; recording a turn on the thread again undoes it.
    #[test]
    fn forgetting_a_thread_the_daemon_still_holds_survives_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let (claude, state) = (d.path().join(".claude"), d.path().join("state"));
        let (cwd, codex) = (d.path().to_str().unwrap(), d.path().join(".codex"));
        registry(&claude, A, cwd, "idle", 1_789_000_000_000);
        let sessions = codex.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join("rollout-dddd.jsonl");
        fs::write(
            &rollout,
            format!(
                r#"{{"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{{"id":"dddd","timestamp":"2026-09-01T00:00:00Z","cwd":{}}}}}"#,
                serde_json::to_string(cwd).unwrap()
            ) + "\n",
        )
        .unwrap();
        // Stand in for the daemon: this process holds the lock and its pid is the recorded one.
        let daemon = codex.join("app-server-daemon");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(
            daemon.join("app-server.pid"),
            serde_json::json!({"pid": std::process::id()}).to_string(),
        )
        .unwrap();
        fs::create_dir_all(codex.join("thread-writer-locks")).unwrap();
        let held = fs::File::create(codex.join("thread-writer-locks").join("dddd.lock")).unwrap();
        // A Codex home that exists lifts the `is_dir` short circuit in native discovery, so this
        // machine's own Codex clients reach the list. Keep only the ids the fixture owns.
        let ids = || -> Vec<String> {
            fleet_rows(&claude, &state, &[], &config::Policy::default())
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .filter(|id| id == "dddd" || id == A)
                .collect()
        };
        assert_eq!(ids(), ["dddd", A], "the held thread has a row");
        codex::forget(&state, "dddd").unwrap();
        assert_eq!(
            ids(),
            ["dddd", A],
            "dropping the record alone leaves the lock's row"
        );
        Ledger::new(&state).unwrap().hide("dddd").unwrap();
        assert_eq!(ids(), [A], "a forgotten thread stays gone on a restart");
        // Reviving it from history clears the hidden id, so the lock's row returns at once.
        Ledger::new(&state).unwrap().unhide("dddd").unwrap();
        assert_eq!(ids(), ["dddd", A], "a revived thread is in the list again");
        Ledger::new(&state).unwrap().hide("dddd").unwrap();
        assert_eq!(ids(), [A]);
        // A resume records the thread after its first turn, which brings the row back.
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
        assert_eq!(ids(), ["dddd", A], "recording it again un-forgets the row");
        drop(held);
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

    /// A pending delete already hides the row, so its pinned folder must take the same
    /// frame, complete: waiting for the next read made the row blink out, come back empty,
    /// then gain its git state a second later.
    #[test]
    fn deleting_the_last_session_leaves_its_pinned_folder_in_the_same_frame() {
        let d = dir();
        let claude = d.path();
        let work = claude.join("work");
        fs::create_dir(&work).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        registry(claude, A, work.to_str().unwrap(), "idle", 1_757_682_871_000);
        let mut app = app(claude);
        app.refresh().unwrap();
        app.pin_folder(work.clone()).unwrap();
        // As in use, the folder was added before the deletion, so a read has seen it.
        app.refresh().unwrap();
        poll_until(&mut app, |a| a.loading.is_none());
        let name = fleet::tilde(&work);
        let row = |app: &App| {
            app.rows
                .iter()
                .find(|r| r.kind == Kind::Folder(name.clone()))
                .map(Row::text)
        };
        assert_eq!(
            row(&app),
            None,
            "the session covers the folder while it lives"
        );
        app.queue_stop(A.into(), "delete", || Ok(true));
        let text = row(&app).unwrap_or_default();
        assert_eq!(
            text, "main · clean",
            "the folder row is there with its git state before the next read: {text}"
        );
        assert!(
            !app.rows
                .iter()
                .any(|r| matches!(&r.kind, Kind::Session(id, _) if id == A)),
            "and the deleted session is gone"
        );
        // The read that follows keeps the same row, so nothing moves a second later.
        poll_until(&mut app, |a| a.stopping.is_empty());
        poll_until(&mut app, |a| a.loading.is_none());
        assert_eq!(row(&app).unwrap_or_default(), text, "unchanged by the read");
    }

    fn history_fixture(
        count: usize,
    ) -> (
        tempfile::TempDir,
        App,
        Terminal<ratatui::backend::TestBackend>,
    ) {
        let d = dir();
        let project = d.path().join("projects/history");
        fs::create_dir_all(&project).unwrap();
        for n in 0..count {
            let id = format!("{n:08x}-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            let at = chrono::DateTime::parse_from_rfc3339("2026-09-10T12:00:00Z").unwrap()
                + chrono::Duration::seconds(n as i64);
            let records = [
                serde_json::json!({"type":"user","cwd":d.path(),"timestamp":"2026-09-09T12:00:00Z","message":{"content":format!("old session {n:03}")}}),
                serde_json::json!({"type":"assistant","timestamp":at.to_rfc3339(),"message":{"id":"m","model":"fixture-model","usage":{"input_tokens":20,"output_tokens":4},"content":[{"type":"text","text":format!("reply {n}")}]}}),
            ];
            fs::write(
                project.join(format!("{id}.jsonl")),
                records.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap();
        }
        let mut app = app(d.path());
        app.rebuild();
        app.history.reader = Some(
            history::Reader::new(vec![history::Source {
                harness: HarnessKind::Claude,
                home: d.path().to_owned(),
            }])
            .unwrap(),
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        (d, app, terminal)
    }

    fn history_until(
        app: &mut App,
        terminal: &mut Terminal<ratatui::backend::TestBackend>,
        done: impl Fn(&App) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.history_tick();
            terminal.draw(|f| app.draw(f)).unwrap();
            if done(app) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "history did not settle: {:?}",
                app.history.error
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn ctrl_h_loads_history_below_the_main_list_without_changing_live_counts() {
        let (d, mut app, mut terminal) = history_fixture(4);
        let mut live = session(A, "idle", "live session", 0);
        live.cwd = d.path().to_owned();
        app.data.sessions.push(live);
        app.rebuild();
        let before = app.data.summary(0).to_string();
        assert!(app.history.rows.is_empty());
        assert!(app.hint_line().to_string().contains("ctrl+h"));
        app.key(KeyCode::Char('h'), KeyModifiers::CONTROL).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.rows.iter().any(|r| r.entry.columns.is_some())
        });
        assert_eq!(app.history.rows.len(), 4);
        assert_eq!(app.data.summary(0).to_string(), before);
        assert_eq!(app.data.sessions.len(), 1);
        assert!(app.viewers.is_empty());
        assert!(app.opening.is_none());
        assert_eq!(app.selected_cwd().as_deref(), Some(d.path()));
        let live_at = app
            .rows
            .iter()
            .position(|r| matches!(&r.kind, Kind::Session(id, _) if id == A))
            .unwrap();
        let history_at = app
            .rows
            .iter()
            .position(|r| matches!(r.kind, Kind::History(_)))
            .unwrap();
        assert!(history_at > live_at);
        assert_eq!(
            app.history.rows[0].entry.title.as_deref(),
            Some("old session 003")
        );
        assert_eq!(app.enter_label(), "resume");
        let historical = &app.rows[history_at];
        assert!(historical.text().contains("fixture-model"));
        assert!(!historical.working());
        assert_eq!(historical.cells[2].0.trim(), "old session 003");
        assert!(historical.cells.iter().any(|c| c.0.trim() == "20"));
        app.key(KeyCode::Char('h'), KeyModifiers::CONTROL).unwrap();
        assert!(!app.rows.iter().any(|r| matches!(r.kind, Kind::History(_))));
        assert_eq!(app.data.summary(0).to_string(), before);
    }

    #[test]
    fn history_prefetches_older_pages_and_does_not_wrap_while_scrolling_down() {
        let (_d, mut app, mut terminal) = history_fixture(120);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.history.rows.len(), HISTORY_PAGE);
        assert!(
            app.history
                .rows
                .iter()
                .filter(|r| r.entry.columns.is_some())
                .count()
                < HISTORY_PAGE
        );
        for expected in [100, 120] {
            let last = app
                .visible
                .iter()
                .rposition(|&i| matches!(app.rows[i].kind, Kind::History(_)))
                .unwrap();
            app.cursor = last;
            for _ in 0..5 {
                app.step(1);
            }
            assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));
            terminal.draw(|f| app.draw(f)).unwrap();
            history_until(&mut app, &mut terminal, |a| {
                a.history.rows.len() >= expected && a.history.fetch.is_none()
            });
        }
        assert!(app.history.next.is_none());
        assert_eq!(
            app.history.rows.last().unwrap().entry.title.as_deref(),
            Some("old session 000")
        );
        let keys: HashSet<_> = app.history.rows.iter().map(|r| &r.key).collect();
        assert_eq!(keys.len(), 120);
        assert!(
            app.history
                .rows
                .windows(2)
                .all(|r| r[0].entry.last_activity >= r[1].entry.last_activity)
        );
    }

    #[test]
    fn history_filter_searches_unloaded_pages_and_discards_an_obsolete_request() {
        let (_d, mut app, mut terminal) = history_fixture(120);
        app.toggle_history();
        app.history_tick();
        assert!(app.history.fetch.is_some());
        app.key(KeyCode::Char('f'), KeyModifiers::CONTROL).unwrap();
        for c in "old session 005".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.history.rows.len(), 1);
        assert_eq!(
            app.history.rows[0].entry.title.as_deref(),
            Some("old session 005")
        );
        assert!(
            app.rows
                .iter()
                .filter(|r| matches!(r.kind, Kind::History(_)))
                .all(|r| r.text().contains("old session 005"))
        );
        app.key(KeyCode::Char('f'), KeyModifiers::CONTROL).unwrap();
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.rows.len() == HISTORY_PAGE
        });
    }

    #[test]
    fn history_search_shows_an_excerpt_and_previews_the_match_without_resuming() {
        let (_d, mut app, mut terminal) = history_fixture(2);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let path = app.history.rows[0].entry.transcript.clone();
        let mut records = fs::read_to_string(&path).unwrap();
        for i in 0..90 {
            let text = if i == 10 {
                "The retry backoff discussion".to_owned()
            } else {
                format!("Other message {i}")
            };
            records.push_str(&format!(
                "{}\n",
                json!({"type":"user","message":{"content":text}})
            ));
        }
        fs::write(&path, &records).unwrap();
        app.key(KeyCode::Char('f'), KeyModifiers::CONTROL).unwrap();
        for c in "retry backoff".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.history.rows.len(), 1);
        assert!(matches!(
            app.selected().map(|r| &r.kind),
            Some(Kind::History(_))
        ));
        let excerpt = app
            .rows
            .iter()
            .find(|r| r.kind == Kind::HistoryStatus && r.text().contains("retry"))
            .unwrap();
        assert!(
            excerpt
                .cells
                .iter()
                .any(|(text, style)| text.contains("retry") && style.fg == Some(Color::Yellow))
        );
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        let text = pane_text(&app, &terminal);
        assert!(text.contains("retry backoff"), "{text}");
        assert!(!text.contains("Other message 89"), "{text}");
        assert!(app.viewers.is_empty() && app.opening.is_none());
        assert_eq!(fs::read_to_string(path).unwrap(), records);
        assert_eq!(app.enter_label(), "resume");
    }

    #[test]
    fn history_typing_searches_while_loading_and_recovers_from_no_matches() {
        let (_d, mut app, mut terminal) = history_fixture(120);
        app.toggle_history();
        app.history_tick();
        assert!(app.history.fetch.is_some());
        for c in "old session 005".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        assert_eq!(app.filter.text, "old session 005");
        assert!(app.text.is_empty());
        assert!(matches!(app.mode, Mode::Normal));
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.history.rows.len(), 1);
        assert_eq!(
            app.history.rows[0].entry.title.as_deref(),
            Some("old session 005")
        );
        assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));

        app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert!(app.history.rows.is_empty());
        assert!(app.mode_line().to_string().contains("old session 005x"));
        assert!(app.panel().is_none());
        app.viewers.push(viewer_open(A, "attach", "OTHER SESSION"));
        assert_eq!(app.shown(), None);
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(app.opening.is_none() && app.pending.is_empty());
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.filter.text, "old session 005");
        assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));
        assert!(app.text.is_empty());

        assert!(!app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert!(app.filter.text.is_empty());
        assert_eq!(app.history.rows.len(), HISTORY_PAGE);
        assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));
    }

    #[test]
    fn history_search_edits_and_resumes_without_using_either_composer_draft() {
        let (_d, mut app, mut terminal) = history_fixture(4);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        app.fill("keep this instruction".into());
        app.terminal_input = Input::new("keep this command");
        app.harness = harness::launchable().len();
        app.paste("old\r\nsession 00");
        app.key(KeyCode::Home, KeyModifiers::NONE).unwrap();
        app.paste("é ");
        app.key(KeyCode::Char('w'), KeyModifiers::CONTROL).unwrap();
        assert_eq!(app.filter.text, "old session 00");
        app.paste("");
        assert!(app.images.is_empty());
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));
        assert!(app.mode_line().to_string().starts_with("history / "));

        let first = key(&app);
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        let second = key(&app).unwrap();
        assert_ne!(first.as_deref(), Some(second.as_str()));
        assert_eq!(app.filter.text, "old session 00");
        app.viewers.push(viewer_open(&second, "attach", "HISTORY"));
        wait_paint(&mut app, 0, "HISTORY");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0));
        assert_eq!(app.viewers.len(), 1);
        assert!(app.opening.is_none() && app.pending.is_empty());
        assert_eq!(app.text, "keep this instruction");
        assert_eq!(app.terminal_input.text, "keep this command");
        app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(app.filter.text.is_empty());
        assert_eq!(app.text, "keep this instruction");
        assert_eq!(app.terminal_input.text, "keep this command");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.history.visible);
        assert_eq!(app.text, "keep this instruction");
        assert_eq!(app.terminal_input.text, "keep this command");
    }

    #[test]
    fn live_rows_keep_the_composer_while_history_is_visible() {
        let (d, mut app, mut terminal) = history_fixture(2);
        let mut live = session(A, "idle", "live session", 0);
        live.cwd = d.path().to_owned();
        app.data.sessions = vec![live];
        app.rebuild();
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        app.paste(" draft");
        assert_eq!(app.text, "x draft");
        assert!(app.filter.text.is_empty());
        assert!(!app.mode_line().to_string().starts_with("history / "));

        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char('o'), KeyModifiers::NONE).unwrap();
        assert_eq!(app.filter.text, "o");
        assert_eq!(app.text, "x draft");
        app.key(KeyCode::Char('h'), KeyModifiers::CONTROL).unwrap();
        assert!(!app.history.visible);
        app.key(KeyCode::Char('y'), KeyModifiers::NONE).unwrap();
        assert_eq!(app.text, "x drafty");
        assert_eq!(app.filter.text, "o");
    }

    #[test]
    fn hiding_history_discards_an_inflight_page_and_a_live_arrival_does_not_steal_history_selection()
     {
        let (d, mut app, mut terminal) = history_fixture(4);
        app.toggle_history();
        app.history_tick();
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.fetch.is_none());
        assert!(!app.history.visible);
        assert!(app.history.rows.is_empty());
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        let selected = key(&app);
        let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        let mut live = session(A, "active", "new live arrival", 0);
        live.cwd = d.path().to_owned();
        data.sessions.push(live);
        app.apply(data);
        assert_eq!(key(&app), selected);
        let launch = app.launch_row(HarnessKind::Claude, d.path(), "new instruction");
        assert_eq!(
            key(&app).as_deref(),
            Some(launch.as_str()),
            "an explicit launch still takes selection"
        );
    }

    #[test]
    fn a_history_selection_clears_another_viewer_and_never_speculatively_resumes() {
        let (_d, mut app, mut terminal) = history_fixture(2);
        app.viewers.push(viewer_open(A, "attach", "OTHER VIEWER"));
        wait_paint(&mut app, 0, "OTHER VIEWER");
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && matches!(a.selected().map(|r| &r.kind), Some(Kind::History(_)))
        });
        assert_eq!(app.shown(), None);
        let selected = key(&app).unwrap();
        rested(&mut app, &selected, Duration::from_secs(2));
        assert_eq!(app.prespawn_target(), None);
        assert_eq!(app.viewers.len(), 1);
        assert!(
            !rows(&terminal, 160)
                .iter()
                .any(|l| l.contains("OTHER VIEWER"))
        );
    }

    #[test]
    fn history_live_exclusions_follow_native_homes_and_known_ledger_sessions() {
        let (_d, mut app, mut terminal) = history_fixture(4);
        let id = "00000003-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut live = session(id, "idle", "already live", 0);
        live.cwd = app.claude.clone();
        app.data.sessions.push(live);
        let mut record = crate::ledger::Record::new("run".into(), crate::ledger::Status::Ok);
        record.harness = Some(HarnessKind::Claude);
        record.session_id = Some("00000002-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
        app.data.runs.push(Run {
            started: record,
            terminal: None,
        });
        app.rebuild();
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        assert_eq!(app.history.rows.len(), 2);
        assert!(
            app.history
                .rows
                .iter()
                .all(|r| r.entry.key.session_id != id)
        );
    }

    /// Forgetting a row hides its id for good, so a revived session would keep a viewer and no
    /// row. Only a deliberate revive clears it: a peek never reaches this path.
    #[test]
    fn reviving_a_forgotten_session_clears_its_hidden_id() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let id = app.history.rows[0].entry.key.session_id.clone();
        let ledger = Ledger::new(&app.state).unwrap();
        ledger.hide(&id).unwrap();
        app.cursor = app
            .visible
            .iter()
            .position(|&i| matches!(app.rows[i].kind, Kind::History(_)))
            .unwrap();
        app.enter().unwrap();
        assert!(app.opening.is_some(), "the revive is under way");
        assert!(!ledger.hidden().unwrap().contains(&id));
        assert!(app.cancel_opening());
    }

    #[test]
    fn resumed_history_viewers_follow_every_harness_into_live_rows() {
        for harness in ["claude", "codex", "pi", "opencode"] {
            let (d, mut app, mut terminal) = history_fixture(1);
            app.toggle_history();
            history_until(&mut app, &mut terminal, |a| a.history.ready);
            let mut entry = app.history.rows[0].entry.clone();
            entry.key.harness = harness.into();
            entry.key.home = match harness {
                "codex" => codex::home(&app.claude),
                "pi" => crate::pi::home(&app.claude),
                "opencode" => crate::opencode::home(&app.claude),
                _ => entry.key.home.clone(),
            };
            entry.transcript = entry.key.home.join("sessions/2026/09/10/rollout.jsonl");
            let viewer_key = history_key(&entry.key);
            app.history.rows = vec![HistoryRow {
                key: viewer_key.clone(),
                entry: entry.clone(),
            }];
            app.history.opened.insert(viewer_key.clone(), entry.clone());
            app.viewers
                .push(viewer_open(&viewer_key, harness, "RESUMED"));
            let pid = app.viewers[0].viewer.pid();
            app.rebuild();
            app.cursor = app
                .visible
                .iter()
                .position(|&i| matches!(app.rows[i].kind, Kind::History(_)))
                .unwrap();
            app.focus = Some(0);
            let mut native = history_session(&entry);
            native.cwd = d.path().to_owned();
            native.pid = Some(pid);
            native.state = "idle".into();
            native.kind = match harness {
                "claude" => Some("bg".into()),
                "codex" => Some("daemon".into()),
                _ => None,
            };
            if harness == "pi" {
                native.session_id = format!("pi-{pid}");
                native.title = None;
            }
            let first_id = native.session_id.clone();
            let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
            data.sessions.push(native);
            app.apply(data);
            assert_eq!(key(&app).as_deref(), Some(first_id.as_str()), "{harness}");
            assert_eq!(app.focus, Some(0));
            assert_eq!(app.viewer_of(&app.selected().unwrap().kind), Some(0));
            assert_eq!(app.enter_label(), "return");
            app.enter().unwrap();
            assert_eq!(app.viewers.len(), 1);
            assert_eq!(app.viewers[0].viewer.pid(), pid);
            assert!(!app.rows.iter().any(|r| matches!(r.kind, Kind::History(_))));
            if harness == "pi" {
                assert_eq!(app.data.sessions[0].title, entry.title);
                let mut reported = app.data.sessions[0].clone();
                reported.session_id = entry.key.session_id.clone();
                let mut other = session(B, "idle", "another pi", 0);
                other.harness = "pi".into();
                other.cwd = PathBuf::from("/aaa");
                let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
                data.sessions = vec![other, reported];
                app.apply(data);
                assert_eq!(key(&app).as_deref(), Some(entry.key.session_id.as_str()));
                assert_eq!(app.viewer_of(&app.selected().unwrap().kind), Some(0));
                assert_eq!(app.viewers.len(), 1);
            }
            app.focus = None;
            app.toggle_history();
            assert_eq!(app.viewer_of(&app.selected().unwrap().kind), Some(0));
        }
    }

    #[test]
    fn history_viewers_do_not_match_the_same_thread_id_in_another_codex_home() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let mut entry = app.history.rows[0].entry.clone();
        entry.key.harness = "codex".into();
        entry.key.home = app.claude.join("one");
        let viewer_key = history_key(&entry.key);
        app.history.opened.insert(viewer_key.clone(), entry.clone());
        app.viewers.push(viewer_open(&viewer_key, "codex", "ONE"));
        let mut other = history_session(&entry);
        other.transcript_path = Some(app.claude.join("two/sessions/rollout.jsonl"));
        app.data.sessions.push(other);
        assert_eq!(
            app.viewer_of(&Kind::Session(entry.key.session_id.clone(), "-".into())),
            None
        );
    }

    #[test]
    fn history_paging_continues_when_every_loaded_row_becomes_live() {
        let (_d, mut app, mut terminal) = history_fixture(60);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        assert_eq!(app.history.rows.len(), HISTORY_PAGE);
        let live: Vec<_> = app
            .history
            .rows
            .iter()
            .map(|r| history_session(&r.entry))
            .collect();
        app.data.sessions = live;
        app.rebuild();
        assert!(!app.rows.iter().any(|r| matches!(r.kind, Kind::History(_))));
        history_until(&mut app, &mut terminal, |a| a.history.rows.len() == 60);
        assert!(app.rows.iter().any(|r| matches!(r.kind, Kind::History(_))));
    }

    #[test]
    fn history_wheel_and_page_keys_scroll_the_list_with_the_pane_closed() {
        let (_d, mut app, mut terminal) = history_fixture(40);
        app.split = false;
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        assert!(app.wants_mouse());
        let before = app.cursor;
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: app.list_area.x + 1,
            row: app.list_area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.cursor > before);
        let before = app.cursor;
        app.key(KeyCode::PageDown, KeyModifiers::NONE).unwrap();
        assert!(app.cursor > before);
        assert!(matches!(app.selected().unwrap().kind, Kind::History(_)));
    }

    #[test]
    fn history_unarchive_uses_native_argv_and_failure_prevents_resume() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        let program = d.path().join("fake codex");
        fs::write(&program, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$CAPTURE\"\nprintf 'home=%s\\n' \"$CODEX_HOME\" >> \"$CAPTURE\"\nif [ \"$1\" = unarchive ]; then exit \"$FAIL_UNARCHIVE\"; fi\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        for fail in ["0", "7"] {
            let capture = d.path().join(format!("capture{fail}"));
            let mut command = Command::new(&program);
            command
                .args(["--remote", "unix:///socket with spaces", "resume", "--", A])
                .env("CODEX_HOME", d.path())
                .env("CAPTURE", &capture)
                .env("FAIL_UNARCHIVE", fail)
                .current_dir(d.path());
            let result = harness::then_exec(
                harness::spec::args(
                    &harness::spec(HarnessKind::Codex).commands.unarchive,
                    &[("id", A.as_ref())],
                )
                .unwrap(),
                command,
            )
            .output()
            .unwrap();
            assert_eq!(result.status.success(), fail == "0");
            let captured = fs::read_to_string(capture).unwrap();
            assert!(captured.starts_with(&format!("unarchive\n--\n{A}\n")));
            assert_eq!(
                captured.contains("--remote\nunix:///socket with spaces\nresume\n--\n"),
                fail == "0"
            );
        }
    }

    #[test]
    fn history_resume_refuses_missing_files_before_preparing_a_harness() {
        let (d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let mut entry = app.history.rows[0].entry.clone();
        entry.cwd = d.path().join("gone");
        assert!(
            history_command(&entry)
                .unwrap_err()
                .to_string()
                .contains("directory")
        );
        entry.cwd = d.path().to_owned();
        fs::remove_file(&entry.transcript).unwrap();
        assert!(
            history_command(&entry)
                .unwrap_err()
                .to_string()
                .contains("transcript")
        );
    }

    #[test]
    fn history_key_belongs_to_a_focused_client_and_history_rows_offer_no_delete() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.viewers.push(viewer_open(A, "attach", "CLIENT"));
        app.focus = Some(0);
        app.key(KeyCode::Char('h'), KeyModifiers::CONTROL).unwrap();
        assert!(!app.history.visible, "a focused client keeps ctrl+h");
        app.focus = None;
        app.toggle_history();
        app.split = false;
        app.focus = Some(0);
        app.history_tick();
        assert!(
            app.history.fetch.is_none(),
            "a full-frame viewer hides the history viewport"
        );
        app.focus = None;
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let transcript = app.history.rows[0].entry.transcript.clone();
        let before = fs::read(&transcript).unwrap();
        assert_eq!(app.stop_verb(), None);
        app.key(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        app.key(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        assert!(app.stopping.is_empty());
        assert_eq!(fs::read(transcript).unwrap(), before);
    }

    #[test]
    fn history_results_preserve_the_live_fleets_stale_marker() {
        for live_stale in [false, true] {
            for history_fails in [false, true] {
                let (d, mut app, mut terminal) = history_fixture(1);
                let good = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
                if live_stale {
                    let (tx, rx) = mpsc::channel();
                    app.loading = Some(rx);
                    tx.send(Err(anyhow::anyhow!("live fixture read failed")))
                        .unwrap();
                    app.poll();
                }
                if history_fails {
                    let invalid_home = d.path().join("not-a-directory");
                    fs::write(&invalid_home, "").unwrap();
                    app.history.reader = Some(
                        history::Reader::new(vec![history::Source {
                            harness: HarnessKind::Claude,
                            home: invalid_home,
                        }])
                        .unwrap(),
                    );
                }
                app.key(KeyCode::Char('h'), KeyModifiers::CONTROL).unwrap();
                history_until(&mut app, &mut terminal, |a| {
                    if history_fails {
                        a.history.error.is_some()
                    } else {
                        a.history.ready && a.history.fetch.is_none()
                    }
                });
                assert_eq!(app.stale, live_stale);
                assert_eq!(
                    app.header_summary().to_string().contains("! stale"),
                    live_stale
                );
                assert_eq!(
                    rows(&terminal, 160).iter().any(|r| r.contains("! stale")),
                    live_stale
                );
                if live_stale {
                    let (tx, rx) = mpsc::channel();
                    app.loading = Some(rx);
                    tx.send(Ok(good)).unwrap();
                    app.poll();
                    assert!(!app.stale, "only a successful live read clears its marker");
                    assert_eq!(app.history.error.is_some(), history_fails);
                }
            }
        }
    }

    fn transcript_until(
        app: &mut App,
        terminal: &mut Terminal<ratatui::backend::TestBackend>,
        done: impl Fn(&App) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.history_tick();
            app.transcript_tick();
            terminal.draw(|f| app.draw(f)).unwrap();
            if done(app) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "preview did not settle: {:?}",
                app.transcript.error
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn pane_text(app: &App, terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
        let mut text = String::new();
        for y in app.pane.top()..app.pane.bottom() {
            for x in app.pane.left()..app.pane.right() {
                if let Some(cell) = terminal.backend().buffer().cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn transcript_selection_reads_history_without_starting_or_changing_a_session() {
        let (_d, mut app, mut terminal) = history_fixture(2);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let path = app.history.rows[0].entry.transcript.clone();
        let before = fs::read(&path).unwrap();
        let summary = app.header_summary().to_string();
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        let text = pane_text(&app, &terminal);
        assert!(text.contains("history · read only"), "{text}");
        assert!(
            text.contains("old session 001") && text.contains("reply 1"),
            "{text}"
        );
        assert!(app.viewers.is_empty() && app.opening.is_none());
        assert!(app.focus.is_none() && !app.transcript.focused);
        assert_eq!(app.enter_label(), "resume");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(app.header_summary().to_string(), summary);
    }

    #[test]
    fn transcript_never_displays_the_previous_rows_text_even_before_the_next_tick() {
        let (_d, mut app, mut terminal) = history_fixture(2);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        assert!(pane_text(&app, &terminal).contains("reply 1"));
        app.step(1);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!pane_text(&app, &terminal).contains("reply 1"));
        transcript_until(&mut app, &mut terminal, |a| {
            a.transcript
                .document
                .as_ref()
                .is_some_and(|d| d.messages.iter().any(|m| m.text == "reply 0"))
        });
        assert!(pane_text(&app, &terminal).contains("reply 0"));
        app.toggle_history();
        app.transcript_tick();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.transcript.document.is_none());
        assert!(!pane_text(&app, &terminal).contains("reply 0"));
    }

    #[test]
    fn transcript_focus_scrolls_read_only_and_enter_returns_to_an_existing_native_viewer() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let row_key = app.history.rows[0].key.clone();
        let path = app.history.rows[0].entry.transcript.clone();
        let messages: String = (0..30).map(|i| {
            format!("{}\n", serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":format!("preview line {i:02}")}]}}))
        }).collect();
        fs::write(&path, messages).unwrap();
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        assert!(pane_text(&app, &terminal).contains("preview line 29"));
        let selected = key(&app);
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert!(app.transcript.focused && app.focus.is_none());
        app.key(KeyCode::Home, KeyModifiers::NONE).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(pane_text(&app, &terminal).contains("preview line 00"));
        app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        app.paste("do not run this");
        assert!(app.text.is_empty() && app.opening.is_none());
        assert_eq!(key(&app), selected);
        app.key(KeyCode::PageDown, KeyModifiers::NONE).unwrap();
        assert!(app.transcript.scroll > 0);
        assert!(app.hint_line().to_string().contains("scroll"));
        app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.split_active() && app.transcript.focused);
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.transcript.focused);
        app.split = true;
        terminal.draw(|f| app.draw(f)).unwrap();
        app.focus_transcript();
        app.viewers.push(viewer_open(&row_key, "attach", "NATIVE"));
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(!app.transcript.focused);
        assert_eq!(app.focus, Some(0));
        assert_eq!(
            app.viewers.len(),
            1,
            "returning does not start another native client"
        );
    }

    #[test]
    fn transcript_wheel_scrolls_without_moving_the_list_selection() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let path = app.history.rows[0].entry.transcript.clone();
        fs::write(path, format!("{}\n",serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":(0..60).map(|i| format!("line {i}\n\n")).collect::<String>()}]}}))).unwrap();
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        let before = app.transcript.scroll;
        let selected = key(&app);
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: app.pane.x + 1,
            row: app.pane.y + 3,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.transcript.scroll < before);
        assert_eq!(key(&app), selected);
        assert!(!app.transcript.focused);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.pane.x + 1,
            row: app.pane.y + 3,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.transcript.focused);
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert!(!app.transcript.focused);
    }

    #[test]
    fn transcript_preview_is_not_a_fallback_for_live_rows_or_an_unpainted_viewer() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        let entry = app.history.rows[0].entry.clone();
        let row_key = app.history.rows[0].key.clone();
        app.viewers.push(silent_open(&row_key));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!pane_text(&app, &terminal).contains("reply 0"));
        assert!(!pane_text(&app, &terminal).contains("read only"));
        app.transcript_tick();
        assert!(app.transcript.document.is_none());
        app.viewers.clear();
        let mut live = history_session(&entry);
        live.kind = Some("interactive".into());
        live.state = "idle".into();
        app.data.sessions.push(live);
        app.rebuild();
        app.cursor = app
            .visible
            .iter()
            .position(|&i| matches!(app.rows[i].kind, Kind::Session(..)))
            .unwrap();
        app.transcript_tick();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.transcript_target().is_none());
        assert!(!pane_text(&app, &terminal).contains("reply 0"));
        assert!(!pane_text(&app, &terminal).contains("read only"));
    }

    #[test]
    fn transcript_failure_does_not_clear_a_stale_live_read_and_can_be_reloaded() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| {
            a.history.ready && a.history.fetch.is_none()
        });
        let path = app.history.rows[0].entry.transcript.clone();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        app.stale = true;
        transcript_until(&mut app, &mut terminal, |a| a.transcript.error.is_some());
        assert!(app.stale && app.header_summary().to_string().contains("! stale"));
        assert!(pane_text(&app, &terminal).contains("Preview unavailable"));
        fs::write(path, bytes).unwrap();
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char('r'), KeyModifiers::CONTROL).unwrap();
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        assert!(app.stale);
        assert!(pane_text(&app, &terminal).contains("reply 0"));
        assert!(app.viewers.is_empty() && app.opening.is_none());
    }

    #[test]
    fn transcript_wrapping_preserves_newlines_indent_and_unicode_graphemes() {
        let text = |s, w| {
            transcript_wrap(s, w)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(text("one two three", 7), ["one two", "three"]);
        assert_eq!(text("  let x = 1;\n\nend", 30), ["  let x = 1;", "", "end"]);
        let wrapped = transcript_wrap("中中a\u{0301}b", 4);
        assert_eq!(
            wrapped.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["中中", "a\u{0301}b"]
        );
        assert!(wrapped.iter().all(|line| line.width() <= 4));
    }

    #[test]
    fn run_peek_clears_the_previous_agent_and_refreshes_without_starting_a_viewer() {
        let (d, mut app, mut terminal) = history_fixture(0);
        let output = d.path().join("events.jsonl");
        let event = |text: &str| {
            format!(
                "{}\n",
                json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
            )
        };
        fs::write(&output, event("first run output")).unwrap();
        let mut started = crate::ledger::Record::new(A.into(), crate::ledger::Status::Started);
        started.fired_at = Some(chrono::Utc::now());
        started.output = Some(output.clone());
        started.job = Some("preview fixture".into());
        started.harness = Some(HarnessKind::Claude);
        app.data.runs.push(Run {
            started,
            terminal: None,
        });
        app.viewers.push(viewer_open(B, "attach", "OTHER AGENT"));
        app.rebuild();
        app.cursor = app
            .visible
            .iter()
            .position(|&i| matches!(app.rows[i].kind, Kind::Run(..)))
            .unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.shown().is_none());
        assert!(!pane_text(&app, &terminal).contains("OTHER AGENT"));
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        let text = pane_text(&app, &terminal);
        assert!(
            text.contains("first run output") && text.contains("output · read only"),
            "{text}"
        );
        assert_eq!(app.viewers.len(), 1);
        assert!(app.opening.is_none() && app.focus.is_none());
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert!(app.transcript.focused);
        assert!(app.hint_line().to_string().contains("follow log"));
        let output_text = (0..60).map(|i| format!("output {i}\n")).collect::<String>();
        fs::write(&output, event(&output_text)).unwrap();
        app.transcript.loaded_at = Some(Instant::now() - Duration::from_secs(2));
        transcript_until(&mut app, &mut terminal, |a| {
            a.transcript
                .document
                .as_ref()
                .is_some_and(|d| d.messages[0].text.contains("output 59"))
        });
        assert!(pane_text(&app, &terminal).contains("output 59"));
        app.key(KeyCode::PageUp, KeyModifiers::NONE).unwrap();
        let scroll = app.transcript.scroll;
        fs::write(&output, event(&(output_text + "final output"))).unwrap();
        let mut ended = crate::ledger::Record::new(A.into(), crate::ledger::Status::Failed);
        ended.reason = Some("fixture failure".into());
        app.data.runs[0].terminal = Some(ended);
        app.rebuild();
        app.transcript.loaded_at = Some(Instant::now() - Duration::from_secs(2));
        transcript_until(&mut app, &mut terminal, |a| {
            a.transcript
                .document
                .as_ref()
                .is_some_and(|d| d.messages[0].text.contains("final output"))
        });
        assert_eq!(
            app.transcript.scroll, scroll,
            "new output preserves a scrolled view"
        );
        assert!(pane_text(&app, &terminal).contains("fixture failure"));
        assert_eq!(app.viewers.len(), 1);
        assert!(app.opening.is_none());
    }

    #[test]
    fn history_scroll_loads_older_messages_and_preserves_the_visible_text() {
        let (_d, mut app, mut terminal) = history_fixture(1);
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let path = app.history.rows[0].entry.transcript.clone();
        fs::write(path, (0..110).map(|i| format!("{}\n", json!({"type":if i % 2 == 0 {"user"} else {"assistant"},"message":{"content":format!("message {i:03}")}}))).collect::<String>()).unwrap();
        transcript_until(&mut app, &mut terminal, |a| a.transcript.document.is_some());
        assert!(pane_text(&app, &terminal).contains("message 109"));
        app.focus_transcript();
        app.transcript
            .scroll(-(app.transcript.max_scroll() as isize - 3));
        terminal.draw(|f| app.draw(f)).unwrap();
        let before = pane_text(&app, &terminal);
        transcript_until(&mut app, &mut terminal, |a| {
            a.transcript
                .document
                .as_ref()
                .is_some_and(|d| d.messages.len() == 80)
        });
        assert_eq!(pane_text(&app, &terminal), before);
        app.key(KeyCode::Home, KeyModifiers::NONE).unwrap();
        transcript_until(&mut app, &mut terminal, |a| {
            a.transcript
                .document
                .as_ref()
                .is_some_and(|d| d.older.is_none())
        });
        app.key(KeyCode::Home, KeyModifiers::NONE).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(pane_text(&app, &terminal).contains("message 000"));
        app.key(KeyCode::End, KeyModifiers::NONE).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(pane_text(&app, &terminal).contains("message 109"));
        assert!(app.viewers.is_empty() && app.opening.is_none());
    }

    #[test]
    fn conversation_markdown_keeps_styles_while_wrapping_and_removes_fences() {
        let lines = transcript_markdown(
            "# Result\n\n**The check passed** and `value` is ready.\n\n```rust\n  let value = 1;\n```",
            18,
        );
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("**") && !text.contains("```") && !text.contains("# Result"),
            "{text}"
        );
        assert!(text.contains("let value = 1;"), "{text}");
        assert!(lines.iter().all(|l| l.width() <= 18));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains("check")
                    && s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn conversation_uses_native_markers_and_shaded_prompt_blocks_without_role_headers() {
        let colors = viewer::Colors::default();
        for (harness, prompt, reply, marker) in [
            (
                "claude",
                json!({"type":"user","message":{"content":"Question"}}),
                json!({"type":"assistant","message":{"content":[{"type":"text","text":"Reply **ready**"}]}}),
                "❯ Question",
            ),
            (
                "codex",
                json!({"type":"event_msg","payload":{"item":{"type":"UserMessage","content":[{"text":"Question"}]}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reply **ready**"}]}}),
                "› Question",
            ),
            (
                "pi",
                json!({"type":"message","message":{"role":"user","content":"Question"}}),
                json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Reply **ready**"}]}}),
                "Question",
            ),
        ] {
            let doc = transcript::parse(harness, format!("{prompt}\n{reply}\n").as_bytes());
            let prompt = conversation_message(&doc.messages[0], harness, 30, &colors);
            assert!(prompt.iter().any(|line| line.to_string().trim() == marker));
            if harness != "claude" {
                assert!(prompt.iter().all(|line| line.style.bg.is_some()));
            }
            let reply = conversation_message(&doc.messages[1], harness, 30, &colors);
            let text = reply
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let expected = match harness {
                "claude" => "⏺ Reply ready",
                "codex" => "• Reply ready",
                _ => "Reply ready",
            };
            assert_eq!(text, expected);
        }
        let light = viewer::Colors {
            bg: "rgb:ffff/ffff/ffff".into(),
            ..colors.clone()
        };
        assert_ne!(
            conversation_prompt_background("codex", &colors),
            conversation_prompt_background("codex", &light)
        );
        assert_ne!(
            conversation_prompt_background("pi", &colors),
            conversation_prompt_background("pi", &light)
        );
    }

    #[test]
    fn a_harness_row_turns_it_off_and_the_connectivity_row_answers_for_every_harness() {
        let mut c = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let none = KeyModifiers::NONE;
        c.go(field_at("opencode_enabled"));
        assert!(
            c.config().unwrap().0.enabled_for(HarnessKind::Opencode),
            "an untouched row leaves the harness offered"
        );
        c.key(KeyCode::Left, none);
        assert_eq!(c.values[field_at("opencode_enabled")], "false");
        let saved = c.config().unwrap().0;
        assert!(!saved.enabled_for(HarnessKind::Opencode));
        assert!(
            saved.enabled_for(HarnessKind::Claude),
            "one harness turned off leaves the others alone"
        );

        c.go(field_at("check"));
        assert!(
            c.line()
                .to_string()
                .contains("Check which harnesses can launch")
        );
        c.key(KeyCode::Enter, none);
        let answer = c.line().to_string();
        for kind in harness::launchable() {
            assert!(
                answer.contains(&kind.to_string()),
                "{kind} is missing from {answer}"
            );
        }
        assert!(
            c.values[field_at("check")].is_empty() && c.config().is_ok(),
            "the probe writes nothing into the file"
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
        let session = placeholder(HarnessKind::Claude, "starting:1", claude, "again");
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
        let session = placeholder(HarnessKind::Claude, "starting:2", claude, "once more");
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
        let session = placeholder(
            HarnessKind::Claude,
            "starting:1",
            claude,
            "fix the tests\nplease",
        );
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
        let session = placeholder(HarnessKind::Claude, "starting:2", claude, "again");
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
    fn every_harness_selects_its_launch_before_discovery_and_keeps_it_during_a_slow_read() {
        for &kind in harness::known() {
            let d = dir();
            registry(d.path(), A, d.path().to_str().unwrap(), "idle", 1);
            let mut app = app(d.path());
            app.refresh().unwrap();
            let stale = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
            let id = app.launch_row(kind, d.path(), "fix the tests\nplease");
            assert_eq!(key(&app).as_deref(), Some(id.as_str()));
            assert_eq!(app.focus, None, "{kind} keeps the list focused");
            assert_eq!(app.enter_label(), "starting");
            assert_eq!(app.stop_verb(), None, "no process exists yet");
            assert!(app.selected().unwrap().text().contains("fix the tests"));
            let s = app.selected_session().unwrap();
            assert_eq!(s.harness, kind.to_string());
            assert_eq!(
                (s.pid, s.model.as_ref(), s.context_tokens),
                (None, None, None)
            );
            app.apply(stale);
            assert_eq!(key(&app).as_deref(), Some(id.as_str()));
            app.enter().unwrap();
            assert!(app.status.contains("still starting"));
            assert!(app.viewers.is_empty());
        }
    }

    fn finish_opening(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.poll_opening() {
            assert!(Instant::now() < deadline, "preparation did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn terminal_until(app: &mut App, ready: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready(app) {
            app.pump();
            assert!(
                Instant::now() < deadline,
                "terminal did not become ready: {} {:?}",
                app.status,
                app.viewers
                    .iter()
                    .map(|open| open.viewer.screen().contents())
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_composer_skips_a_harness_config_does_not_offer() {
        let d = dir();
        let jobs = d.path().join("jobs.yaml");
        fs::write(
            &jobs,
            "version: 3\ndefaults:\n  claude_enabled: false\n  pi_enabled: false\njobs: []\n",
        )
        .unwrap();
        let mut app = App::new(Path::new("cones"), &jobs, d.path(), d.path()).unwrap();
        assert_eq!(
            app.launch_name(),
            "codex",
            "start.harness defaults to a harness this file turns off"
        );
        let cycle = |app: &mut App| {
            app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        };
        cycle(&mut app);
        assert_eq!(app.launch_name(), "opencode", "pi is skipped");
        cycle(&mut app);
        assert!(app.terminal_selected(), "the terminal stays reachable");
        cycle(&mut app);
        assert_eq!(app.launch_name(), "codex", "the cycle wraps");
    }

    #[test]
    fn a_terminal_keeps_its_folder_draft_and_viewer_across_refreshes() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let folder = d.path().join("working folder");
        fs::create_dir(&folder).unwrap();
        app.pin_folder(folder.clone()).unwrap();
        app.shell = "/bin/sh".into();
        app.fill("an unfinished agent instruction".into());
        for _ in harness::launchable() {
            app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        }
        assert!(app.terminal_selected());
        assert!(app.composer().to_string().contains("terminal (sh)"));
        app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        app.paste(" a separate command");
        assert_eq!(app.terminal_input.text, "x a separate command");
        assert_eq!(app.text, "an unfinished agent instruction");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(app.terminal_selected());
        assert!(app.terminal_input.text.is_empty());
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert_eq!(app.terminals.len(), 1);
        let id = app.terminals[0].session_id.clone();
        let pid = app.terminals[0].pid.unwrap();
        assert_eq!(key(&app).as_deref(), Some(id.as_str()));
        assert_eq!(app.terminals[0].cwd, folder);
        assert_eq!(app.focus, Some(0));
        assert_eq!(app.viewers[0].harness, None);
        assert!(app.pending.is_empty() && app.opening.is_none());

        app.paste("printf '\\nDIRECTORY=%s\\n' \"$PWD\"\n");
        let expected = format!("DIRECTORY={}", folder.canonicalize().unwrap().display());
        terminal_until(&mut app, |a| {
            a.viewers[0].viewer.screen().contents().contains(&expected)
        });
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0), "Tab belongs to shell completion");
        app.key(KeyCode::Char('c'), KeyModifiers::CONTROL).unwrap();
        assert!(app.quit_armed.is_none(), "Ctrl+C belongs to the shell");
        assert_eq!(app.focus, Some(0));
        assert!(app.hint_line().to_string().contains("ctrl+z back"));
        assert!(app.strip(0, 160).to_string().contains("ctrl+z back"));
        app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
        assert_eq!(app.focus, None);
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(id.as_str()));
        assert_eq!(app.enter_label(), "return");
        assert_eq!(app.terminals.len(), 1);
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.viewers[app.focus.unwrap()].viewer.pid(), pid);
        assert_eq!(app.text, "an unfinished agent instruction");
        app.paste("exit\n");
        terminal_until(&mut app, |a| a.viewers.is_empty());
        assert!(app.terminals.is_empty());
        assert!(!app.data.sessions.iter().any(|s| s.session_id == id));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.terminal_selected());
        assert_eq!(app.text, "an unfinished agent instruction");
    }

    #[test]
    fn the_terminal_launcher_edits_and_preserves_its_own_draft_even_on_the_menu() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert!(app.on_menu());
        app.harness = harness::launchable().len();
        assert!(app.composer().to_string().contains("Type a command"));
        for c in "echo café".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Left, KeyModifiers::NONE).unwrap();
        app.paste("f");
        assert_eq!(
            app.terminal_input,
            Input {
                text: "echo caffé".into(),
                at: 9
            }
        );
        assert!(app.composer().to_string().contains("echo caffé"));
        app.key(KeyCode::End, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap();
        app.paste("pwd\r\nprintf done");
        let command = "echo caffé\npwd\nprintf done";
        assert_eq!(app.terminal_input.text, command);
        assert!(
            app.composer()
                .to_string()
                .contains("echo caffé⏎pwd⏎printf done")
        );
        assert!(app.viewers.is_empty(), "editing does not start a shell");
        app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        app.fill("agent draft".into());
        for _ in 0..harness::launchable().len() {
            app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        }
        assert!(app.terminal_selected());
        assert_eq!(app.terminal_input.text, command);
        app.paste("");
        assert!(app.images.is_empty());
        assert_eq!(app.text, "agent draft");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert_eq!(app.terminal_input, Input::default());
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(!app.terminal_selected());
        assert_eq!(app.text, "agent draft");
    }

    #[test]
    fn a_terminal_launcher_command_runs_in_the_selected_folder_and_keeps_the_shell() {
        use std::os::unix::fs::PermissionsExt;

        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            if !Path::new(shell).is_file() {
                continue;
            }
            let d = dir();
            let folder = d.path().join("working folder");
            fs::create_dir(&folder).unwrap();
            // Keep real shell startup files out of the test and delay the initial prompt.
            let wrapper = d.path().join("test-shell");
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nexport HOME='{}' ZDOTDIR='{}'\nunset ENV BASH_ENV\nexec {shell} \"$@\"\n",
                    d.path().display(),
                    d.path().display()
                ),
            )
            .unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
            for file in [".zshrc", ".bashrc"] {
                fs::write(d.path().join(file), "sleep 0.05\nPS1='READY: '\n").unwrap();
            }
            let mut app = app(d.path());
            app.refresh().unwrap();
            app.pin_folder(folder.clone()).unwrap();
            app.shell = wrapper;
            app.harness = harness::launchable().len();
            app.fill("do not run the agent draft".into());
            app.paste("value='hello café'\nprintf '%s\\n' \"$value\" > 'command output'");
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            assert_eq!(app.focus, Some(0));
            terminal_until(&mut app, |_| {
                fs::read_to_string(folder.join("command output")).is_ok_and(|s| s == "hello café\n")
            });
            assert!(app.terminal_input.text.is_empty());
            assert_eq!(app.text, "do not run the agent draft");
            app.paste("printf '%s\\n' \"$value\" > 'same shell'\n");
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            terminal_until(&mut app, |_| {
                fs::read_to_string(folder.join("same shell")).is_ok_and(|s| s == "hello café\n")
            });
            assert_eq!(app.terminals.len(), 1);
        }
    }

    #[test]
    fn a_terminal_can_be_reopened_with_tab_in_either_layout() {
        for split in [false, true] {
            let d = dir();
            let mut app = app(d.path());
            app.refresh().unwrap();
            app.cwd = d.path().into();
            app.shell = "/bin/sh".into();
            app.harness = harness::launchable().len();
            app.split = split;
            app.fill("keep this agent instruction".into());
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            let id = key(&app).unwrap();
            let pid = app.viewers[app.focus.unwrap()].viewer.pid();
            for _ in 0..2 {
                app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
                assert_eq!(app.focus, None);
                app.refresh().unwrap();
                assert_eq!(key(&app).as_deref(), Some(id.as_str()));
                app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
                assert_eq!(app.focus, Some(0), "Tab must return with split={split}");
                assert_eq!(app.viewers[0].viewer.pid(), pid);
                assert_eq!(app.terminals.len(), 1, "returning must reuse the shell");
                assert_eq!(app.split, split, "returning preserves the layout");
                assert_eq!(app.text, "keep this agent instruction");
            }
            app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
            assert!(app.hint_line().to_string().contains("tab pane"));
        }
    }

    #[test]
    fn only_the_focused_terminal_can_request_a_return_from_its_shell() {
        for terminal in [false, true] {
            for focused in [false, true] {
                let d = dir();
                let mut app = app(d.path());
                let key = if terminal {
                    "terminal:test"
                } else {
                    "agent:test"
                };
                app.viewers.push(viewer_open(
                    key,
                    "shell",
                    r"\033]777;cones;return\007SHELL-READY",
                ));
                app.focus = focused.then_some(0);
                terminal_until(&mut app, |a| {
                    a.viewers[0]
                        .viewer
                        .screen()
                        .contents()
                        .contains("SHELL-READY")
                });
                assert_eq!(app.focus, (focused && !terminal).then_some(0));
                assert_eq!(app.viewers.len(), 1, "returning keeps the shell alive");
                app.focus(0);
                app.pump();
                assert_eq!(app.focus, Some(0), "old requests must not steal focus");
            }
        }
    }

    #[test]
    fn terminal_launches_are_distinct_and_closing_one_keeps_the_other() {
        let d = dir();
        let mut app = app(d.path());
        app.cwd = d.path().into();
        app.shell = "/bin/sh".into();
        app.harness = harness::launchable().len();
        app.refresh().unwrap();
        for _ in 0..4 {
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
        }
        assert_eq!(app.terminals.len(), 4, "user terminals are never evicted");
        assert_eq!(app.viewers.len(), 4);
        let ids: HashSet<_> = app.terminals.iter().map(|s| s.session_id.clone()).collect();
        assert_eq!(ids.len(), 4);
        let closed = key(&app).unwrap();
        let kept = app.terminals[0].session_id.clone();
        app.key(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        assert_eq!(app.viewers.len(), 4, "first press only arms the close");
        app.key(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        assert_eq!(app.viewers.len(), 3);
        assert!(app.viewer_index(&kept).is_some());
        assert!(app.viewer_index(&closed).is_none());
        assert!(
            app.stopping.is_empty(),
            "shells never invoke harness stop commands"
        );
        app.refresh().unwrap();
        assert!(!app.data.sessions.iter().any(|s| s.session_id == closed));
        assert_eq!(app.terminals.len(), 3);
    }

    #[test]
    fn a_failed_terminal_launch_leaves_the_rows_and_instruction_intact() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let before = key(&app);
        app.shell = "/missing/shell".into();
        app.harness = harness::launchable().len();
        app.fill("keep my instruction".into());
        app.paste("printf retry");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(app.viewers.is_empty() && app.terminals.is_empty());
        assert_eq!(key(&app), before);
        assert_eq!(app.text, "keep my instruction");
        assert_eq!(app.terminal_input, Input::new("printf retry"));
        assert!(app.status.contains("failed"));
    }

    #[test]
    fn foreground_launches_keep_selection_and_reuse_their_viewer_when_the_id_changes() {
        for kind in [HarnessKind::Codex, HarnessKind::Pi, HarnessKind::Opencode] {
            for moved in [false, true] {
                let d = dir();
                registry(d.path(), A, d.path().to_str().unwrap(), "idle", 1);
                let mut app = app(d.path());
                app.refresh().unwrap();
                let id = app.launch_row(kind, d.path(), "fix the tests");
                let (release, wait) = mpsc::channel();
                app.prepare_viewer(
                    format!("{kind} in /x"),
                    id.clone(),
                    None,
                    Some("fix the tests".into()),
                    move || {
                        wait.recv_timeout(Duration::from_secs(3))?;
                        let mut c = Command::new("/bin/sh");
                        c.args(["-c", "read line"]);
                        Ok(c)
                    },
                );
                assert!(
                    !app.poll_opening(),
                    "a slow prepare leaves the row on screen"
                );
                if moved {
                    app.select_new(A);
                }
                release.send(()).unwrap();
                finish_opening(&mut app);
                assert_eq!(app.focus, None, "startup keeps dashboard focus");
                assert_eq!(key(&app).as_deref(), Some(if moved { A } else { &id }));
                let pid = app.viewers[0].viewer.pid();
                assert_eq!(app.pending[0].session.pid, Some(pid));
                if !moved {
                    app.enter().unwrap();
                    assert_eq!(app.focus, Some(0));
                }
                for native in [format!("{kind}-{pid}"), B.to_owned()] {
                    let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
                    let mut s = placeholder(kind, &native, d.path(), "");
                    s.title = None;
                    s.pid = Some(pid);
                    s.state = "active".into();
                    data.sessions.push(s);
                    app.apply(data);
                    assert!(app.pending.is_empty());
                    assert_eq!(
                        app.data.sessions.len(),
                        2,
                        "one existing row and one launch"
                    );
                    assert_eq!(key(&app).as_deref(), Some(if moved { A } else { &native }));
                    assert_eq!(app.viewers[0].key, native);
                    assert_eq!(app.viewers[0].viewer.pid(), pid);
                    assert!(app.data.sessions.iter().any(|s| {
                        s.session_id == native && s.title.as_deref() == Some("fix the tests")
                    }));
                }
                app.unfocus();
                app.select_new(B);
                assert_eq!(app.enter_label(), "return");
                app.enter().unwrap();
                assert_eq!(app.viewers.len(), 1);
                assert_eq!(app.focus, Some(0));
            }
        }
    }

    #[test]
    fn a_claude_attach_keeps_the_registry_workers_pid() {
        let d = dir();
        registry_bg(d.path(), A, d.path().to_str().unwrap(), "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let worker = app.selected_session().unwrap().pid;
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "read line"]);
        assert!(app.open((30, 120), c, "attach", A.into(), None));
        assert_ne!(worker, Some(app.viewers[0].viewer.pid()));
        assert_eq!(app.selected_session().unwrap().pid, worker);
        app.refresh().unwrap();
        assert_eq!(app.selected_session().unwrap().pid, worker);
    }

    #[test]
    fn stopping_an_owned_foreground_session_closes_its_viewer_before_and_after_native_identity() {
        for kind in [HarnessKind::Codex, HarnessKind::Pi, HarnessKind::Opencode] {
            for native in [false, true] {
                let d = dir();
                let mut app = app(d.path());
                let id = app.launch_row(kind, d.path(), "fix the tests");
                let mut c = Command::new("/bin/sh");
                c.args(["-c", "read line"]);
                let record =
                    (kind == HarnessKind::Codex).then(|| (d.path().to_owned(), chrono::Utc::now()));
                assert!(app.open((30, 120), c, &kind.to_string(), id, record));
                let pid = app.viewers[0].viewer.pid();
                let id = if native {
                    B.into()
                } else {
                    format!("{kind}-{pid}")
                };
                let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
                let mut s = placeholder(kind, &id, d.path(), "fix the tests");
                s.pid = Some(pid);
                s.state = "active".into();
                if !native {
                    s.kind = None;
                } else if kind == HarnessKind::Codex {
                    let rollout = d.path().join("rollout.jsonl");
                    fs::write(&rollout, "").unwrap();
                    s.transcript_path = Some(rollout);
                }
                data.sessions.push(s);
                app.apply(data);
                app.unfocus();
                assert_eq!(key(&app).as_deref(), Some(id.as_str()));
                app.stop();
                app.stop();
                poll_until(&mut app, |app| app.stopping.is_empty());
                assert!(app.viewers.is_empty());
                assert!(app.pending.is_empty());
                assert!(!app.status.contains("failed"), "{}", app.status);
                assert!(
                    codex::threads(&app.state).is_empty(),
                    "forget removes the saved thread"
                );
            }
        }
    }

    #[test]
    fn a_codex_daemon_id_replaces_its_launch_and_records_only_the_identified_thread() {
        for process_first in [false, true] {
            let d = dir();
            let mut app = app(d.path());
            app.refresh().unwrap();
            let since = chrono::Utc::now();
            let id = app.launch_row(HarnessKind::Codex, d.path(), "fix the tests");
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "read line"]);
            assert!(app.open(
                (30, 120),
                c,
                "codex in /x",
                id.clone(),
                Some((d.path().to_owned(), since)),
            ));
            let pid = app.viewers[0].viewer.pid();
            if process_first {
                let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
                let mut s = placeholder(
                    HarnessKind::Codex,
                    &format!("codex-{pid}"),
                    d.path(),
                    "fix the tests",
                );
                s.kind = None;
                s.pid = Some(pid);
                data.sessions.push(s);
                app.apply(data);
            }
            app.unfocus();
            assert!(
                !app.viewers[0].recorded,
                "no thread has been identified yet"
            );
            assert!(codex::threads(&app.state).is_empty());
            let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
            let mut s = placeholder(HarnessKind::Codex, B, d.path(), "reported title");
            s.state = "active".into();
            let rollout = d.path().join("rollout.jsonl");
            fs::write(
                &rollout,
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {"item": {"type": "UserMessage", "content": [
                        {"type": "text", "text": "fix the tests"}
                    ]}}
                })
                .to_string(),
            )
            .unwrap();
            s.transcript_path = Some(rollout);
            assert_eq!(s.pid, None, "the daemon row has no client pid");
            data.sessions.push(s);
            app.apply(data);
            assert_eq!(app.data.sessions.len(), 1);
            assert_eq!(key(&app).as_deref(), Some(B));
            assert_eq!(app.viewers[0].key, B);
            assert_eq!(app.selected_session().unwrap().pid, Some(pid));
            assert_eq!(codex::threads(&app.state)[0].id, B);
            app.enter().unwrap();
            assert_eq!(app.viewers.len(), 1);
            assert_eq!(app.viewers[0].viewer.pid(), pid);
        }
    }

    #[test]
    fn ambiguous_codex_threads_do_not_claim_a_launch_or_record_the_newest_thread() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let since = chrono::Utc::now();
        let id = app.launch_row(HarnessKind::Codex, d.path(), "fix the tests");
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "read line"]);
        app.open(
            (30, 120),
            c,
            "codex in /x",
            id.clone(),
            Some((d.path().to_owned(), since)),
        );
        let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        for native in [A, B] {
            let mut s = placeholder(HarnessKind::Codex, native, d.path(), "another launch");
            s.state = "active".into();
            let rollout = d.path().join(format!("{native}.jsonl"));
            fs::write(
                &rollout,
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {"item": {"type": "UserMessage", "content": [
                        {"type": "text", "text": "fix the tests"}
                    ]}}
                })
                .to_string(),
            )
            .unwrap();
            s.transcript_path = Some(rollout);
            data.sessions.push(s);
        }
        app.apply(data);
        assert_eq!(app.viewers[0].key, id);
        assert_eq!(key(&app).as_deref(), Some(id.as_str()));
        app.unfocus();
        assert!(!app.viewers[0].recorded);
        assert!(codex::threads(&app.state).is_empty());
    }

    #[test]
    fn an_unrelated_codex_prompt_in_the_launch_folder_cannot_claim_its_viewer() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let since = chrono::Utc::now();
        let id = app.launch_row(HarnessKind::Codex, d.path(), "fix the tests");
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "read line"]);
        app.open(
            (30, 120),
            c,
            "codex in /x",
            id.clone(),
            Some((d.path().to_owned(), since)),
        );
        let mut data = Data::load(&app.jobs_path, &app.state, &app.claude).unwrap();
        let mut s = placeholder(HarnessKind::Codex, B, d.path(), "fix the tests");
        let rollout = d.path().join("other.jsonl");
        fs::write(
            &rollout,
            serde_json::json!({
                "type": "event_msg",
                "payload": {"item": {"type": "UserMessage", "content": [
                    {"type": "text", "text": "a different task"}
                ]}}
            })
            .to_string(),
        )
        .unwrap();
        s.transcript_path = Some(rollout);
        data.sessions.push(s);
        app.apply(data);
        assert_eq!(app.viewers[0].key, id);
        assert!(!app.viewers[0].recorded);
        assert_eq!(key(&app).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn cancelling_or_failing_a_foreground_launch_removes_its_row_and_restores_the_prompt() {
        for kind in [HarnessKind::Codex, HarnessKind::Pi, HarnessKind::Opencode] {
            for failure in ["cancel", "prepare", "spawn"] {
                let d = dir();
                let mut app = app(d.path());
                let id = app.launch_row(kind, d.path(), "fix the tests\nplease");
                app.prepare_viewer(
                    kind.to_string(),
                    id,
                    None,
                    Some("fix the tests\nplease".into()),
                    move || {
                        if failure == "prepare" {
                            anyhow::bail!("fixture refused launch");
                        }
                        Ok(Command::new("/a/fixture/program/that/does/not/exist"))
                    },
                );
                if failure == "cancel" {
                    assert!(app.cancel_opening());
                } else {
                    finish_opening(&mut app);
                    assert!(app.status.contains("failed"));
                }
                assert!(app.pending.is_empty());
                assert!(app.data.sessions.is_empty());
                assert!(app.viewers.is_empty());
                assert_eq!(app.text, "fix the tests\nplease");
            }
        }
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
        app.log = Some(Diagnostics::new(log.clone(), false));
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
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            records.iter().any(|r| r["event"] == "viewer.failed"
                && r["data"]["error"] == "no daemon"
                && r["data"]["row_id"] == "codex:test"),
            "{text}"
        );
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
            operation: None,
            context: json!({}),
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
                operation: None,
                context: json!({}),
                id: A.into(),
                label: "aaaaaaaa".into(),
                verb: "delete",
                result: first_rx,
            },
            PendingStop {
                operation: None,
                context: json!({}),
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
                "enter start job · ctrl+x delete · ctrl+e edit · shift+tab session · esc back"
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
            hint.starts_with("enter add folder")
                && hint.contains("ctrl+h history")
                && hint.contains("← → pick")
                && hint.ends_with("esc quit"),
            "an empty dashboard opens on the menu row, folder picked: {hint}"
        );
        app.text = "fix the tests".into();
        app.shell = "/bin/sh".into();
        for (prefix, name) in [
            (">_ codex", "codex"),
            ("\u{3c0} pi", "pi"),
            ("o opencode", "opencode"),
            ("terminal (sh)", "terminal"),
            ("\u{273b} claude", "claude"),
        ] {
            app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
            let (composer, hint) = (text(app.composer()), text(app.hint_line()));
            assert!(
                composer.starts_with(&format!("{prefix} \u{203a} ")),
                "{composer}"
            );
            assert!(
                hint.starts_with(&format!("enter start {name} in ")),
                "{hint}"
            );
            assert!(hint.contains("shift+tab session"), "{hint}");
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
            job.cells[2].0.trim() == "off" && job.cells[4].0.trim() == "0 2 * * *",
            "status and schedule are independent cells around the job name: {text}"
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
            names.contains("model")
                && names.contains("folder")
                && names.contains("next run")
                && !names.contains("context"),
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
            row.text().ends_with(" · 1 change"),
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
    fn whole_columns_keep_headings_and_short_values_with_their_widest_value() {
        let row = |activity: &str| {
            vec![
                ("▁".into(), plain()),
                ("x".into(), plain()),
                (activity.into(), plain()),
            ]
        };
        let (header, rows) = columns(
            &["", "title", "activity"],
            vec![row("▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁"), row("-")],
            &mut Widths::new(),
        );
        let spans = |cells: &[(String, Style)]| {
            cells
                .iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect()
        };
        assert_eq!(whole_cells(spans(&header.cells), 20, 2).len(), 2);
        for row in &rows {
            assert_eq!(whole_cells(spans(row), 18, 2).len(), 2);
            assert_eq!(whole_cells(spans(row), 26, 2).len(), 3);
        }
        assert_eq!(whole_cells(spans(&header.cells), 28, 2).len(), 3);
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
            harness: Some(
                harness::by_name(what.split_whitespace().next().unwrap_or(""))
                    .map_or(HarnessKind::Claude, |s| s.kind),
            ),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: false,
            operation: None,
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
            strip.trim_end().ends_with("ctrl+z back · ctrl+\\ split"),
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
    /// the only way back in once discovery replaces the launch placeholder.
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

    #[test]
    fn each_harness_returns_from_empty_input_and_reenters_the_same_viewer() {
        for &kind in harness::known() {
            let d = dir();
            let mut app = app(d.path());
            let spec = harness::spec(kind);
            let mut row = placeholder(kind, A, d.path(), "fixture");
            row.state = "idle".into();
            app.data.sessions.push(row);
            app.rebuild();
            app.select_new(A);
            let rule = "─".repeat(80);
            let screen = if kind == HarnessKind::Pi {
                // The native Pi editor paints no prompt marker and hides the hardware caret.
                format!(
                    "VIEW\\033[2;1H{rule}\\033[3;1H \\033[7m \\033[0m\\033[4;1H{rule}\\033[3;2H\\033[?25l"
                )
            } else if kind == HarnessKind::Opencode {
                let bottom = "▀".repeat(77);
                format!(
                    "VIEW\\033[2;3H┃\\033[3;3H┃\\033[4;3H┃\\033[5;3H┃  Build · Fixture\\033[6;3H╹{bottom}\\033[3;6H\\033[?25h"
                )
            } else {
                "VIEW\\033[3;1H> \\033[?25h".into()
            };
            let mut open = viewer_open(A, &spec.commands.viewer, &screen);
            open.harness = Some(kind);
            app.data.sessions[0].pid = Some(open.viewer.pid());
            let pid = open.viewer.pid();
            app.viewers.push(open);
            wait_paint(&mut app, 0, "VIEW");
            app.focus = Some(0);
            if kind == HarnessKind::Opencode {
                app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
                assert_eq!(app.focus, Some(0));
            }
            let back = if kind == HarnessKind::Opencode {
                "←"
            } else {
                "tab"
            };
            assert!(
                app.hint_line()
                    .to_string()
                    .starts_with(&format!("{back} back")),
                "{kind}"
            );
            assert!(
                app.strip(0, 120)
                    .to_string()
                    .ends_with(&format!("{back} back · ctrl+\\ split")),
                "{kind}"
            );
            for modifier in [
                KeyModifiers::ALT,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
            ] {
                assert!(!app.key(KeyCode::Left, modifier).unwrap());
                assert_eq!(
                    app.focus,
                    Some(0),
                    "{kind}: modified arrows belong to the client"
                );
            }
            assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
            assert_eq!(app.focus, None, "{kind}: Left returns to cones");
            assert_eq!(app.viewers.len(), 1);
            app.enter().unwrap();
            assert_eq!(
                app.focus,
                Some(0),
                "{kind}: Enter returns to the existing viewer"
            );
            assert_eq!(app.viewers[0].viewer.pid(), pid);
            for (key, modifier) in [
                (KeyCode::Tab, KeyModifiers::NONE),
                (KeyCode::Char('z'), KeyModifiers::CONTROL),
            ] {
                if kind == HarnessKind::Opencode && key == KeyCode::Tab {
                    continue;
                }
                app.key(key, modifier).unwrap();
                assert_eq!(app.focus, None, "{kind}");
                app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
                assert_eq!(app.focus, Some(0), "{kind}");
                assert_eq!(
                    app.viewers[0].viewer.pid(),
                    pid,
                    "returning starts no replacement client"
                );
            }
            if kind == HarnessKind::Opencode {
                app.close(0);
                assert!(
                    app.data.sessions.iter().all(|s| s.pid != Some(pid)),
                    "closing the native client makes its conversation available to history"
                );
            }
        }
    }

    #[test]
    fn tab_reaches_each_harness_with_a_draft_and_ctrl_z_returns() {
        for &kind in harness::known() {
            let d = dir();
            let mut app = app(d.path());
            let received = d.path().join("received");
            let rule = "─".repeat(80);
            let screen = if kind == HarnessKind::Pi {
                format!(
                    "\x1b[HVIEW\x1b[2;1H{rule}\x1b[3;1H /com\x1b[7m \x1b[0m\x1b[4;1H{rule}\x1b[3;6H\x1b[?25l"
                )
            } else {
                "\x1b[HVIEW\x1b[3;1H> /com\x1b[?25h".into()
            };
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "stty raw -echo; printf '%s' \"$1\"; cat > \"$2\"",
                    "viewer",
                    &screen,
                ])
                .arg(&received);
            app.viewers.push(Open {
                key: A.into(),
                what: kind.to_string(),
                harness: Some(kind),
                viewer: Viewer::spawn(command, 12, 80, None, viewer::Colors::default()).unwrap(),
                record: None,
                recorded: false,
                first_paint_logged: false,
                last_focused: Instant::now(),
                speculative: false,
                operation: None,
            });
            wait_paint(&mut app, 0, "VIEW");
            app.focus = Some(0);
            let pid = app.viewers[0].viewer.pid();
            assert!(
                app.hint_line().to_string().starts_with("ctrl+z back"),
                "{kind}"
            );
            assert!(
                app.strip(0, 120)
                    .to_string()
                    .ends_with("ctrl+z back · ctrl+\\ split"),
                "{kind}"
            );
            for (key, modifiers) in [
                (KeyCode::Tab, KeyModifiers::NONE),
                (KeyCode::BackTab, KeyModifiers::SHIFT),
                (KeyCode::Tab, KeyModifiers::ALT),
            ] {
                assert!(!app.key(key, modifiers).unwrap());
                assert_eq!(app.focus, Some(0), "{kind}: completion keeps focus");
            }
            terminal_until(&mut app, |_| {
                std::fs::read(&received).is_ok_and(|bytes| bytes == b"\t\x1b[Z\x1b\t")
            });
            assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
            assert_eq!(app.focus, None, "{kind}: Ctrl+Z returns with a draft");
            assert_eq!(app.viewers.len(), 1, "{kind}: the viewer stays alive");
            assert_eq!(app.viewers[0].viewer.pid(), pid);
        }
    }

    #[test]
    fn pi_left_with_a_draft_stays_in_the_native_editor() {
        let d = dir();
        let mut app = app(d.path());
        let rule = "─".repeat(80);
        let screen = format!(
            "VIEW\\033[2;1H{rule}\\033[3;1H \\033[7mk\\033[0meep this draft\\033[4;1H{rule}\\033[3;2H\\033[?25l"
        );
        app.viewers.push(viewer_open(A, "pi", &screen));
        wait_paint(&mut app, 0, "VIEW");
        app.focus = Some(0);
        assert!(!app.key(KeyCode::Left, KeyModifiers::NONE).unwrap());
        assert_eq!(app.focus, Some(0));
        assert!(!app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap());
        assert_eq!(app.focus, Some(0), "the native mode switch stays native");
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
            cost_info: None,
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
            text.trim_end().ends_with("ctrl+z back · ctrl+\\ split"),
            "a frame wide enough for the split offers it: {text}"
        );
        assert_eq!(line.width(), 200, "padded to the width");

        let text = app.strip(0, 60).to_string();
        assert!(!text.contains("needs"), "no partial note: {text}");
        assert!(text.starts_with("▲ cones · the one on screen"), "{text}");
        assert!(
            text.trim_end().ends_with("ctrl+z back · ctrl+\\ split"),
            "{text}"
        );
        assert_eq!(app.strip(0, 60).width(), 60);

        let text = app.strip(0, 24).to_string();
        assert!(text.trim_end().ends_with("ctrl+z back"), "{text}");
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
            harness: Some(HarnessKind::Claude),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: false,
            operation: None,
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
            cells(&t, 28, 0..100).starts_with("ctrl+z back"),
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
        for what in ["pi", "pi in /x"] {
            app.viewers[0].what = what.into();
            app.viewers[0].harness = Some(HarnessKind::Pi);
            assert_eq!(
                app.foot_rows(),
                2,
                "pi keeps the footer below its input rule"
            );
        }
        for what in ["codex", "codex in /x", "logs"] {
            app.viewers[0].what = what.into();
            app.viewers[0].harness = (what != "logs").then_some(HarnessKind::Codex);
            for focus in [None, Some(0)] {
                app.focus = focus;
                assert_eq!(app.foot_rows(), 1, "{what} does not move the composer");
            }
        }
    }

    #[test]
    fn a_pi_input_near_the_top_does_not_move_the_dashboard_composer() {
        let d = dir();
        let mut app = app(d.path());
        let rule = "─".repeat(60);
        app.viewers.push(viewer_open(
            "pi:start:test",
            "pi in /x",
            &format!("PI\\r\\n{rule}\\r\\ninput\\r\\n{rule}\\r\\nfolder\\r\\nmodel"),
        ));
        app.focus = Some(0);
        wait_paint(&mut app, 0, "PI");
        assert_eq!(app.foot_rows(), 1);
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
            !screen.iter().any(|r| r.contains("ctrl+z back")),
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
            !left[29].contains("ctrl+]"),
            "unfocused, the hint line has no viewer keys: {:?}",
            left[29]
        );
        assert!(
            left[29].contains("ctrl+\\ layout"),
            "the layout key is listed whenever there is a pane: {:?}",
            left[29]
        );

        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0));
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0), "a screen without a prompt keeps tab");
        app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap();
        assert_eq!(app.focus, None);
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
        assert!(hint.contains("ctrl+z back"), "{hint:?}");
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
                .starts_with("filter: one  ctrl+z back"),
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
            strip.trim_end().ends_with("ctrl+z back · ctrl+\\ split"),
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
        assert!(wide.ends_with("shift+tab session · esc quit"), "{wide}");
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
            cost_info: None,
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
        while !app.menu_is("help") {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        assert!(app.menu_is("help"));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("Move between rows") && pane(&t).contains("Enter to search"),
            "hover: {}",
            pane(&t)
        );
        assert!(pane(&t).contains("guide › the keys"), "{}", pane(&t));
        assert!(left(&t).contains(&A[..8]), "the list stays: {}", left(&t));
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 0, .. })));
        assert!(app.split_active());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("Move between rows") && pane(&t).contains("search ›"),
            "{}",
            pane(&t)
        );
        assert!(left(&t).contains(&A[..8]), "{}", left(&t));
        assert!(
            !left(&t).contains("Type an instruction…"),
            "the list is on a button, which takes no instruction: {}",
            left(&t)
        );
        assert!(!app.key(KeyCode::Down, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 1, .. })));
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Normal), "ctrl+z leaves the guide");
        t.draw(|f| app.draw(f)).unwrap();
        assert!(
            pane(&t).contains("Move between rows"),
            "still picked: {}",
            pane(&t)
        );
        assert!(!app.key(KeyCode::Enter, KeyModifiers::SHIFT).unwrap());
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 0, .. })));
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
            left(&t).contains("jobs   config   columns   help   the jobs: start"),
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
    fn right_on_a_row_with_nothing_typed_goes_to_the_agent() {
        let d = dir();
        registry(d.path(), A, "/src/one", "idle", 1_757_682_871_000);
        let mut app = app(d.path());
        app.refresh().unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Session(..))) {
            app.step(1);
        }
        app.split = false;
        assert!(!app.key(KeyCode::Right, KeyModifiers::NONE).unwrap());
        assert!(
            !app.split,
            "a closed pane stays closed: the agent takes the whole frame"
        );
        assert!(
            app.status.contains("own terminal"),
            "right reached the agent itself: {:?}",
            app.status
        );
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
        app.menu = MENU.iter().position(|m| m.0 == "config").unwrap();
        app.enter().unwrap();
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 34)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let text = rows(&t, 120).join("\n");
        assert!(
            text.contains("chart scale") && !text.contains("use Bedrock"),
            "{text}"
        );
        assert!(
            text.contains("Time allowed for the second ctrl+x press"),
            "{text}"
        );
        assert!(
            text.contains("[ ] group") && text.contains("esc done"),
            "{text}"
        );
        let go = |app: &mut App, name| {
            if let Mode::Config(f) = &mut app.mode {
                f.go(field_at(name));
            }
        };
        go(&mut app, "activity.metric");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert_eq!(
            config::file_activity(&app.jobs_path).unwrap().metric,
            "messages"
        );
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        go(&mut app, "timeout_min");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        for c in "abc".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Config(f) if f.open && f.error.as_ref().unwrap().starts_with("timeout_min:"))
        );
        assert_eq!(config::defaults(&app.jobs_path).timeout_min, None);
        for _ in 0..3 {
            app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Char('5'), KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        go(&mut app, "write");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        go(&mut app, "model");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        for _ in 0..3 {
            app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        let saved = config::defaults(&app.jobs_path);
        assert_eq!((saved.timeout_min, saved.write), (Some(5.0), Some(true)));
        assert_eq!(saved.model.as_deref(), Some("opus[1m]"));
        assert_eq!(config::file_columns(&app.jobs_path), None);
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(app.selected().map(|r| &r.kind), Some(Kind::Session(id, _)) if id == A));
        assert_eq!(app.config_form().values[field_at("model")], "opus[1m]");
    }

    #[test]
    fn config_selection_and_choices_stay_visible_in_short_panes() {
        let mut form = ConfigForm::new(
            &config::Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        for height in [1, 3, 4, 7, 8, 20] {
            for choice in [false, true] {
                form.go(field_at(if choice { "model" } else { "pi_provider" }));
                if choice {
                    form.key(KeyCode::Enter, KeyModifiers::NONE);
                    form.key(KeyCode::End, KeyModifiers::NONE);
                }
                let mut t = Terminal::new(ratatui::backend::TestBackend::new(40, height)).unwrap();
                t.draw(|f| form.draw(f, f.area())).unwrap();
                let text = rows(&t, 40).join("\n");
                let selected = text
                    .lines()
                    .find(|l| l.starts_with('›'))
                    .unwrap_or_else(|| panic!("{height}, choice={choice}: {text}"));
                assert!(
                    selected.contains(if choice {
                        "type a custom value"
                    } else {
                        "provider"
                    }),
                    "{selected}"
                );
                form.key(KeyCode::PageUp, KeyModifiers::NONE);
                t.draw(|f| form.draw(f, f.area())).unwrap();
                assert!(rows(&t, 40).iter().any(|l| l.starts_with('›')));
            }
        }
    }

    #[test]
    fn config_mouse_and_choice_saves_keep_focus_in_both_pane_layouts() {
        let d = dir();
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        for layout in ["right", "bottom", "full"] {
            let mut app = app(d.path());
            app.split = layout != "full";
            app.data.pane.at = if layout == "bottom" {
                "bottom"
            } else {
                "right"
            }
            .into();
            app.mode = Mode::Config(app.config_form());
            let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 44)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            assert!(app.wants_mouse());
            let Mode::Config(form) = &app.mode else {
                unreachable!()
            };
            let area = form.area;
            app.mouse(click(area.x + 10, area.y + 1));
            assert!(matches!(&app.mode, Mode::Config(f) if f.tab() == 1));
            if let Mode::Config(form) = &mut app.mode {
                form.go(field_at("model"));
            }
            t.draw(|f| app.draw(f)).unwrap();
            let Mode::Config(form) = &app.mode else {
                unreachable!()
            };
            let (_, at) = form.lines(form.area.width);
            let selected_y = form.area.y + form.header_rows() + (at - form.top) as u16;
            let value_x = form.area.x + form.label_width(form.area.width) as u16 + 5;
            app.mouse(click(area.x + 5, selected_y));
            assert!(matches!(&app.mode, Mode::Config(f) if f.choice.is_none()));
            app.mouse(click(value_x, selected_y));
            assert!(matches!(&app.mode, Mode::Config(f) if f.choice.is_some()));
            t.draw(|f| app.draw(f)).unwrap();
            let Mode::Config(form) = &app.mode else {
                unreachable!()
            };
            app.mouse(click(form.area.x + 8, form.area.y + form.header_rows() + 1));
            assert_eq!(
                config::defaults(&app.jobs_path).model.as_deref(),
                Some("fable")
            );
            t.draw(|f| app.draw(f)).unwrap();
            let Mode::Config(form) = &app.mode else {
                unreachable!()
            };
            assert!(form.choice.is_none() && form.field().name == "model");
            let (_, at) = form.lines(form.area.width);
            assert_eq!(
                form.area.y + form.header_rows() + (at - form.top) as u16,
                selected_y,
                "{layout}"
            );
            let before = form.area;
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            assert!(matches!(&app.mode, Mode::Config(f) if f.area == before));
        }
    }

    #[test]
    fn config_failed_save_keeps_the_saved_choice_and_the_picker_open() {
        let d = dir();
        let mut app = app(d.path());
        let valid = "version: 3\ndefaults:\n  model: opus\njobs: []\n";
        fs::write(&app.jobs_path, valid).unwrap();
        let mut form = app.config_form();
        form.go(field_at("model"));
        app.mode = Mode::Config(form);
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        let invalid = "version: 3\njobs: [\n";
        fs::write(&app.jobs_path, invalid).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Config(f) if f.choice.is_some() && f.values[f.row] == "opus" && f.error.is_some())
        );
        assert_eq!(fs::read_to_string(&app.jobs_path).unwrap(), invalid);
        fs::write(&app.jobs_path, valid).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert_eq!(
            config::defaults(&app.jobs_path).model.as_deref(),
            Some("opus[1m]")
        );
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
        assert!(
            s.contains(" folder   jobs   config   columns   help "),
            "{s}"
        );
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
        assert!(matches!(app.mode, Mode::Guide(Guide { top: 0, .. })));
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
    fn harness_names_start_in_the_same_column_whatever_the_mark_is() {
        let start = |harness: &str| {
            let cell = logo_cell(harness);
            let name = cell.find(harness).expect("the cell names the harness");
            Span::raw(&cell[..name]).width()
        };
        assert_eq!(start("codex"), 3, "a two-cell mark and one space");
        assert_eq!(start("claude"), start("codex"));
        assert_eq!(start("pi"), start("codex"));
        assert_eq!(start("terminal"), start("codex"));
        assert_eq!(logo_cell("nosuch"), "nosuch", "no mark, nothing to pad");
    }

    #[test]
    fn run_columns_keep_icons_and_use_reported_values_for_live_and_finished_runs() {
        use crate::ledger::{Record, Status};
        let d = dir();
        let jobs = d.path().join("none.yaml");
        fs::write(
            &jobs,
            "version: 3\nrun_columns: [model, context, tokens, dir, trigger, last]\njobs: []\n",
        )
        .unwrap();
        let output = d.path().join("events.jsonl");
        fs::write(&output, concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fixture-model\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"fixture-model\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":2000,\"output_tokens\":7},\"content\":[{\"type\":\"text\",\"text\":\"Reported reply\"}]}}\n",
        )).unwrap();
        let mut record = Record::new(A.into(), Status::Started);
        record.harness = Some(HarnessKind::Claude);
        record.session_id = Some(B.into());
        record.job = Some("fixture-job".into());
        record.cwd = Some(d.path().into());
        record.trigger = Some("manual".into());
        record.fired_at = Some(chrono::Utc::now());
        record.output = Some(output.clone());
        record.policy = Some(serde_json::json!({"model":"unreported-config-model"}));
        let ledger = Ledger::new(d.path()).unwrap();
        ledger.append(&record).unwrap();
        let row = |data: &Data| {
            data.rows(false)
                .into_iter()
                .find(|r| matches!(r.kind, Kind::Run(..)))
                .unwrap()
        };
        let load = || Data::load(&jobs, d.path(), d.path()).unwrap();
        let mut data = load();
        let live = row(&data);
        let texts: Vec<&str> = live.cells.iter().map(|c| c.0.trim()).collect();
        assert_eq!(
            &texts[1..7],
            [
                "✻",
                "fixture-job",
                "fixture-model",
                "2k",
                "2k/7",
                d.path().to_str().unwrap()
            ]
        );
        assert_eq!(&texts[7..], ["manual", "Reported reply"]);
        fs::create_dir(d.path().join("statusline")).unwrap();
        fs::write(
            d.path().join(format!("statusline/{B}.json")),
            r#"{"context_window":{"context_window_size":200000}}"#,
        )
        .unwrap();
        let mut terminal = Record::new(A.into(), Status::Ok);
        terminal.tokens_in = Some(9000);
        terminal.tokens_out = Some(40);
        terminal.duration_s = Some(74.2);
        terminal.cost_usd = Some(0.21);
        terminal.transcript = Some(d.path().join("archived.jsonl"));
        fs::rename(&output, terminal.transcript.as_ref().unwrap()).unwrap();
        ledger.append(&terminal).unwrap();
        data = load();
        assert!(row(&data).text().contains("2k/200k"));
        assert!(row(&data).text().contains("9k/40"));
        data.run_columns = vec![
            "harness".into(),
            "status".into(),
            "took".into(),
            "cost".into(),
        ];
        let finished = row(&data);
        let texts: Vec<&str> = finished.cells.iter().map(|c| c.0.trim()).collect();
        assert_eq!(
            &texts[1..],
            ["✻  claude", "ok", "fixture-job", "74s", "$0.21"]
        );
        data.run_columns.clear();
        assert_eq!(
            row(&data).cells.len(),
            3,
            "only status icon, harness icon and job remain"
        );
        fs::remove_file(terminal.transcript.unwrap()).unwrap();
        data = load();
        assert!(row(&data).cells[3..5].iter().all(|c| c.0.trim() == "-"));
        assert!(!row(&data).text().contains("unreported-config-model"));
        data.runs[0].started.harness = None;
        assert_eq!(
            row(&data).cells[1].0.trim(),
            "-",
            "old records do not acquire an invented harness"
        );
    }

    #[test]
    fn columns_picker_keeps_focus_on_toggle_and_remembers_each_table() {
        let d = dir();
        let mut picker = ColumnsPicker::new(&d.path().join("none.yaml"), 0);
        picker.key(KeyCode::Down);
        let before = picker.current().clone();
        assert!(matches!(
            picker.key(KeyCode::Char(' ')),
            ColumnAction::Save(_)
        ));
        assert_eq!(picker.current().at, before.at);
        assert_eq!(picker.current().order, before.order);
        assert!(!picker.current().shown.contains(before.selected()));
        picker.key(KeyCode::Char(' '));
        assert_eq!(picker.current().chosen(), before.chosen());
        picker.key(KeyCode::Char(']'));
        assert_eq!(picker.current().selected(), before.selected());
        assert_eq!(picker.current().position(), Some(2));
        picker.key(KeyCode::Right);
        picker.key(KeyCode::End);
        let run = picker.current().selected().to_owned();
        picker.key(KeyCode::Down);
        assert_eq!(picker.current().selected(), run);
        picker.key(KeyCode::Left);
        assert_eq!(picker.current().selected(), before.selected());
        picker.key(KeyCode::Left);
        assert_eq!(picker.tab, 0);
        picker.key(KeyCode::Right);
        assert_eq!(picker.current().selected(), run);
        for _ in 0..8 {
            picker.key(KeyCode::Right);
        }
        assert_eq!(picker.tab, 3);
        picker.key(KeyCode::Home);
        picker.key(KeyCode::Up);
        assert_eq!(picker.current().at, 0);
        assert!(picker.tabs, "up past the first column reaches the buttons");
        let before_tabs = picker.sets.clone();
        for (key, tab) in [
            (KeyCode::Right, 0),
            (KeyCode::Left, 3),
            (KeyCode::Home, 0),
            (KeyCode::Char('['), 3),
            (KeyCode::Char(']'), 0),
            (KeyCode::Backspace, 0),
            (KeyCode::End, 3),
            (KeyCode::Home, 0),
            (KeyCode::Right, 1),
        ] {
            assert!(matches!(picker.key(key), ColumnAction::Stay));
            assert_eq!(picker.tab, tab);
            assert!(picker.tabs, "switching tables keeps the buttons focused");
            assert_eq!(picker.sets, before_tabs, "browsing never changes columns");
        }
        assert_eq!(picker.current().selected(), run);
        assert!(picker.hints().to_string().contains("↓ columns"));
        picker.key(KeyCode::Down);
        assert!(!picker.tabs);
        assert_eq!(picker.current().selected(), run, "down restores the cursor");
        for enter in [KeyCode::Enter, KeyCode::Char(' ')] {
            picker.key(KeyCode::Home);
            picker.key(KeyCode::Up);
            let before = picker.sets.clone();
            assert!(matches!(picker.key(enter), ColumnAction::Stay));
            assert!(!picker.tabs);
            assert_eq!(picker.sets, before, "entering the columns saves nothing");
        }
        picker.key(KeyCode::Up);
        assert!(matches!(picker.key(KeyCode::Esc), ColumnAction::Close));
    }

    #[test]
    fn columns_menu_uses_the_table_we_left_and_config_returns_to_its_link() {
        let d = dir();
        let mut app = app(d.path());
        for (kind, tab) in [
            (Kind::Session(A.into(), "idle".into()), 0),
            (Kind::Run(A.into(), "ok".into()), 1),
            (Kind::Job("test".into()), 2),
            (Kind::History("test".into()), 3),
        ] {
            app.mode = Mode::Normal;
            app.rows = vec![
                Row {
                    kind: Kind::Menu,
                    cells: vec![],
                },
                Row {
                    kind,
                    cells: vec![],
                },
            ];
            app.visible = vec![0, 1];
            app.cursor = 1;
            app.step(-1);
            app.menu = MENU
                .iter()
                .position(|(name, ..)| *name == "columns")
                .unwrap();
            app.open_menu();
            assert!(matches!(&app.mode, Mode::Columns(p) if p.tab == tab));
            assert_eq!(app.panel(), Some("columns"));
            assert!(app.panel_focused());
        }
        fs::write(&app.jobs_path, "version: 3\ncolumns: [state]\njobs: []\n").unwrap();
        app.column_context = 0;
        let mut config = app.config_form();
        config.go(field_at("columns"));
        app.mode = Mode::Config(config);
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert_eq!(config::file_columns(&app.jobs_path), Some(vec![]));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.row == field_at("columns")));
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert_eq!(
            config::file_columns(&app.jobs_path),
            Some(vec![]),
            "another config save keeps the picker change"
        );
    }

    #[test]
    fn columns_picker_failed_save_restores_the_checkmark_and_live_table() {
        let d = dir();
        let mut app = app(d.path());
        let valid = "version: 3\ncolumns: [context, model]\njobs: []\n";
        fs::write(&app.jobs_path, valid).unwrap();
        app.refresh().unwrap();
        app.open_columns(None);
        let before = match &app.mode {
            Mode::Columns(f) => f.current().clone(),
            _ => unreachable!(),
        };
        let columns = app.data.columns.clone();
        let invalid = "version: 3\njobs: [\n";
        fs::write(&app.jobs_path, invalid).unwrap();
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Columns(f) if f.error.is_some() && *f.current() == before)
        );
        assert_eq!(app.data.columns, columns);
        assert_eq!(fs::read_to_string(&app.jobs_path).unwrap(), invalid);
        fs::write(&app.jobs_path, valid).unwrap();
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Columns(f) if f.error.is_none()));
        assert_eq!(app.data.columns, ["model"]);
    }

    #[test]
    fn columns_picker_scrolling_mouse_and_indicators_work_in_short_panes() {
        let d = dir();
        let mut picker = ColumnsPicker::new(&d.path().join("none.yaml"), 0);
        picker.key(KeyCode::End);
        let last = picker.current().selected().to_owned();
        for height in [1, 3, 4, 7, 8, 20] {
            let mut t = Terminal::new(ratatui::backend::TestBackend::new(60, height)).unwrap();
            t.draw(|f| picker.draw(f, f.area())).unwrap();
            let text = (0..height)
                .map(|y| cells(&t, y, 0..60))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(&format!("› [ ]   {last}")),
                "{height} rows: {text}"
            );
            assert!(picker.hints().width() <= 60);
            picker.key(KeyCode::Home);
            picker.key(KeyCode::Up);
            t.draw(|f| picker.draw(f, f.area())).unwrap();
            assert!(
                !(0..height).any(|y| cells(&t, y, 0..60).starts_with("› ")),
                "the columns lose their focus marker when the buttons have focus"
            );
            if height >= 4 {
                let y = if height < 8 { 0 } else { 1 };
                let cell = t.backend().buffer().cell((1, y)).unwrap();
                assert_eq!(cell.bg, ORANGE);
                assert_eq!(cell.fg, Color::Black);
                assert!(cells(&t, y, 0..60).contains("←→ table"));
            }
            picker.key(KeyCode::Enter);
            picker.key(KeyCode::End);
        }
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        t.draw(|f| picker.draw(f, f.area())).unwrap();
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(picker.mouse(click(10, 5)), ColumnAction::Stay));
        assert_eq!(picker.current().at, 0);
        assert!(picker.current().default, "clicking the name only focuses");
        assert!(matches!(picker.mouse(click(3, 5)), ColumnAction::Save(_)));
        assert_eq!(picker.current().at, 0);
        assert!(!picker.current().shown.contains(picker.current().selected()));
        t.draw(|f| picker.draw(f, f.area())).unwrap();
        assert_eq!(t.backend().buffer().cell((0, 5)).unwrap().symbol(), "›");
        assert_eq!(cells(&t, 5, 2..5), "[ ]");
        assert_eq!(t.backend().buffer().cell((0, 5)).unwrap().fg, ORANGE);
        picker.mouse(click(14, 1));
        assert_eq!(picker.tab, 1, "the runs tab is clickable");
        assert!(picker.tabs);
        t.draw(|f| picker.draw(f, f.area())).unwrap();
        assert_eq!(t.backend().buffer().cell((12, 1)).unwrap().bg, ORANGE);
        assert_ne!(t.backend().buffer().cell((0, 5)).unwrap().symbol(), "›");
        picker.mouse(click(10, 6));
        assert!(!picker.tabs, "clicking a column returns focus to its row");
        assert_eq!(picker.current().at, 1);
        assert!(picker.current().default);
        picker.mouse(click(1, 1));
        assert_eq!(picker.tab, 0, "the first button has no arrow prefix");
        assert!(picker.tabs);
        assert!(matches!(picker.mouse(click(3, 5)), ColumnAction::Save(_)));
        assert!(!picker.tabs, "a checkbox click both focuses and toggles");
        assert!(picker.current().shown.contains(picker.current().selected()));
    }

    #[test]
    fn columns_picker_mouse_focus_and_table_height_survive_toggles_in_both_layouts() {
        let d = dir();
        let mut app = app(d.path());
        for split in [true, false] {
            fs::write(
                &app.jobs_path,
                "version: 3\ncolumns: [context, model]\njobs: []\n",
            )
            .unwrap();
            app.refresh().unwrap();
            app.split = split;
            app.open_columns(None);
            let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 18)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            assert!(app.wants_mouse());
            let area = match &app.mode {
                Mode::Columns(f) => f.area,
                _ => unreachable!(),
            };
            let click = |x| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x + x,
                row: area.y + 6,
                modifiers: KeyModifiers::NONE,
            };
            app.mouse(click(10));
            assert!(matches!(&app.mode, Mode::Columns(f) if f.current().selected() == "model"));
            assert_eq!(config::columns(&app.jobs_path), ["context", "model"]);
            app.mouse(click(3));
            assert_eq!(config::columns(&app.jobs_path), ["context"]);
            assert!(matches!(&app.mode, Mode::Columns(f) if f.current().selected() == "model"));
            for at in 0..config::COLUMNS.len() {
                if let Mode::Columns(f) = &mut app.mode {
                    f.sets[0].at = at;
                }
                t.draw(|f| app.draw(f)).unwrap();
                let row = |app: &App| match &app.mode {
                    Mode::Columns(f) => {
                        f.area.y + f.header_rows() + (f.current().at - f.top) as u16
                    }
                    _ => unreachable!(),
                };
                let before = row(&app);
                app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
                t.draw(|f| app.draw(f)).unwrap();
                assert_eq!(row(&app), before, "toggle moves row {at}, split={split}");
            }
            app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
            assert!(matches!(app.mode, Mode::Normal));
        }
    }

    #[test]
    fn run_column_picker_saves_reorders_and_resets_without_changing_session_columns() {
        let d = dir();
        fs::write(
            d.path().join("none.yaml"),
            "version: 3\ncolumns: [state, model]\nrun_columns: [model, context]\njobs: []\n",
        )
        .unwrap();
        let mut app = app(d.path());
        app.open_columns(None);
        let go = |app: &mut App, name: &str| {
            if let Mode::Columns(f) = &mut app.mode {
                f.tab = COLUMN_SETS
                    .iter()
                    .position(|(key, _)| *key == name)
                    .unwrap();
            }
        };
        go(&mut app, "run_columns");
        if let Mode::Columns(f) = &app.mode {
            assert!(f.sets[0].order.iter().any(|c| c == "activity"));
            assert!(!f.sets[0].order.iter().any(|c| c == "trigger"));
            assert!(f.sets[1].order.iter().any(|c| c == "trigger"));
            assert!(!f.sets[1].order.iter().any(|c| c == "activity"));
        }
        app.key(KeyCode::Char(']'), KeyModifiers::NONE).unwrap();
        assert_eq!(config::run_columns(&app.jobs_path), ["context", "model"]);
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert!(config::run_columns(&app.jobs_path).is_empty());
        assert!(app.data.run_columns.is_empty());
        go(&mut app, "columns");
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert!(
            config::run_columns(&app.jobs_path).is_empty(),
            "saving another field preserves an empty run set"
        );
        go(&mut app, "run_columns");
        app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert_eq!(
            config::run_columns(&app.jobs_path),
            config::DEFAULT_RUN_COLUMNS
        );
        assert_eq!(app.data.run_columns, config::DEFAULT_RUN_COLUMNS);
        assert_eq!(config::columns(&app.jobs_path), ["model"]);
        assert_eq!(config::file_run_columns(&app.jobs_path), None);
    }

    #[test]
    fn agent_folder_follows_grouping_and_reply_keeps_its_meaning() {
        let d = dir();
        let mut app = app(d.path());
        let mut live = session(A, "idle", "fixture title", 30);
        live.cwd = PathBuf::from("/fixture/folder");
        live.last = Some("reply survives grouping".into());
        app.data.sessions = vec![live];
        app.split = true;
        app.size = (30, 200);
        app.rebuild();
        let header = |app: &App| {
            app.rows
                .iter()
                .find(|r| r.kind == Kind::Columns)
                .unwrap()
                .text()
        };
        let row = |app: &App| {
            app.rows
                .iter()
                .find(|r| matches!(r.kind, Kind::Session(..)))
                .unwrap()
                .text()
        };
        assert!(
            !header(&app).contains("folder"),
            "the folder heading already identifies the group"
        );
        assert!(
            !header(&app).contains("last reply"),
            "the default avoids duplicating the preview"
        );
        assert!(
            !row(&app).contains("claude"),
            "the icon identifies the harness by default"
        );
        app.by_state = true;
        app.rebuild();
        assert!(header(&app).contains("folder"));
        assert!(row(&app).contains("/fixture/folder"));
        app.split = false;
        app.rebuild();
        assert!(header(&app).contains("last reply"));
        assert!(row(&app).contains("reply survives grouping"));
        app.split = true;
        app.data.columns_default = false;
        app.rebuild();
        assert!(
            row(&app).contains("reply survives grouping"),
            "an explicit last reply column stays visible beside the pane"
        );
    }

    #[test]
    fn job_and_history_pickers_reorder_hide_and_reset_independently() {
        let d = dir();
        fs::write(d.path().join("none.yaml"), "version: 3\ncolumns: [state]\nrun_columns: [cost]\njob_columns: [schedule, next_run]\nhistory_columns: [folder, last_active]\njobs: []\n").unwrap();
        let mut app = app(d.path());
        app.open_columns(None);
        for (key, first, second) in [
            ("job_columns", "schedule", "next_run"),
            ("history_columns", "folder", "last_active"),
        ] {
            if let Mode::Columns(form) = &mut app.mode {
                form.tab = COLUMN_SETS
                    .iter()
                    .position(|(name, _)| *name == key)
                    .unwrap();
            }
            app.key(KeyCode::Char(']'), KeyModifiers::NONE).unwrap();
            let saved = || {
                if key == "job_columns" {
                    config::job_columns(&app.jobs_path)
                } else {
                    config::history_columns(&app.jobs_path)
                }
            };
            assert_eq!(saved(), [second, first]);
            app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
            app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
            app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
            assert!(if key == "job_columns" {
                app.data.job_columns.is_empty()
            } else {
                app.data.history_columns.is_empty()
            });
            app.key(KeyCode::Backspace, KeyModifiers::NONE).unwrap();
            assert_eq!(
                if key == "job_columns" {
                    &app.data.job_columns
                } else {
                    &app.data.history_columns
                },
                &built_column_set(key)
            );
        }
        assert_eq!(config::columns(&app.jobs_path), ["state"]);
        assert_eq!(config::run_columns(&app.jobs_path), ["cost"]);
        assert_eq!(config::file_job_columns(&app.jobs_path), None);
        assert_eq!(config::file_history_columns(&app.jobs_path), None);
    }

    #[test]
    fn history_headers_use_their_own_columns_and_latest_activity() {
        let (_d, mut app, mut terminal) = history_fixture(2);
        app.data.columns = vec!["state".into(), "activity".into()];
        app.data.history_columns = vec!["folder".into(), "last_active".into()];
        app.toggle_history();
        history_until(&mut app, &mut terminal, |a| a.history.ready);
        let table = app.history.table(&app.data, &HashSet::new());
        let header = table
            .iter()
            .find(|r| r.kind == Kind::Columns)
            .unwrap()
            .text();
        assert!(header.contains("folder") && header.contains("last active"));
        assert!(!header.contains("state") && !header.contains("activity"));
        let entry = &app.history.rows[0].entry;
        let row = table
            .iter()
            .find(|r| matches!(r.kind, Kind::History(_)))
            .unwrap();
        assert_eq!(row.cells[3].0.trim(), fleet::tilde(&entry.cwd));
        assert_eq!(
            row.cells[4].0.trim(),
            fleet::age(entry.last_activity.unwrap())
        );
    }

    #[test]
    fn branch_reads_identify_worktrees_and_detached_checkouts() {
        let d = dir();
        let repo = d.path().join("repo");
        let worktree = d.path().join("worktree");
        fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&repo, &["init", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        assert_eq!(git_branch(&repo).as_deref(), Some("main"));
        assert_eq!(git_branch(&worktree).as_deref(), Some("feature"));
        git(&worktree, &["checkout", "--detach"]);
        assert!(git_branch(&worktree).unwrap().starts_with('@'));
        assert_eq!(git_branch(d.path()), None);
    }

    #[test]
    fn the_columns_picker_arranges_the_table_and_keeps_the_order_in_jobs_yaml() {
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
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
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
            cost_info: None,
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
            screen[29]
                .trim_end()
                .ends_with("ctrl+z back · ctrl+\\ split"),
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
            harness: Some(HarnessKind::Claude),
            viewer: Viewer::spawn(c, 12, 80, None, viewer::Colors::default()).unwrap(),
            record: None,
            recorded: false,
            first_paint_logged: false,
            last_focused: Instant::now(),
            speculative: false,
            operation: None,
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
            operation: None,
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
            operation: None,
            context: json!({}),
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

    /// A thread the daemon holds is joined, not started, so peek pre-opens it like an attach.
    /// A Codex client in its own terminal has no thread to join, and a saved row whose daemon is
    /// gone waits for enter rather than starting one on a hover.
    #[test]
    fn a_rested_codex_thread_row_is_a_prespawn_target_and_its_own_terminal_is_not() {
        let d = dir();
        let home = d.path().join("codex");
        let rollout = home.join("sessions/2026/09/17/rollout.jsonl");
        let pid_file = home.join("app-server-daemon/app-server.pid");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
        fs::write(
            &pid_file,
            serde_json::json!({"pid": std::process::id()}).to_string(),
        )
        .unwrap();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let mut data = Data::load(&d.path().join("none.yaml"), d.path(), d.path()).unwrap();
        data.sessions.push(Session {
            session_id: B.into(),
            harness: "codex".into(),
            kind: Some("daemon".into()),
            cwd: PathBuf::from("/src/two"),
            state: "working".into(),
            started: None,
            last_activity: None,
            model: None,
            pid: None,
            transcript_path: Some(rollout),
            tokens_in: None,
            tokens_out: None,
            context_tokens: None,
            context_window: None,
            cost_usd: None,
            cost_info: None,
            title: None,
            last: None,
            coordinator: false,
            activity: Vec::new(),
        });
        app.apply(data);
        app.settle();
        assert_eq!(key(&app).as_deref(), Some(B));
        rested(&mut app, B, OLD);
        assert_eq!(
            app.prespawn_target(),
            Some((B.to_owned(), PathBuf::from("/src/two")))
        );
        fs::remove_file(&pid_file).unwrap();
        assert_eq!(
            app.prespawn_target(),
            None,
            "a hover never starts the daemon a resume needs"
        );
        fs::write(
            &pid_file,
            serde_json::json!({"pid": std::process::id()}).to_string(),
        )
        .unwrap();
        app.data.sessions[0].kind = None;
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
        app.data.sessions.push(placeholder(
            HarnessKind::Claude,
            "four",
            d.path(),
            "fixture",
        ));
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
        codex.harness = Some(HarnessKind::Codex);
        codex.last_focused = Instant::now() - Duration::from_secs(60);
        app.viewers.push(codex);
        for k in ["one", "two"] {
            app.viewers.push(silent_open(k));
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        app.data.sessions.push(placeholder(
            HarnessKind::Claude,
            "four",
            d.path(),
            "fixture",
        ));
        app.open((12, 80), c, "attach", "four".into(), None);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["codex-1", "two", "four"], "{keys:?}");
        // Two more Codex clients take the attaches' places, then an attach with no attach left
        // to close opens as a fourth live viewer.
        for k in ["codex-2", "codex-3", "five"] {
            let mut c = Command::new("/bin/sleep");
            c.arg("5");
            let what = if k == "five" { "attach" } else { "codex" };
            let kind = if k == "five" {
                HarnessKind::Claude
            } else {
                HarnessKind::Codex
            };
            app.data
                .sessions
                .push(placeholder(kind, k, d.path(), "fixture"));
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
