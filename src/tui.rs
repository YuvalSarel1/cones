//! `cones tui` is the native dashboard: every live harness session grouped by directory or by
//! state, and runs, with a composer at the bottom like `claude agents`: type an instruction,
//! `enter` starts a session in the selected row's directory under the harness `tab` picked.
//! Jobs have a screen of their own behind the menu's `jobs` button, where they are started,
//! added (the `new job` row), edited (`ctrl+e`) and deleted (`ctrl+x`); `esc` comes back.
//! `ctrl+x` marks the row red and a second press acts; any other key keeps it, and so does
//! `confirm_secs` seconds of no key (jobs.yaml, 2 by default). On a finished run it hides the row
//! here for good; the ledger keeps it.
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
/// every mode a child of an earlier build may have left on: focus events, color-scheme
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
/// A second ctrl+c within this window quits the dashboard, as in Claude Code.
const QUIT_CONFIRM: Duration = Duration::from_millis(1500);
/// What a first ctrl+c says, while the composer's rules go red with it.
const QUIT_HINT: &str = "ctrl+c again quits · any other key stays";
/// A working row's icon: a bar that fills and empties, the same family as the sparkline and
/// the resting `▁`, holding two extra frames full and two empty so the turn reads as a breath
/// rather than a flicker. Full is `▇`, never `█`: the full block touches the row above and the
/// bar reads as part of it. One animation for every harness; until 2026-09-15 each harness spun
/// its own mark, and Claude's star spent a third of its cycle as a dot, so a working row read
/// as less than an idle one.
const SPINNER: [&str; 16] = [
    "▁", "▁", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "▇", "▇", "▆", "▅", "▄", "▃", "▂",
];
/// Milliseconds per spinner frame; the draw loop ticks every 100.
const FRAME_MS: usize = 160;
/// The strip's mark beside the word cones. Still: the mascot does not animate.
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
    /// The top menu row, its buttons in `MENU`, the picked one in `App::menu`. From
    /// `App::rebuild`, never from `Data::rows`.
    Menu,
    /// A pinned folder nothing runs in, in `~` form: its group's one row until a session
    /// starts there or ctrl+x removes the folder.
    Folder(String),
    /// The jobs screen's last row: enter opens the wizard on a new job.
    NewJob,
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
            Kind::Menu => Some("menu"),
            Kind::Folder(dir) => Some(dir),
            Kind::NewJob => Some("new job"),
            _ => None,
        }
    }
}

/// One row of the sessions table: a job in its folder's group, or a session.
enum Entry<'a> {
    Job(&'a ResolvedJob),
    Session(&'a Session),
    /// A pinned folder nothing runs in: its group's one row.
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

/// Everything the dashboard shows, loaded in one pass.
pub struct Data {
    pub jobs: Vec<ResolvedJob>,
    pub runs: Vec<Run>,
    pub sessions: Vec<Session>,
    /// Session column names after the harness and title, from jobs.yaml's `columns:`.
    pub columns: Vec<String>,
    /// The viewer pane's layout, from jobs.yaml.
    pub pane: config::Pane,
    /// What a new cones terminal comes up with, from jobs.yaml: the composer's harness and
    /// whether the pane is open. Read at startup and never again.
    pub start: config::Start,
    /// The `sparkline` column's window, metric and bound, from jobs.yaml.
    pub spark: config::Sparkline,
    /// Seconds an armed `ctrl+x` mark stays with no key pressed; 0 keeps it until a key.
    pub confirm_secs: f64,
    /// Folders the menu's `folder` prompt picked, kept as rows while nothing runs there.
    pub folders: Vec<PathBuf>,
    /// Folders a session has been seen in, newest first: what the `folder` prompt recalls.
    pub recent: Vec<PathBuf>,
    /// The git branch and tree state of each pinned folder nothing runs in, for its row.
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
        // Only the folders that get a row: a folder a session or a job is in shows those.
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
            spark: config::sparkline(jobs_path),
            confirm_secs: config::confirm_secs(jobs_path),
            folders,
            recent: ledger.recent(&seen)?,
            git,
        })
    }

    /// Whether `dir` has a group of its own already: a session runs there.
    fn has_rows_in(&self, dir: &Path) -> bool {
        self.sessions.iter().any(|s| s.cwd == dir)
    }

    fn count(&self, state: &str) -> usize {
        self.sessions.iter().filter(|s| s.state == state).count()
    }

    /// The fleet in one line: the state's icon and count per state, each in the state's color,
    /// then the jobs and runs. A count of zero goes dim so the live numbers stand out. The
    /// working icon is the spinner's `frame`, so it moves with the rows while anything works.
    pub fn summary(&self, frame: usize) -> Line<'static> {
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

    /// Sessions grouped by directory like Claude's own agents view, or by state so the row that
    /// needs a human is on top.
    pub fn rows(&self, by_state: bool) -> Vec<Row> {
        self.rows_excluding(by_state, false, &HashSet::new(), &mut Widths::new())
    }

    /// A confirmed delete leaves the list immediately while the harness command finishes.
    /// The source data stays intact so a failed command can restore its row. `jobs_view` is
    /// the jobs screen: the jobs alone, grouped as one, and no session, folder or run.
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
        // A group's key sorts it and names it: folders by name with case set aside, shown in
        // `~` form; grouped by state a rank digit leads, needs input first.
        let folder = |dir: &Path| {
            let name = if dir.as_os_str().is_empty() {
                "no directory".to_owned()
            } else {
                fleet::tilde(dir)
            };
            (name.to_lowercase(), name)
        };
        let ranked = |rank: u8, name: &str| (format!("{rank}{name}"), name.to_owned());
        // On the jobs screen the jobs are one group in jobs.yaml order; the main screen has
        // the sessions and the pinned folders.
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
        // A pinned folder sorts among the live folders by name; grouped by state it follows the
        // jobs. The same key as the sessions use, so it joins its group rather than doubling it.
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
        // The state column sits before the title, where the eye lands after the icon, when
        // the column set lists it; the other columns follow the title in their order. The
        // jobs screen has the columns a job can fill, whatever the set says for sessions.
        let job_columns = ["model".to_owned(), "activity".to_owned(), "last".to_owned()];
        // One set whatever the width, so a column never moves: the pane opening or a narrow
        // terminal cuts the columns off the right edge, the rest stay where they were.
        let set = &self.columns;
        let has_state = jobs_view || set.iter().any(|c| c == "state");
        let cols: Vec<&String> = if jobs_view {
            job_columns.iter().collect()
        } else {
            set.iter().filter(|c| *c != "state").collect()
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
                        (logo(&s.harness), brand(&s.harness)),
                        if has_state {
                            cell("state", s, by_state, None)
                        } else {
                            // The same words as the footer, on the row, so a session that
                            // cannot be joined from here is known before it is selected. The
                            // folder's orchestrator says so here and carries its title in cones'
                            // orange, so it is told from the workers at a glance.
                            (
                                [
                                    s.coordinator.then_some("orchestrator"),
                                    s.own_terminal().then_some("own terminal"),
                                ]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>()
                                .join(" · "),
                                if s.coordinator { lit() } else { dim() },
                            )
                        },
                        (
                            // A long title would push every metric column off a 120-column screen.
                            clip(
                                &s.title
                                    .clone()
                                    .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
                                40,
                            ),
                            if s.coordinator { lit() } else { plain() },
                        ),
                    ];
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
                        (logo(&j.harness.to_string()), brand(&j.harness.to_string())),
                        // The last run's status, or `off`, takes the state slot with the
                        // schedule beside it.
                        if has_state {
                            let (word, style) = job_cell("state", j, last, &status, by_state);
                            (format!("{word} · {}", j.schedule), style)
                        } else {
                            (format!("job · {}", j.schedule), dim())
                        },
                        (j.name.clone(), plain()),
                    ];
                    row.extend(cols.iter().map(|c| job_cell(c, j, last, &status, by_state)));
                    row
                }
            })
            .collect();
        let spark_title = self.spark.title();
        let mut names = vec!["", "", if has_state { "state" } else { "" }, "title"];
        names.extend(cols.iter().map(|c| match c.as_str() {
            "tokens" => "tokens in/out",
            "last" if by_state => "dir",
            "sparkline" => spark_title.as_str(),
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
                        "timeout {:.0}m · budget ${:.2} · write {} · overlap {:?}",
                        j.timeout_min,
                        j.budget_usd,
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
                // Model, start, last activity and context are the harness's words, `-` when it
                // has none; the window shows only when the harness stated one.
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
/// Three header lines carry Little feet and the bordered summary.
pub fn list(jobs_path: &Path, state: &Path, claude: &Path) -> Result<String> {
    let data = Data::load(jobs_path, state, claude)?;
    let mut out = String::new();
    let summary = data.summary(0);
    let width = summary.width() + 16;
    let folder = std::env::current_dir().map_or_else(|_| String::new(), |p| fleet::tilde(&p));
    for line in header_lines(summary, &folder, width) {
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

/// The top menu's buttons: name, what `enter` does on it, and the explanation shown beside it
/// while it is picked.
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

/// What `enter` does to the selected row: start a job, follow a headless run, attach a session;
/// on the menu row, press button `menu`.
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

/// The top menu: one row of buttons above the tables, reached with `↑` past the first table;
/// `←` `→` pick one and `enter` presses it. `folder` adds a row for a directory nothing runs
/// in, so work can start there, `jobs` opens the jobs screen, `config` edits the defaults,
/// `help` opens the guide.
fn menu_rows() -> Vec<Row> {
    // A blank row keeps the menu off the cone. The row's cells come from `App::menu_cells`
    // at draw time, since the picked button and its explanation change without a rebuild.
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

/// The mascot beside a three-row frame. Keep the right border visible when counts are clipped.
fn header_lines(summary: Line<'static>, folder: &str, width: usize) -> Vec<Line<'static>> {
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
    let inner = width - 14;
    let folder = fit(
        vec![Span::styled(format!(" {folder} "), dim())],
        inner.saturating_sub(11),
    );
    let folder_width: usize = folder.iter().map(Span::width).sum();
    let mut top = vec![
        Span::styled("── ", dim()),
        Span::styled("cones ", lit()),
        Span::styled("─".repeat(inner - 9 - folder_width), dim()),
    ];
    top.extend(folder);
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

/// The usage guide as a paragraph, keys in one column and their verbs dim beside them, from
/// wrapped line `top`.
fn guide(top: usize) -> Paragraph<'static> {
    let width = GUIDE
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![];
    for (key, what) in GUIDE {
        if key.is_empty() {
            // A blank line above every heading; the first keeps the guide off the cone, as the
            // list's blank row does.
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                (*what).to_owned(),
                Style::default().fg(ORANGE),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:width$}  "), bold()),
                Span::styled((*what).to_owned(), dim()),
            ]));
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((top as u16, 0))
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
/// A job's cell under a session column: the last run's status where a session shows its state,
/// its model, how long since the last run fired, its directory when grouped by state; the
/// columns that are a session's alone stay blank.
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
        "model" => (j.model.clone().unwrap_or_else(|| "-".into()), dim()),
        "age" | "activity" => (
            last.and_then(|r| r.started.fired_at)
                .map_or_else(|| "-".into(), fleet::age),
            dim(),
        ),
        "last" if by_state => (fleet::tilde(&j.cwd), dim()),
        _ => (String::new(), dim()),
    }
}

/// A session's cell under `column`; `spark` is its sparkline, drawn once for the whole fleet so
/// every row shares one bound.
fn cell(column: &str, s: &Session, by_state: bool, spark: Option<&str>) -> (String, Style) {
    let since = |t: Option<chrono::DateTime<chrono::Utc>>| t.map_or_else(|| "-".into(), fleet::age);
    match column {
        "state" => (label(&s.state).into(), color(&s.state)),
        "sparkline" => {
            let bars = spark.unwrap_or_default().to_owned();
            let quiet = bars.chars().all(|c| c == '▁');
            (bars, if quiet { dim() } else { plain() })
        }
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
/// A menu button at rest: white on a dark fill, as a terminal draws a key cap.
fn button() -> Style {
    Style::default().bg(Color::Indexed(237)).fg(Color::White)
}
/// The picked menu button, lit in the cone's orange.
fn pressed() -> Style {
    Style::default().bg(ORANGE).fg(Color::Black)
}
/// cones' own orange, bold: the header cone and the folder's orchestrator.
fn lit() -> Style {
    Style::default().fg(ORANGE).add_modifier(Modifier::BOLD)
}

/// What is typed with a block cursor on the character at `cursor`, a byte offset, or after
/// the text when it is at the end; or the placeholder with the cursor on its first letter: how
/// Claude Code draws its own input.
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

/// `at`, a byte offset into `text` that may be stale, brought back inside it and onto a
/// character boundary.
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

/// Readline's editing of one line, for the composer. macOS terminals send the shortcuts their
/// users press in the encodings below: VS Code, iTerm2 with natural text editing and Ghostty
/// turn cmd+left and cmd+right into ctrl+a and ctrl+e, cmd+delete into ctrl+u, option+delete
/// into ctrl+w or alt+backspace and option+left/right into alt+b/alt+f or alt+arrows; cmd
/// itself never reaches a terminal program. Returns the cursor after the key, `None` when
/// the key is not an edit.
fn edit(text: &mut String, cursor: usize, code: KeyCode, mods: KeyModifiers) -> Option<usize> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let at = snap(text, cursor);
    // ponytail: a word is a run of non-spaces, for every word key alike.
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

/// A line being typed and the cursor in it, a byte offset. Every prompt the dashboard reads
/// from the keyboard is one, so `edit`'s keys and `tab` on a path work the same in all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub text: String,
    pub at: usize,
}

impl Input {
    /// `text` with the cursor after it.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            at: text.len(),
            text,
        }
    }

    /// An edit key applied where the cursor is; false for any other key.
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match edit(&mut self.text, self.at, code, mods) {
            Some(at) => {
                self.at = at;
                true
            }
            None => false,
        }
    }

    /// `tab` on a path: the text grown as `complete_dir` grows it, the cursor after it; or,
    /// when nothing grew, the names that still match, for the hint line.
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

/// A pasted image sits in the instruction as one private-use character, `IMAGE` plus its index
/// into `App::images`, as Claude Code's `[Image #n]`: drawn as that label, deleted as one
/// character by any edit key, and expanded to the PNG's path at launch.
const IMAGE: u32 = 0xE000;

fn image_marker(n: usize) -> char {
    char::from_u32(IMAGE + n as u32).expect("private-use range")
}

fn image_index(c: char) -> Option<usize> {
    (IMAGE..IMAGE + 0x100)
        .contains(&(c as u32))
        .then(|| (c as u32 - IMAGE) as usize)
}

/// Splice image `n`'s marker into the instruction at `at`, spaced from what is typed either
/// side; returns the cursor after it.
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

/// The instruction with each image marker replaced by `f` of its index: the label on screen,
/// the path at launch.
fn expand(text: &str, mut f: impl FnMut(usize) -> String) -> String {
    text.chars()
        .map(|c| image_index(c).map_or_else(|| c.to_string(), &mut f))
        .collect()
}

/// One glyph per state, one family: a bar. Working fills and empties (`SPINNER` draws it), a
/// still full bar in yellow wants a human, the lowest bar is resting or stopped, dim, with the
/// word telling the two apart. Finished work keeps `✓` and `✗`, as in `claude agents`. `-` is
/// a session whose harness reported no state, a Codex before its first turn; not a failure.
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

/// Which harness a session or job runs under, with the mark each app draws for itself: Claude's
/// ✻, Codex's `>_` startup box title, pi's π window title. Still, in the harness's color; the
/// state icon carries the motion.
fn logo(harness: &str) -> String {
    match harness {
        "claude" => "✻ claude".into(),
        "codex" => ">_ codex".into(),
        "pi" => "π pi".into(),
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

/// Keep `spans` within `width` columns by cutting from the right end, so what is cut is the
/// tail of the last span that fits in part and everything after it.
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
    // A thread ctrl+x forgot stays gone, even while another client has the daemon holding it
    // again; its id is a line in `hidden`, like a hidden run.
    let hidden = Ledger::new(state)?.hidden()?;
    out.retain(|s| !hidden.contains(&s.session_id));
    fleet::sort(&mut out);
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

/// A folder's git state for a row nothing else fills: the branch and whether the tree is
/// clean, from one `git status --porcelain --branch`; None outside a repository or without git.
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

/// Where the wizard is: one question at a time, every answer so far kept on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    What,
    Where,
    When,
    At,
    Name,
}

/// What a key in the wizard asks the dashboard to do.
#[derive(Debug, PartialEq)]
pub enum FormAction {
    Stay,
    Cancel,
    /// Run the task once, supervised, in the directory: `cones run --prompt` there.
    RunOnce(String, PathBuf),
    /// Write the job, replacing the one with this name when editing.
    Save(Option<String>, Box<config::Job>),
}

/// The `when` options: `once` runs now, the rest schedule a job; `cron` takes five fields.
const WHEN: [&str; 6] = ["once", "hourly", "daily", "weekdays", "weekly", "cron"];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// A row of options with the picked one lit and bracketed.
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

/// The `when` pick and the `at` answer a schedule comes from, so an edit opens on the same
/// options that made it: `0 9 * * *` is daily at 09:00. Anything else is `cron` as written.
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

/// The five-field schedule for a `when` pick and its `at` answer; the error is the one line
/// the wizard shows inline. `cron` is checked the way `cones install` checks it.
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

/// A job name from the task's first words: `Read the TODOs!` becomes `read-the-todos`.
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

/// The wizard the menu's `runs` button opens (`ctrl+e` on a job row edits): the task, where
/// it runs, how often, at what time, and the job's name, one question at a time where the
/// list is, with every answer so far above the current one. `enter` answers, `← →` pick an
/// option, `↑` or backspace on an empty answer steps back, `esc` cancels. `once` runs the
/// task now instead of writing a job. Editing keeps every field the wizard does not ask
/// about (model, budget, write). Pure: filesystem facts come in through `base`, `fallback`
/// and `launch_dir`; the file is written by the dashboard on `Save`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobForm {
    pub step: Step,
    pub prompt: String,
    pub dir: String,
    /// An index into `WHEN`.
    pub when: usize,
    pub at: String,
    pub name: String,
    pub error: Option<String>,
    /// The cursor in the answer being typed, a byte offset; past the end means after it.
    cursor: usize,
    schedule: String,
    /// The job being edited, as written in the file; `None` adds one.
    original: Option<config::Job>,
    base: PathBuf,
    fallback: PathBuf,
}

impl JobForm {
    /// `base` is where a relative directory is taken from, the jobs file's; `fallback` is what
    /// an empty directory means and is shown as the placeholder; `seed` is what the composer
    /// held, the task's first draft.
    pub fn new(base: &Path, fallback: &Path, original: Option<config::Job>, seed: &str) -> Self {
        let (name, dir, prompt, (when, at)) = match &original {
            Some(j) => (
                j.name.clone(),
                j.cwd.display().to_string(),
                j.prompt.clone(),
                from_cron(&j.schedule),
            ),
            None => Default::default(),
        };
        Self {
            step: Step::What,
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
            schedule: String::new(),
            original,
            base: base.to_owned(),
            fallback: fallback.to_owned(),
        }
    }

    /// Move to `step`, the cursor after its answer.
    fn go(&mut self, step: Step) {
        self.step = step;
        self.cursor = usize::MAX;
    }

    /// `tab` on `where`: the directory completed as the folder prompt's is, from the jobs
    /// file's directory, where a relative answer is taken from.
    pub fn complete(&mut self) -> Vec<String> {
        let mut input = Input::new(std::mem::take(&mut self.dir));
        let names = input.complete(&self.base);
        self.dir = input.text;
        self.cursor = usize::MAX;
        names
    }

    /// The answer being typed; `when` is picked, not typed.
    fn field(&mut self) -> Option<&mut String> {
        match self.step {
            Step::What => Some(&mut self.prompt),
            Step::Where => Some(&mut self.dir),
            Step::When => None,
            Step::At => Some(&mut self.at),
            Step::Name => Some(&mut self.name),
        }
    }

    /// Whether the pick needs a time: hourly and once do not.
    fn asks_at(&self) -> bool {
        self.when >= 2
    }

    /// What an empty `at` means, and its placeholder.
    fn at_placeholder(&self) -> &'static str {
        match WHEN[self.when] {
            "weekly" => "mon 09:00",
            "cron" => "0 9 * * 1-5",
            _ => "09:00",
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> FormAction {
        if code == KeyCode::Esc {
            return FormAction::Cancel;
        }
        self.error = None;
        let cursor = self.cursor;
        match code {
            KeyCode::Enter => return self.next(),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab if self.step == Step::When => {
                let n = WHEN.len();
                self.when = (self.when + if code == KeyCode::Left { n - 1 } else { 1 }) % n;
                self.at.clear();
            }
            KeyCode::Up => self.back(),
            KeyCode::Backspace if self.field().is_none_or(|f| f.is_empty()) => self.back(),
            _ => {
                if let Some(f) = self.field()
                    && let Some(at) = edit(f, cursor, code, mods)
                {
                    self.cursor = at;
                }
            }
        }
        FormAction::Stay
    }

    fn back(&mut self) {
        self.go(match self.step {
            Step::What | Step::Where => Step::What,
            Step::When => Step::Where,
            Step::At => Step::When,
            Step::Name if self.asks_at() => Step::At,
            Step::Name => Step::When,
        });
    }

    /// Check the answer; move on, or at the last question hand the job over. The checks are the
    /// file's own, so what passes here passes `cones install`.
    fn next(&mut self) -> FormAction {
        match self.step {
            Step::What => {
                if self.prompt.trim().is_empty() {
                    self.error = Some("the task cannot be empty".into());
                } else {
                    self.go(Step::Where);
                }
            }
            Step::Where => match launch_dir(&self.dir, &self.base, &self.fallback) {
                Ok(dir) => {
                    self.dir = fleet::tilde(&dir);
                    self.go(Step::When);
                }
                Err(e) => self.error = Some(e),
            },
            Step::When => {
                if WHEN[self.when] == "once" {
                    return match launch_dir(&self.dir, &self.base, &self.fallback) {
                        Ok(dir) => FormAction::RunOnce(self.prompt.trim().to_owned(), dir),
                        Err(e) => {
                            self.error = Some(e);
                            FormAction::Stay
                        }
                    };
                }
                if self.asks_at() {
                    self.go(Step::At);
                } else {
                    self.schedule = to_cron(self.when, "").unwrap_or_default();
                    self.go(Step::Name);
                }
            }
            Step::At => {
                if self.at.trim().is_empty() {
                    self.at = self.at_placeholder().to_owned();
                }
                match to_cron(self.when, &self.at) {
                    Ok(s) => {
                        self.schedule = s;
                        self.go(Step::Name);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Step::Name => {
                let ok = !self.name.is_empty()
                    && self.name.len() <= 80
                    && self
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                if !ok {
                    self.error = Some("1-80 letters, digits, - or _".into());
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
        // The name is suggested from the task the first time it is asked.
        if self.step == Step::Name && self.name.is_empty() {
            self.name = slug(&self.prompt);
        }
        FormAction::Stay
    }

    /// The wizard where the list is: a title, then every question the pick calls for, the
    /// answered ones with their answers, the current one with the cursor or the options, the
    /// ones to come dim with what an empty answer would mean.
    fn lines(&self) -> Vec<Line<'static>> {
        let title = match &self.original {
            Some(j) => format!("edit {}", j.name),
            None => "new job".to_owned(),
        };
        let mut lines = vec![
            Line::default(),
            Line::from(Span::styled(title, Style::default().fg(ORANGE))),
            Line::default(),
        ];
        let fallback = fleet::tilde(&self.fallback);
        let once = WHEN[self.when] == "once";
        for step in [Step::What, Step::Where, Step::When, Step::At, Step::Name] {
            if (step == Step::At && !self.asks_at()) || (step > Step::When && once) {
                continue;
            }
            let (label, value, placeholder): (&str, &str, &str) = match step {
                Step::What => ("what", &self.prompt, "the task"),
                Step::Where => ("where", &self.dir, &fallback),
                Step::When => ("when", WHEN[self.when], ""),
                Step::At => ("at", &self.at, self.at_placeholder()),
                Step::Name => ("name", &self.name, "from the task"),
            };
            let style = match step.cmp(&self.step) {
                std::cmp::Ordering::Equal => lit(),
                std::cmp::Ordering::Less => bold(),
                std::cmp::Ordering::Greater => dim(),
            };
            let mut spans = vec![Span::styled(format!("  {label:<6} "), style)];
            if step == Step::When {
                if step == self.step {
                    picks(&mut spans, &WHEN, self.when);
                } else {
                    spans.push(Span::styled(
                        value.to_owned(),
                        style.remove_modifier(Modifier::BOLD),
                    ));
                }
            } else if step == self.step {
                spans.extend(typed(value, self.cursor, placeholder));
            } else if step < self.step || !value.is_empty() {
                spans.push(Span::raw(value.to_owned()));
            } else {
                spans.push(Span::styled(placeholder.to_owned(), dim()));
            }
            if step == self.step
                && let Some(e) = &self.error
            {
                spans.push(Span::styled(
                    format!("  {e}"),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    /// The prompt line: the current question and what an answer looks like.
    fn line(&self) -> Line<'static> {
        let (what, help) = match self.step {
            Step::What => ("what", "the task, as you would type it to the harness"),
            Step::Where => (
                "where",
                "a folder; empty takes the one shown, tab completes",
            ),
            Step::When => (
                "when",
                "once runs it now, supervised and in the ledger; the rest schedule a job",
            ),
            Step::At => (
                "at",
                match WHEN[self.when] {
                    "weekly" => "a day and a local time, as in mon 09:00",
                    "cron" => "minute hour day month weekday, as in 0 9 * * 1-5",
                    _ => "a local time, as in 09:00",
                },
            ),
            Step::Name => (
                "name",
                "the job's name in jobs.yaml and launchd: letters, digits, - or _",
            ),
        };
        Line::from(vec![
            Span::styled(format!("{what} › "), Style::default().fg(ORANGE)),
            Span::styled(help.to_owned(), dim()),
        ])
    }
}

/// A field the config editor shows: the group it sits under, the block inside that group when
/// it has one, its name, the words beside it on its row, the fuller explanation under the list
/// while it is selected, what an empty answer means (the built-in), and how its value is
/// entered.
struct Field {
    group: &'static str,
    /// The dim sub-head above the row, shared by the rows around it; empty sits under the
    /// group's own head.
    sub: &'static str,
    name: &'static str,
    short: &'static str,
    long: &'static str,
    builtin: &'static str,
    input: Answer,
}

/// How a field takes its value: typed; one of a few words, `-` for the built-in; or those
/// words or something typed, named for the help line.
enum Answer {
    Typed,
    Pick(&'static [&'static str]),
    PickOrType(&'static [&'static str], &'static str),
}

impl Field {
    /// The words a field offers, when it offers any.
    fn picks(&self) -> Option<&'static [&'static str]> {
        match self.input {
            Answer::Typed => None,
            Answer::Pick(o) | Answer::PickOrType(o, _) => Some(o),
        }
    }

    /// Whether typing edits the value.
    fn typed(&self) -> bool {
        !matches!(self.input, Answer::Pick(_))
    }

    /// How option `o` reads on the prompt line: `-` of a field the harness owns is
    /// `system default`.
    fn label<'a>(&self, o: &'a str) -> &'a str {
        if o == "-" && self.builtin == SYSTEM {
            SYSTEM
        } else {
            o
        }
    }

    /// Whether `value` is shown as picks: empty, the built-in, or one of the options.
    fn picked(&self, value: &str) -> bool {
        self.picks()
            .is_some_and(|o| value.is_empty() || o.contains(&value))
    }
}

/// The comma-separated items of a list value.
const BOOL: &[&str] = &["-", "false", "true"];

/// The built-in of a field cones leaves to the harness when it is empty: nothing is passed,
/// and the harness's own configuration decides. The `-` pick of such a field reads this.
const SYSTEM: &str = "system default";

/// The groups the editor shows, each with a line on what it holds, in the order a reader
/// meets them: `cones` is every key outside the `defaults:` block, the `columns:` and
/// `confirm_secs:` lines and the `start:`, `pane:` and `sparkline:` blocks; `harnesses` and
/// `runs` are the `defaults:` block itself, how claude and codex are run and what a
/// supervised run may do. A group's rows come first and its blocks after, each block under a
/// dim sub-head named for the block in the file, `start`, `pane`, `sparkline`, or for the
/// harness whose fields it holds.
const GROUPS: [(&str, &str); 3] = [
    ("cones", "the dashboard itself"),
    ("harnesses", "how claude and codex are run"),
    ("runs", "every supervised run"),
];

/// The fields under their groups and blocks. A field named for a harness reaches only that
/// harness. `start.harness` is what the composer comes up on, `runs.harness` what a job that
/// names none runs under: one row each, so neither has to mean both.
const FIELDS: [Field; 22] = [
    Field {
        group: "cones",
        sub: "",
        name: "confirm_secs",
        short: "ctrl+x stays armed (s)",
        long: "Seconds an armed ctrl+x waits for its second press with no key pressed, up to 600. 0 keeps the mark until the next key.",
        builtin: "2",
        input: Answer::Typed,
    },
    Field {
        group: "cones",
        sub: "start",
        name: "start.harness",
        short: "the composer comes up on",
        long: "The harness the composer is on in a new cones terminal; shift+tab and ctrl+o change it from there and cones writes nothing back. Codex sessions start, Codex jobs are still unavailable.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex"]),
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
        short: "where the pane sits",
        long: "right puts the pane beside the list, bottom under it.",
        builtin: "right",
        input: Answer::Pick(&["-", "right", "bottom"]),
    },
    Field {
        group: "cones",
        sub: "sparkline",
        name: "sparkline.bars",
        short: "bar count",
        long: "Number of bars, 1 to 64, oldest first. 16 bars at 1m show the last 16 minutes.",
        builtin: "16",
        input: Answer::Typed,
    },
    Field {
        group: "cones",
        sub: "sparkline",
        name: "sparkline.bucket",
        short: "time per bar",
        long: "Time per bar, such as 30s, 1m or 5m. Maximum 24h.",
        builtin: "1m",
        input: Answer::PickOrType(&["-", "30s", "1m", "5m", "15m", "1h"], "a duration"),
    },
    Field {
        group: "cones",
        sub: "sparkline",
        name: "sparkline.metric",
        short: "count per bar",
        long: "lines: all transcript lines. messages: assistant replies. tools: tool calls. tokens: output tokens.",
        builtin: "lines",
        input: Answer::Pick(&["-", "lines", "messages", "tools", "tokens"]),
    },
    Field {
        group: "cones",
        sub: "sparkline",
        name: "sparkline.bound",
        short: "chart scale",
        long: "fleet: busiest bucket on screen. row: each row's busiest bucket. log: fleet on a log scale. A number sets the count for a full bar.",
        builtin: "fleet",
        input: Answer::PickOrType(&["-", "fleet", "row", "log"], "a number"),
    },
    Field {
        group: "harnesses",
        sub: "",
        name: "bedrock",
        short: "run on Amazon Bedrock",
        long: "true sends Claude and Codex to Amazon Bedrock, false to their own endpoints; system default passes nothing and the harness's own configuration decides. true is refused without the profile and region below, since the switch alone reaches Bedrock with nothing to authenticate it.",
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
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "model",
        short: "alias or model id",
        long: "The alias or model id passed to Claude as --model, for jobs and for sessions the composer starts. system default passes nothing and Claude's own settings decide.",
        builtin: SYSTEM,
        input: Answer::PickOrType(&["-", "fable", "opus", "sonnet", "haiku"], "a model id"),
    },
    Field {
        group: "harnesses",
        sub: "claude",
        name: "max_turns",
        short: "turns per run",
        long: "Maximum assistant turns per Claude run. Empty passes nothing and Claude's own limit stands.",
        builtin: SYSTEM,
        input: Answer::Typed,
    },
    Field {
        group: "harnesses",
        sub: "codex",
        name: "codex_model",
        short: "model id",
        long: "Passed to Codex as -m for sessions the composer starts; on Bedrock the id carries the openai. prefix. Empty passes nothing and Codex's own config decides. Codex jobs are still unavailable.",
        builtin: SYSTEM,
        input: Answer::Typed,
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
        short: "for a job that names none",
        long: "The harness a job runs under when it names no harness of its own. What the composer comes up on is start.harness. Codex jobs are still unavailable.",
        builtin: "claude",
        input: Answer::Pick(&["-", "claude", "codex"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "timeout_min",
        short: "time limit (min)",
        long: "Positive minutes, up to 10080 (one week). cones stops overdue runs and records a timeout.",
        builtin: "30",
        input: Answer::Typed,
    },
    Field {
        group: "runs",
        sub: "",
        name: "budget_usd",
        short: "cost per run (USD)",
        long: "Maximum cost per run in USD, passed to Claude as --max-budget-usd, which stops the run when it is reached. Codex jobs are still unavailable.",
        builtin: "2.00",
        input: Answer::Typed,
    },
    Field {
        group: "runs",
        sub: "",
        name: "daily_budget_usd",
        short: "cost per 24h (USD)",
        long: "Rolling cap per job over 24 hours. Active runs reserve budget_usd; runs that would exceed the cap are skipped. Must be at least budget_usd. Empty means no cap.",
        builtin: "none",
        input: Answer::Typed,
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
        short: "when already running",
        long: "When a job is already running: skip the next run, allow both, or replace the active run.",
        builtin: "skip",
        input: Answer::Pick(&["-", "skip", "allow", "replace"]),
    },
    Field {
        group: "runs",
        sub: "",
        name: "notify",
        short: "failure alerts",
        long: "Notify on failures, timeouts and runs skipped for budget.",
        builtin: "false",
        input: Answer::Pick(BOOL),
    },
];

/// The rows the session form shows: what the next session the composer starts runs on, the
/// same `defaults` fields the editor shows, seeded from the policy that session would take.
/// Nothing on this form is written to the file.
const SESSION: [&str; 6] = [
    "harness",
    "model",
    "codex_model",
    "bedrock",
    "aws_profile",
    "aws_region",
];

/// Where `name` sits in `FIELDS`.
fn field_at(name: &str) -> usize {
    FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap_or_else(|| panic!("no config field {name}"))
}

/// What a key in the config editor asks the dashboard to do.
#[derive(Debug, PartialEq)]
pub enum ConfigAction {
    Stay,
    Cancel,
    /// A field closed on a new value, so the block is written and the editor stays where it
    /// is: the `defaults` block, the `columns:` list, empty for the built-in, the `sparkline:`,
    /// `pane:` and `start:` blocks, None when every field of one is left to the built-in, and
    /// the `confirm_secs:` line.
    Save(
        Box<config::Policy>,
        Vec<String>,
        Option<config::Sparkline>,
        Option<config::Pane>,
        Option<config::Start>,
        Option<f64>,
    ),
}

/// The config editor the menu's `config` button opens: the `defaults` block of jobs.yaml, the
/// policy every job runs under unless it sets the field itself, and the dashboard's `columns:`
/// line and `sparkline:` block, one row per field under its group where the list is. Every
/// row is name, value, a few words, in three columns that stay put. `↑` `↓` move between
/// fields and `enter` opens the selected one: its value is pressed and the prompt line is
/// where it is edited, the options with the current one bracketed where the field is picked,
/// the value with a cursor where it is typed; `← →` pick, typing edits, `enter` keeps the
/// value, saves the block and returns to the list, `esc` puts the old one back. There is
/// nothing to press to keep the block: every field that closes on a value it did not open
/// with writes it, and `esc` on the list only closes the editor. The selected field's fuller
/// explanation sits under the list. An empty answer leaves the field out of the file, so the
/// built-in applies and shows dim in its place. Pure: the file is read and written by the
/// dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigForm {
    pub row: usize,
    /// Each field as typed, in `FIELDS` order; a picked field holds its option's word, empty
    /// for the built-in.
    pub values: Vec<String>,
    pub error: Option<String>,
    /// The selected field is open for editing; `before` is its value when it was opened, put
    /// back by `esc`.
    pub open: bool,
    before: String,
    /// The cursor in the selected value, a byte offset; past the end means after it.
    cursor: usize,
    /// Only the `SESSION` rows are shown and visited, under one title and no group heads:
    /// the form `ctrl+o` opens, seeded from the policy the next session would run under.
    pub session: bool,
    /// The file's `columns:` line, carried through a save rather than edited here: the
    /// columns are arranged on the table itself, with `ctrl+t`.
    columns: Option<Vec<String>>,
}

impl ConfigForm {
    /// The `SESSION` rows alone, as the settings of the next session the composer starts.
    pub fn session(policy: &config::Policy) -> Self {
        let mut form = Self::new(policy, None, None, None, None, None);
        form.session = true;
        form
    }

    /// Whether row `i` is on screen.
    fn shown(&self, i: usize) -> bool {
        !self.session || SESSION.contains(&FIELDS[i].name)
    }

    pub fn new(
        d: &config::Policy,
        columns: Option<&[String]>,
        spark: Option<&config::Sparkline>,
        pane: Option<&config::Pane>,
        start: Option<&config::Start>,
        confirm_secs: Option<f64>,
    ) -> Self {
        let num = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
        let flag = |v: Option<bool>| v.map(|v| v.to_string()).unwrap_or_default();
        let spark = |f: fn(&config::Sparkline) -> String| spark.map(f).unwrap_or_default();
        let pane = |f: fn(&config::Pane) -> String| pane.map(f).unwrap_or_default();
        let values = FIELDS
            .iter()
            .map(|f| match f.name {
                "timeout_min" => num(d.timeout_min),
                "budget_usd" => num(d.budget_usd),
                "daily_budget_usd" => num(d.daily_budget_usd),
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
                "model" => d.model.clone().unwrap_or_default(),
                "harness" => d.harness.map(|h| h.to_string()).unwrap_or_default(),
                "max_turns" => d.max_turns.map(|v| v.to_string()).unwrap_or_default(),
                "codex_model" => d.codex_model.clone().unwrap_or_default(),
                "codex_full_access" => flag(d.codex_full_access),
                "notify" => flag(d.notify),
                "bedrock" => flag(d.bedrock),
                "aws_profile" => d.aws_profile.clone().unwrap_or_default(),
                "aws_region" => d.aws_region.clone().unwrap_or_default(),
                "start.harness" => start.map(|s| s.harness.to_string()).unwrap_or_default(),
                "start.pane" => start.map(|s| s.pane.to_string()).unwrap_or_default(),
                "pane.at" => pane(|p| p.at.clone()),
                "sparkline.bars" => spark(|s| s.bars.to_string()),
                "sparkline.bucket" => spark(|s| s.bucket.clone()),
                "sparkline.metric" => spark(|s| s.metric.clone()),
                "confirm_secs" => num(confirm_secs),
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
            session: false,
            columns: columns.map(<[String]>::to_vec),
        }
    }

    /// Select `row`, the cursor after its value.
    fn go(&mut self, row: usize) {
        self.row = row;
        self.cursor = usize::MAX;
    }

    /// Open the selected field for editing.
    fn enter(&mut self) {
        self.open = true;
        self.before = self.values[self.row].clone();
        self.cursor = usize::MAX;
    }

    fn field(&self) -> &'static Field {
        &FIELDS[self.row]
    }

    /// The values as a policy and the columns list; the error is the one line shown inline on
    /// the field it names.
    #[allow(clippy::type_complexity)]
    fn config(
        &self,
    ) -> Result<
        (
            config::Policy,
            Vec<String>,
            Option<config::Sparkline>,
            Option<config::Pane>,
            Option<config::Start>,
            Option<f64>,
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
        let columns = self.columns.clone().unwrap_or_default();
        let policy = config::Policy {
            timeout_min: num("timeout_min", "a number of minutes, as in 30")?,
            budget_usd: num("budget_usd", "dollars, as in 2.00")?,
            daily_budget_usd: num("daily_budget_usd", "dollars, as in 10.00")?,
            write: flag("write"),
            max_turns: match v("max_turns") {
                "" => None,
                t => Some(
                    t.parse::<u32>()
                        .map_err(|_| format!("max_turns: a whole number, as in 5, not {t:?}"))?,
                ),
            },
            codex_full_access: flag("codex_full_access"),
            overlap: match v("overlap") {
                "skip" => Some(config::Overlap::Skip),
                "allow" => Some(config::Overlap::Allow),
                "replace" => Some(config::Overlap::Replace),
                _ => None,
            },
            notify: flag("notify"),
            model: text("model"),
            codex_model: text("codex_model"),
            bedrock: flag("bedrock"),
            aws_profile: text("aws_profile"),
            aws_region: text("aws_region"),
            harness: match v("harness") {
                "claude" => Some(HarnessKind::Claude),
                "codex" => Some(HarnessKind::Codex),
                _ => None,
            },
        };
        // Bedrock with nothing to authenticate it is refused here as the file refuses it, so
        // the session form, which writes nothing and so never reaches `resolve`, cannot set
        // one either. The message names `bedrock`, so it lands on that row.
        config::bedrock_aws(
            policy.bedrock,
            policy.aws_profile.as_deref(),
            policy.aws_region.as_deref(),
        )
        .map_err(|e| format!("{e:#}"))?;
        // The sparkline block: every field empty leaves it out; otherwise the built-in fills
        // what is not typed, and the block is checked the way jobs.yaml is read.
        let spark = if ["bars", "bucket", "metric", "bound"]
            .iter()
            .all(|f| v(&format!("sparkline.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Sparkline::default();
            let s = config::Sparkline {
                bars: match v("sparkline.bars") {
                    "" => built.bars,
                    t => t.parse().map_err(|_| {
                        format!("sparkline.bars: a whole number, as in 16, not {t:?}")
                    })?,
                },
                bucket: text("sparkline.bucket").unwrap_or(built.bucket),
                metric: text("sparkline.metric").unwrap_or(built.metric),
                bound: text("sparkline.bound").unwrap_or(built.bound),
            };
            // Name the field the message is about, so the error lands on it.
            s.check().map_err(|e| {
                let e = format!("{e:#}");
                let field = ["bars", "bucket", "metric", "bound"]
                    .into_iter()
                    .find(|f| e.starts_with(&format!("sparkline {f}")))
                    .unwrap_or("bars");
                format!(
                    "sparkline.{field}: {}",
                    e.trim_start_matches(&format!("sparkline {field} "))
                )
            })?;
            Some(s)
        };
        // The pane block, the same way.
        let pane = if ["at"].iter().all(|f| v(&format!("pane.{f}")).is_empty()) {
            None
        } else {
            let built = config::Pane::default();
            let p = config::Pane {
                at: text("pane.at").unwrap_or(built.at),
            };
            p.check().map_err(|e| {
                let e = format!("{e:#}");
                format!("pane.at: {}", e.trim_start_matches("pane at "))
            })?;
            Some(p)
        };
        // The start block, the same way: no field typed leaves it out of the file.
        let start = if ["harness", "pane"]
            .iter()
            .all(|f| v(&format!("start.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Start::default();
            Some(config::Start {
                harness: match v("start.harness") {
                    "codex" => HarnessKind::Codex,
                    "claude" => HarnessKind::Claude,
                    _ => built.harness,
                },
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
        Ok((policy, columns, spark, pane, start, mark))
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> ConfigAction {
        self.error = None;
        if !self.open {
            match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                KeyCode::Enter => self.enter(),
                KeyCode::Up => {
                    if let Some(r) = (0..self.row).rev().find(|&i| self.shown(i)) {
                        self.go(r);
                    }
                }
                KeyCode::Down => {
                    if let Some(r) = (self.row + 1..FIELDS.len()).find(|&i| self.shown(i)) {
                        self.go(r);
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
            // A field that closes on a value it did not open with saves the block there and
            // then. Its own complaint keeps it open to be fixed; another field's closes it
            // and stands, since it is what the file would have got.
            KeyCode::Enter => {
                let changed = self.values[self.row] != self.before;
                match self.config() {
                    Err(e) if e.starts_with(&format!("{}:", self.field().name)) => {
                        self.error = Some(e);
                    }
                    Err(e) => {
                        // Another field's complaint: the cursor goes to the field it names,
                        // since that is what stopped the block from being written.
                        self.open = false;
                        self.go(FIELDS
                            .iter()
                            .position(|f| e.starts_with(&format!("{}:", f.name)))
                            .unwrap_or(self.row));
                        self.error = Some(e);
                    }
                    Ok((p, c, s, pn, st, m)) => {
                        self.open = false;
                        if changed {
                            return ConfigAction::Save(Box::new(p), c, s, pn, st, m);
                        }
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab
                if self.field().picked(&self.values[self.row]) =>
            {
                let f = self.field();
                let opts = f.picks().unwrap_or_default();
                let step = |at: usize| {
                    (at + if code == KeyCode::Left {
                        opts.len() - 1
                    } else {
                        1
                    }) % opts.len()
                };
                let at = opts
                    .iter()
                    .position(|o| *o == self.values[self.row])
                    .unwrap_or(0);
                self.values[self.row] = match step(at) {
                    0 => String::new(),
                    at => opts[at].to_owned(),
                };
            }
            // A letter jumps to the option that starts with it.
            KeyCode::Char(c) if !self.field().typed() => {
                let f = self.field();
                let opts = f.picks().unwrap_or_default();
                if let Some(o) = opts.iter().find(|o| f.label(o).starts_with(c)) {
                    self.values[self.row] = if *o == "-" {
                        String::new()
                    } else {
                        (*o).to_owned()
                    };
                }
            }
            _ if self.field().typed() => {
                // Typing over a single pick starts from empty rather than appending to it.
                if matches!(self.field().input, Answer::PickOrType(..))
                    && self.field().picked(&self.values[self.row])
                {
                    self.values[self.row].clear();
                }
                if let Some(at) = edit(&mut self.values[self.row], self.cursor, code, mods) {
                    self.cursor = at;
                }
            }
            _ => {}
        }
        ConfigAction::Stay
    }

    /// The editor where the list is: a title, the fields under their group headers, each row
    /// name, value and a few words in three columns that hold still whichever row is selected,
    /// the selected row's name lit and its value pressed, then the selected field's fuller
    /// explanation, wrapped to `columns` with the rows' indent and padded to the tallest one so
    /// the block keeps its height.
    fn lines(&self, columns: u16) -> Vec<Line<'static>> {
        let title = if self.session {
            (
                "next session",
                "harness, model and provider, from the defaults",
            )
        } else {
            ("config", "jobs.yaml")
        };
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                Span::styled(title.0, Style::default().fg(ORANGE)),
                Span::styled(format!("  {}", title.1), dim()),
            ]),
        ];
        let name_w = FIELDS.iter().map(|f| f.name.len()).max().unwrap_or(0);
        // The value column is as wide as the widest value on screen, so the words beside the
        // rows sit in one column; a value past VALUE_W, the columns list mostly, is cut with an
        // ellipsis and read whole on the prompt line while its row is selected.
        const VALUE_W: usize = 22;
        let shown = |i: usize| {
            let (f, v) = (&FIELDS[i], &self.values[i]);
            let (text, style) = if v.is_empty() {
                (f.builtin, dim())
            } else {
                (v.as_str(), Style::default())
            };
            let text = if text.chars().count() > VALUE_W {
                format!("{}…", text.chars().take(VALUE_W - 1).collect::<String>())
            } else {
                text.to_owned()
            };
            (text, style)
        };
        let value_w = (0..FIELDS.len())
            .filter(|&i| self.shown(i))
            .map(|i| shown(i).0.chars().count())
            .max()
            .unwrap_or(0);
        let mut head: Option<(&str, &str)> = None;
        for (i, f) in FIELDS.iter().enumerate() {
            if !self.shown(i) {
                continue;
            }
            // A group's head above its first row, a dim sub-head where a block inside it
            // starts. The session form is one short list under its own title, so it shows
            // neither.
            if !self.session && head.map(|(g, _)| g) != Some(f.group) {
                let (name, what) = GROUPS
                    .iter()
                    .find(|(g, _)| *g == f.group)
                    .copied()
                    .unwrap_or((f.group, ""));
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::styled(name.to_owned(), Style::default().fg(ORANGE)),
                    Span::styled(format!("  {what}"), dim()),
                ]));
            }
            if !self.session && !f.sub.is_empty() && head.map(|(_, b)| b) != Some(f.sub) {
                lines.push(Line::from(Span::styled(format!("  {}", f.sub), dim())));
            }
            head = Some((f.group, f.sub));
            let selected = i == self.row;
            let (value, style) = shown(i);
            let gap = value_w - value.chars().count() + 2;
            let mut spans = vec![
                Span::styled(
                    format!("    {:<name_w$}  ", f.name),
                    if selected { lit() } else { bold() },
                ),
                Span::styled(
                    value,
                    if selected && self.open {
                        pressed()
                    } else if selected {
                        bold()
                    } else {
                        style
                    },
                ),
                Span::styled(format!("{}{}", " ".repeat(gap), f.short), dim()),
            ];
            if selected && let Some(e) = &self.error {
                spans.push(Span::styled(
                    format!("  {e}"),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        // The explanation wrapped here, not by the widget, so every line of it keeps the
        // rows' indent rather than the second one falling back to the margin.
        let f = self.field();
        let explain = format!("    {:<name_w$}  ", f.name);
        let room = (columns as usize).saturating_sub(explain.len()).max(20);
        let tall = FIELDS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.shown(*i))
            .map(|(_, f)| wrap(f.long, room).len())
            .max()
            .unwrap_or(1);
        let mut rest = wrap(f.long, room).into_iter();
        lines.push(Line::from(vec![
            Span::styled(explain, bold()),
            Span::raw(rest.next().unwrap_or_default()),
        ]));
        let mut n = 1;
        for l in rest {
            lines.push(Line::from(format!("    {l}")));
            n += 1;
        }
        lines.extend((n..tall).map(|_| Line::default()));
        lines
    }

    /// The prompt line: the selected field's value whole, with `enter` to open it; open, where
    /// it is edited: its options with the current one bracketed when it is picked, or its
    /// value under the cursor when it is typed, and what leaving it empty means.
    fn line(&self) -> Line<'static> {
        let f = self.field();
        let value = &self.values[self.row];
        let mut spans = vec![Span::styled(
            format!("{} › ", f.name),
            Style::default().fg(ORANGE),
        )];
        if !self.open {
            if value.is_empty() {
                spans.push(Span::styled(f.builtin.to_owned(), dim()));
            } else {
                spans.push(Span::raw(value.clone()));
            }
            spans.push(Span::styled("  enter edits", dim()));
            return Line::from(spans);
        }
        let help = match f.input {
            Answer::Pick(opts) | Answer::PickOrType(opts, _) if f.picked(value) => {
                let at = opts.iter().position(|o| *o == *value).unwrap_or(0);
                let labels: Vec<&str> = opts.iter().map(|o| f.label(o)).collect();
                picks(&mut spans, &labels, at);
                let typed = match f.input {
                    Answer::PickOrType(_, what) => format!(", or {what}"),
                    _ => String::new(),
                };
                if f.builtin == SYSTEM {
                    format!("  system default passes nothing{typed}")
                } else {
                    format!("  - is the built-in, {}{typed}", f.builtin)
                }
            }
            _ => {
                spans.extend(typed(value, self.cursor, f.builtin));
                match (value.is_empty(), f.builtin == SYSTEM) {
                    (true, true) => "  passes nothing to the harness; type to set it".to_owned(),
                    (true, false) => "  the built-in; type to set it".to_owned(),
                    (false, true) => "  empty is the system default".to_owned(),
                    (false, false) => format!("  empty is the built-in, {}", f.builtin),
                }
            }
        };
        spans.push(Span::styled(help, dim()));
        Line::from(spans)
    }
}

/// `text` broken at spaces into lines of at most `width` columns; a word longer than the
/// width takes a line of its own.
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

/// What a key did to an arrangement: moved the cursor and nothing else, changed which
/// columns the table draws, kept them, or left them as they were found.
enum Arranged {
    Stay,
    Shown(Vec<String>),
    Keep(Vec<String>),
    Cancel(Vec<String>),
}

/// `ctrl+t`: the session columns arranged on the table they belong to. `order` is every
/// column there is, the first `shown` of them the ones the table draws, in their order, and
/// `at` is the one under the cursor. The table redraws under each key, so a set is picked
/// against the rows it applies to and against the width it has to fit; `before` is what
/// `esc` puts back.
struct ColumnForm {
    order: Vec<String>,
    shown: usize,
    at: usize,
    before: Vec<String>,
}

impl ColumnForm {
    /// Seeded from the set on screen: those first, in their order, then every other column
    /// there is, so what can be added is as visible as what is already there.
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
            before: columns.to_vec(),
            order,
        }
    }

    /// The columns the table draws, in their order.
    fn chosen(&self) -> Vec<String> {
        self.order[..self.shown].to_vec()
    }

    /// `← →` walk every column, `space` moves the one under the cursor between shown and
    /// not, `[` `]` move a shown one along the row, `enter` keeps the arrangement and `esc`
    /// drops it. A column leaving goes to the head of the ones not shown, so the cursor
    /// stays on the name it acted on and `space` again brings it back as the last one shown.
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
            KeyCode::Enter => Arranged::Keep(self.chosen()),
            KeyCode::Esc => Arranged::Cancel(self.before.clone()),
            _ => Arranged::Stay,
        }
    }

    /// The prompt line: every column there is, the ones the table draws in their order, then
    /// a separator and the ones it does not, dim. The one under the cursor is pressed, as a
    /// menu button is.
    fn line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled("columns › ", Style::default().fg(ORANGE))];
        for (i, c) in self.order.iter().enumerate() {
            if i == self.shown {
                spans.push(Span::styled("· ", dim()));
            }
            spans.push(Span::styled(
                format!(" {c} "),
                if i == self.at {
                    pressed()
                } else if i < self.shown {
                    plain()
                } else {
                    dim()
                },
            ));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }
}

enum Mode {
    Normal,
    Filter,
    Job(Box<JobForm>),
    /// The menu's `config` button: the editor of jobs.yaml's `defaults` block.
    Config(Box<ConfigForm>),
    /// The menu's `folder` prompt: the path typed so far.
    Folder(Input),
    /// The `ctrl+n` prompt: the selected Claude session's new title.
    Rename(Input),
    /// The usage guide, `ctrl+g`, drawn where the list is; the wrapped line at its top.
    Guide(usize),
    /// `ctrl+t`: the session columns being arranged, with the table live under them.
    Columns(Box<ColumnForm>),
}

/// The usage guide: a key and what it does, in the words of docs/dashboard.md; an entry with
/// no key is a heading. A test keeps every key here in that file.
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
        "the same over the whole frame; ctrl+z or esc come back to the pane",
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
        "ctrl+t",
        "arrange the session columns with the table live under them: ← → pick a column, space shows or hides it, [ ] move it, enter keeps it in jobs.yaml",
    ),
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
        "the harness the next session starts under, claude or codex; the composer's prefix shows it",
    ),
    (
        "ctrl+o",
        "the harness, model and provider the next sessions start with, seeded from the defaults; the composer's prefix shows them",
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
        "into the pane's viewer and back out to the list; shift+tab inside a viewer is the client's",
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
    /// The dashboard's own working directory: where a launch goes from the menu row or with
    /// nothing selected, and what the `folder` prompt takes a relative path from.
    cwd: PathBuf,
    /// The menu row's picked button, an index into `MENU`; `←` `→` move it.
    menu: usize,
    data: Data,
    rows: Vec<Row>,
    /// The rows the list is not showing: the jobs rows while the main screen is up, for the
    /// pane's preview of the `jobs` button; the main rows, menu included, while the jobs
    /// screen is in the pane, for the list beside it.
    other: Vec<Row>,
    /// Indexes into `rows` that pass the filter; the cursor indexes this list.
    visible: Vec<usize>,
    cursor: usize,
    scroll: usize,
    by_state: bool,
    /// The menu's `jobs` button: the jobs screen where the tables are, until esc.
    jobs_view: bool,
    /// Column widths so far, so a value changing length never shifts the table.
    widths: Widths,
    filter: Input,
    mode: Mode,
    status: String,
    /// The composer: the instruction a session in the selected row's directory starts with,
    /// and where in it the next key lands, a byte offset `snap` keeps honest.
    text: String,
    caret: usize,
    /// The PNGs pasted into the instruction, in the order their markers were typed.
    images: Vec<PathBuf>,
    /// The harness the next session starts under; `tab` cycles it. An index into `harness::KNOWN`.
    harness: usize,
    /// The model and provider the next sessions start with, once `ctrl+o` has set them; else
    /// the `defaults` block's. They stay until set again.
    session: Option<config::Policy>,
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
    /// The row key ctrl+x armed; stays until ctrl+x confirms, any other key clears it, or
    /// `confirm_secs` pass since `armed_at` with no key.
    armed: Option<String>,
    armed_at: Instant,
    /// When ctrl+c was last pressed; a second press within `QUIT_CONFIRM` quits. A single
    /// ctrl+c aimed at a viewer that has just closed must not take the dashboard with it.
    quit_armed: Option<Instant>,
    /// `cones tui --debug`: every terminal hand-off and input event is appended here.
    log: Option<PathBuf>,
    /// The viewers alive inside the dashboard, focused or parsing off-screen; at most
    /// `MAX_VIEWERS`, the least recently focused closes when another opens.
    viewers: Vec<Open>,
    /// The viewer that has the pane and the keys; an index into `viewers`.
    focus: Option<usize>,
    /// The real terminal's default colors, probed once at start, for viewers that ask.
    colors: viewer::Colors,
    /// Whether the real terminal reports the mouse to the dashboard right now: for split
    /// clicks and viewer scrolling, including clients that leave the wheel to the terminal.
    mouse_capture: bool,
    /// Clear the terminal before the next frame: set when a viewer leaves the frame.
    needs_clear: bool,
    /// The last frame's rows and columns.
    size: (u16, u16),
    /// The pane viewers are drawn in and sized to, from the last frame: the column beside the
    /// list on a wide frame, else the frame less the strip row under it.
    pane: Rect,
    /// Whether the viewer is drawn beside the list, at any width; `start.pane` from jobs.yaml
    /// to begin with, then ctrl+\ toggles it, from the list or inside a viewer, and off it
    /// the viewer takes the whole frame. A layout, not a state: it stays until toggled again.
    split: bool,
    /// The focused viewer has the whole frame whatever `split` says: shift+enter opened it
    /// so, once. Cleared when the viewer is left, or when ctrl+\ inside it asks for the split.
    full: bool,
    /// The selected row's viewer key and when the cursor arrived on it; after `REST` a Claude
    /// session row's viewer opens out of sight.
    rest: Option<(String, Instant)>,
    /// The key the resting cursor already opened once, so a refused attach is not started
    /// again while the cursor stays there; cleared when the cursor moves.
    prespawned: Option<String>,
    /// Where the list rows were drawn last, so a click finds its row.
    list_area: Rect,
}

/// How long the cursor rests on a Claude session row before its viewer opens ahead of `enter`.
const REST: Duration = Duration::from_millis(400);

/// The rest beside the list, where the pane is waiting for the screen. Spawn to first text is
/// 215 ms at the median and 475 ms at the 90th percentile (`viewer_first_paint` in the debug
/// log, 217 attaches over two days), so 150 ms of rest was 40 percent of what the eye waited.
/// 50 ms lets a held arrow key through and opens on every row a hand steps across.
const REST_SPLIT: Duration = Duration::from_millis(50);

/// Lines one notch of the wheel scrolls an emulated screen, as most terminals scroll.
const WHEEL_LINES: i32 = 3;

/// The window title Claude's client sets in its own agent view, the screen `↑` opens from an
/// attached session's composer. A client parked there shows Claude's session list, not the
/// session the row names.
const AGENT_VIEW_TITLE: &str = "claude agents";

/// The viewers a dashboard keeps alive at once; opening another closes the least recently used
/// `claude attach` of a listed session, the one kind a resting cursor reopens unseen in a
/// quarter second. A Codex client, a harness's agents view or a resumed run has no such way
/// back: closed, its pane stays blank until `enter` starts it over, so
/// those stay until `ctrl+x` or the dashboard quits.
// ponytail: only attaches count against the cap, so many Codex clients exceed it; a cap of
// their own if the memory shows.
const MAX_VIEWERS: usize = 3;

/// The viewers opened by a resting cursor kept alive beside them, the oldest closing first.
/// Each is a `claude attach` process: 160 MB resident, idle CPU under a third of a percent,
/// so two peeked plus three live is 800 MB at worst. One slot made every step between two rows
/// a fresh attach, a quarter to half a second of blank pane; two covers that bounce. Not yet a
/// setting; it will be one alongside `MAX_VIEWERS`.
const SPECULATIVE_VIEWERS: usize = 2;

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
        coordinator: false,
        activity: Vec::new(),
    }
}

/// A viewer and what the dashboard knows about it.
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
    /// Opened while the cursor rested on its row, before `enter` asked for it. Not counted
    /// against `MAX_VIEWERS`; speculative viewers have their own pool of `SPECULATIVE_VIEWERS`,
    /// the oldest going first, so rows the cursor was on lately show at once whatever the
    /// number of live viewers; the first focus clears it.
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
        // What a new terminal comes up with, before `data` moves into the dashboard.
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
            session: None,
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
                                self.fill(prompt);
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
        // The table shows the arrangement being picked, not the file's, until it is kept:
        // a reload landing mid-pick would otherwise snap the columns back.
        if let Mode::Columns(form) = &self.mode {
            data.columns = form.chosen();
        }
        self.removed_sessions
            .retain(|id| data.sessions.iter().any(|s| &s.session_id == id));
        data.sessions
            .retain(|s| !self.removed_sessions.contains(&s.session_id));
        // A started session's row is handed over once the registry lists it. The cursor goes
        // along only if it is still on the placeholder: moved off in the meantime, it stays.
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
        // A session this read lists for the first time is the one just opened, from the
        // composer, another terminal or the registry taking a placeholder's row over.
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

    /// Move the cursor to a session that just appeared, so the row that was opened is the one
    /// under the cursor. Not while a viewer has the keys, nor while an instruction is being
    /// typed for the selected row's directory: neither should have its target changed underneath.
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
        // The jobs screen in the pane has no menu row of its own: the menu stays on the list
        // beside it, with `jobs` pressed.
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
            // A fresh dashboard opens on the first table; the menu is where `↑` ends.
            let below = |i: &usize| {
                let k = &self.rows[*i].kind;
                k.selectable() && *k != Kind::Menu
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
            Kind::Folder(dir) => self
                .data
                .folders
                .iter()
                .find(|p| &fleet::tilde(p) == dir)
                .cloned(),
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

    /// The last frame as a rectangle.
    fn frame(&self) -> Rect {
        Rect::new(0, 0, self.size.1, self.size.0)
    }

    /// Whether the pane is drawn beside the list, at any width.
    fn split_active(&self) -> bool {
        self.split && !(self.full && self.pane_focused())
    }

    /// How long the cursor rests on a row before its viewer opens: shorter beside the list.
    fn rest_for(&self) -> Duration {
        if self.split_active() {
            REST_SPLIT
        } else {
            REST
        }
    }

    /// The split frame's parts: the list, a one cell rule, the viewer pane. `pane.at: right`
    /// puts the list on the left at half the width, up to 100 columns, and the pane full
    /// height with the rest; `bottom` puts the list on top at half the height and the pane
    /// full width under it. No minimum: a small frame gets a small pane.
    fn split_areas(&self, frame: Rect) -> [Rect; 3] {
        if self.data.pane.at == "bottom" {
            return Layout::vertical([
                Constraint::Length(frame.height / 2),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .areas(frame);
        }
        Layout::horizontal([
            Constraint::Length((frame.width / 2).min(100)),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(frame)
    }

    /// The pane a viewer is drawn in and sized to, for a frame of `frame`: beside the list
    /// when the split is on, else the whole frame, each less the row under it that carries
    /// the viewer's keys, the strip on a full frame and the hint line beside the list; never
    /// fewer than one row. Spawn, focus and draw all size the viewer by this, so focusing
    /// never resizes it, and the row is kept whether the viewer has the keys or not, so
    /// taking them and giving them back never resizes it either.
    fn pane(&self, frame: Rect) -> Rect {
        let area = if self.split_active() {
            self.split_areas(frame)[2]
        } else {
            frame
        };
        let height = area.height.saturating_sub(1).max(1);
        Rect { height, ..area }
    }

    /// The viewer the pane shows. Beside the list: the focused one; else the selected row's,
    /// live or speculative, so a Claude row's pre-spawned screen is on view as soon as it
    /// paints; on any other session row nothing, so a Codex row never has a Claude session's
    /// screen under its name; on a row that is not a session, the one focused last. On a
    /// narrow frame only a focused viewer is drawn.
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
        let own = self
            .selected()
            .and_then(|r| Self::viewer_key(&r.kind))
            .and_then(|k| self.viewer_index(&k));
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

    /// `ctrl+n`: the rename prompt, filled with the selected Claude session's title.
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

    /// The viewer the user was in last; a speculative viewer was never in front.
    fn most_recently_focused(&self) -> Option<usize> {
        self.viewers
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.speculative)
            .max_by_key(|(_, o)| o.last_focused)
            .map(|(i, _)| i)
    }

    /// The menu button whose screen the pane shows, by `MENU` name: the one with the keys
    /// (the jobs screen and its wizard, the config editor, the folder prompt, the guide), else
    /// the picked one while the cursor is on the menu row. A focused viewer has the pane.
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

    /// A menu button's screen is on view in the pane without the keys, only picked, so tab
    /// can hand them to it as it hands them to a viewer there.
    fn panel_shown(&self) -> bool {
        self.split_active() && !self.panel_focused() && self.panel().is_some()
    }

    /// A menu button's screen has the keys.
    fn panel_focused(&self) -> bool {
        self.jobs_view
            || matches!(
                self.mode,
                Mode::Guide(_) | Mode::Config(_) | Mode::Job(_) | Mode::Folder(_)
            )
    }

    /// Something in the pane has the keys, a viewer or a button's screen; with `full` set it
    /// has the whole frame.
    fn pane_focused(&self) -> bool {
        self.focus.is_some() || self.panel_focused()
    }

    /// The config editor on jobs.yaml as it is now.
    fn config_form(&self) -> Box<ConfigForm> {
        Box::new(ConfigForm::new(
            &config::defaults(&self.jobs_path),
            config::file_columns(&self.jobs_path).as_deref(),
            config::file_sparkline(&self.jobs_path).as_ref(),
            config::file_pane(&self.jobs_path).as_ref(),
            config::file_start(&self.jobs_path).as_ref(),
            config::file_confirm_secs(&self.jobs_path),
        ))
    }

    /// esc or ctrl+z on the jobs screen: the list, with the cursor back on the menu row it
    /// was opened from, so the pane keeps the jobs on view.
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

    /// ctrl+\: from the list, the pane on or off; inside a viewer, the viewer beside the
    /// list or over the whole frame. A viewer shift+enter gave the whole frame goes beside
    /// the list first, whatever the layout was. The list is rebuilt, since each layout has
    /// its own column set.
    fn toggle_split(&mut self) {
        let once = std::mem::take(&mut self.full) && self.pane_focused();
        self.split = once || !self.split;
        self.needs_clear = true;
        self.rebuild();
        self.debug(|| format!("split {}", self.split));
    }

    /// Give a viewer the pane and the keys. A viewer opened ahead of `enter` becomes an
    /// ordinary one here, and the log gets how long it had been running; it now counts, so
    /// the least recently focused viewers make room for it first, as they would in `open`.
    fn focus(&mut self, mut i: usize) {
        if std::mem::take(&mut self.viewers[i].speculative) {
            let spawned = self.viewers[i].last_focused;
            self.timing("viewer_prespawn_hit", spawned);
            while self.live_viewers() > MAX_VIEWERS {
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

    /// Open `c` as a viewer under `key`, or return to the live viewer that already has that
    /// key. The viewer takes the pane above the strip; a fourth viewer closes the least
    /// recently focused one. The background agent stays in its daemon throughout.
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
                // The least recently focused makes room only once the new one is running. A
                // speculative viewer was never asked for, so it neither counts nor goes.
                while self.live_viewers() >= MAX_VIEWERS {
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

    /// The viewers the user has been in: every one but a speculative viewer.
    fn live_viewers(&self) -> usize {
        self.viewers.iter().filter(|o| !o.speculative).count()
    }

    /// The viewer eviction takes: the least recently focused `claude attach` of a listed
    /// session the user has been in, other than `keep`. Nothing else is evicted, since nothing
    /// reopens it quietly, see `MAX_VIEWERS`.
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

    /// Note which row the cursor is on and since when; a new row starts the rest over.
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

    /// The Claude session whose viewer opens ahead of `enter`: the whole policy in one place.
    /// The dashboard has the frame, in the normal mode with the composer empty and nothing
    /// being prepared; the cursor has rested for `REST` on a Claude background session that
    /// is listed, not being stopped or removed, has no viewer yet and was not tried during
    /// this rest. A background job whose prompt is done is still a live worker that `claude
    /// attach` joins and continues, so it is tried like a working one; a failed or stopped one
    /// has no worker to join. Only `claude attach` is side-effect free for its session:
    /// `cones attach` on a finished run resumes it, and a Codex client shows up in the fleet.
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

    /// Whether `claude attach` has a worker to join in `s`: a Claude session not in its own
    /// terminal and not failed or stopped. A background job whose prompt is done still has one.
    fn joinable(s: &Session) -> bool {
        s.harness == "claude"
            && !s.own_terminal()
            && !matches!(s.state.as_str(), "failed" | "stopped")
    }

    /// Open `claude attach` on `id` out of sight, so `enter` on its row finds it drawn. The
    /// previous speculative viewer goes; a failure is a debug line, since nothing was asked for.
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

    /// Speculative viewers pool beside the live ones, up to `SPECULATIVE_VIEWERS` of their
    /// own, so a row the cursor was on lately shows at once; past that the oldest speculative
    /// goes. The pool is not the live count's leftover: with three live viewers a single slot
    /// made every step between two rows a fresh `claude attach`, half a second to its first text.
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

    /// A speculative viewer whose session left the list has nothing to show; close it quietly.
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

    /// Close a viewer for good: its process group dies with it. A Codex thread it opened is
    /// recorded first, so its row stays.
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

    /// Ctrl+Z inside a viewer: the dashboard takes the frame back and the viewer keeps
    /// parsing off-screen, so `enter` on its row returns to its current screen at once.
    fn unfocus(&mut self) {
        if self.focus.is_none() {
            return;
        }
        // Beside the list nothing leaves the frame, so ratatui's diff is enough; a viewer
        // that had the whole frame is not trusted to have left it clean.
        self.needs_clear = !self.split_active();
        self.full = false;
        let i = self.focus.take().unwrap();
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
        // A client left in Claude's own agent view paints that list under the row's name, and
        // `enter` there attaches whatever row its cursor sits on, so the viewer is dropped:
        // the next rest on the row attaches the session again.
        if self.viewers[i].viewer.title() == Some(AGENT_VIEW_TITLE) {
            self.close(i);
            self.status = "left the agent view · enter attaches the session again".into();
        }
    }

    /// Feed every viewer: read what it wrote, answer its queries, hand it its input, and
    /// take a viewer that ended off the list with its exit in the status line. Returns true
    /// when the screen of the viewer on view changed.
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
            // A speculative viewer was never asked for: its end is the log's business only.
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

    /// The dashboard's one row under a focused viewer, as tmux keeps a status bar under a
    /// pane: the cone, the viewer's name, the fleet counts the header shows, the session
    /// elsewhere that needs input, and the keys that leave. The counts stay live because the
    /// reload loop runs while a viewer is focused. The ends get the width first; the middle
    /// is cut from its right, and the needs-input note is shown whole or not at all. On a
    /// width too narrow for both ends, `ctrl+\ split` goes first, then `tab back`.
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
        // A Codex thread started from the composer keeps a launch key until its first ctrl+z
        // records it, so its own row cannot be told apart from another's; no alert until then.
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
        // Full screen is a choice; the way back to the split is here.
        let mut keys = vec![
            Span::styled("tab back", dim()),
            Span::styled(" · ctrl+\\ split", dim()),
        ];
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
        // A cut that leaves a separator at the end, whole or in part, drops it.
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

    /// Split clicks and every focused viewer need mouse reports. A client that reads no
    /// mouse leaves the wheel to our emulator, including in the full-frame layout.
    fn wants_mouse(&self) -> bool {
        self.split_active() || self.focus.is_some()
    }

    /// Pasted text: wrapped for a focused viewer that asked for bracketed paste, raw
    /// otherwise; into the composer when nothing is focused. An empty paste is cmd+v with
    /// no text on the clipboard, an image: xterm.js (VS Code) brackets the nothing it read.
    /// It becomes ctrl+v, the image paste Claude Code and Codex have, in a viewer; the
    /// composer reads the image off the clipboard itself, as its own ctrl+v does.
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
            let at = snap(&self.text, self.caret);
            self.text.insert_str(at, text);
            self.caret = at + text.len();
        }
    }

    /// `ctrl+v`: the clipboard's image as one `[Image #n]` at the cursor, or why not.
    fn attach_image(&mut self) {
        match paste_image() {
            Ok(path) => {
                self.caret = attach(&mut self.text, self.caret, self.images.len());
                self.images.push(path);
            }
            Err(e) => self.status = e,
        }
    }

    /// The instruction out of the composer with each `[Image #n]` as its PNG's path, where
    /// the harness reads it as a file; the composer is left empty.
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

    /// An instruction back in the composer whole, cursor after it.
    fn fill(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.len();
    }

    /// A mouse event goes to the focused viewer, relative to its pane. Outside the pane, the
    /// strip row or the list, is the dashboard's: a press, move or wheel there goes nowhere,
    /// and a drag or release that crosses out is clamped to the pane's nearest edge so the
    /// viewer sees the button let go. The wheel reaches the pane's viewer focused or not, and
    /// does what a terminal's does: scrolls the emulated screen back when the viewer reads no
    /// mouse (Codex, like a shell, leaves the wheel to the terminal) or shift is held, else
    /// goes to the viewer.
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

    /// `ev` as the pane sees it, or nothing when it fell outside the pane and is dropped.
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

    /// A left click beside the list: on the pane it focuses the pane's viewer, on a list
    /// row it selects the row and takes the keys back. True when the event is still the
    /// viewer's.
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
                // A click on a picked button's screen presses the button, as enter does.
                Some(_) if !self.panel_focused() => {
                    self.full = false;
                    let _ = self.enter();
                    return false;
                }
                // The jobs screen's rows are the list here: the row under the pointer below.
                Some(_) => {}
            }
        }
        if self.focus.is_some() {
            self.unfocus();
        }
        let l = self.list_area;
        if (l.top()..l.bottom()).contains(&ev.row) && ev.column < l.right() {
            let n = self.scroll + (ev.row - l.y) as usize;
            if n < self.visible.len() && self.rows[self.visible[n]].kind.selectable() {
                self.cursor = n;
                if self.rows[self.visible[n]].kind == Kind::Menu {
                    // The button under the pointer, walking the row as `menu_cells` lays it
                    // out: the "▌ " mark, then each button and a gap.
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
                self.status = format!("{} failed: {error:#}", opening.what);
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
    /// alive inside the dashboard and has been seen, so a speculative one still says
    /// `attach`; a session of a harness that cannot be joined from here says so instead of
    /// promising an attach.
    fn enter_label(&self) -> &'static str {
        let row = self.selected();
        if row
            .and_then(|r| Self::viewer_key(&r.kind))
            .and_then(|k| self.viewer_index(&k))
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
        // A row whose viewer is alive returns to its current screen; nothing is started.
        if let Some(i) = Self::viewer_key(&kind).and_then(|k| self.viewer_index(&k)) {
            self.focus(i);
            return Ok(());
        }
        match kind {
            Kind::Job(name) => self.spawn(&["run", &name], None, &format!("started {name}")),
            Kind::NewJob => self.new_job(),
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
            Kind::Menu => self.open_menu(),
            Kind::Folder(dir) => {
                self.status = format!("type an instruction · enter starts a session in {dir}");
            }
            _ => {}
        }
        Ok(())
    }

    /// The picked menu button's screen, with the keys: what `enter` on the menu row opens,
    /// and what tab gives the pane while the button's screen is only on view there.
    fn open_menu(&mut self) {
        match MENU[self.menu].0 {
            "folder" => self.mode = Mode::Folder(Input::default()),
            "jobs" => self.show_jobs(),
            "config" => self.mode = Mode::Config(self.config_form()),
            _ => self.mode = Mode::Guide(0),
        }
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

    /// Where `kind` sits in `harness::KNOWN`; None, the built-in, is Claude.
    fn harness_at(kind: Option<HarnessKind>) -> usize {
        harness::KNOWN
            .iter()
            .position(|k| Some(*k) == kind)
            .unwrap_or(0)
    }

    /// What the next session runs under: the model and provider `ctrl+o` set, else the
    /// `defaults` block's, which reach a session as they reach a job.
    fn session_policy(&self) -> config::Policy {
        self.session
            .clone()
            .unwrap_or_else(|| config::defaults(&self.jobs_path))
    }

    /// The composer's `enter`: a session in the selected row's directory with the text as its
    /// first instruction, under the harness `tab` picked. Claude starts in the background on a
    /// thread and its row appears when Claude lists it; Codex opens here and Ctrl+Z leaves it.
    fn start(&mut self) {
        // The menu's `jobs` button and the `new job` row: the wizard, with the instruction as
        // the task's first draft.
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
        if kind == HarnessKind::Codex {
            let record = Some((dir.clone(), since));
            let retry = Some(prompt.clone());
            // A new thread has no id yet; the key is unique to this launch until the thread is
            // recorded on the first ctrl+z, when the viewer takes the thread's id as its key.
            let key = format!("codex:start:{}", since.timestamp_millis());
            self.prepare_viewer(what, key, record, retry, move || {
                match harness::start(kind, &dir, prompt.trim(), &policy)? {
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

    /// Mark `key`'s row red and start the mark's clock.
    fn arm(&mut self, key: String) {
        self.armed = Some(key);
        self.armed_at = Instant::now();
    }

    /// Each pass of the draw loop: a mark left alone for `confirm_secs` clears as if a key had
    /// kept it, and a ctrl+c the second press did not follow stops showing after
    /// `QUIT_CONFIRM`; each takes its hint off the line with it.
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

    /// A first ctrl+c is still waiting for its second press.
    fn quitting(&self) -> bool {
        self.quit_armed
            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM)
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
                self.arm(name);
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
                self.arm(id);
            }
        }
    }

    /// ctrl+x on a pinned folder's row: once arms, again drops the folder from the dashboard.
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

    /// The menu's `folder` prompt took a directory: it gets a row at once and keeps it across
    /// restarts until ctrl+x removes it. The cursor moves onto the row, so the composer starts
    /// its next session there. A directory something already runs in has its group; the
    /// cursor stays where it was.
    fn pin_folder(&mut self, dir: PathBuf) -> Result<()> {
        if !self.data.folders.contains(&dir) {
            self.data.folders.push(dir.clone());
            self.save_folders()?;
        }
        self.rebuild();
        self.select_new(&fleet::tilde(&dir));
        Ok(())
    }

    /// ctrl+p: pin the selected row's folder, so it keeps a row after the last session there
    /// leaves; on the menu row, the dashboard's own.
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

    /// Write `columns:` to jobs.yaml with the rest of the file as it stands: the columns
    /// are the only thing `ctrl+t` sets.
    fn save_columns(&mut self, cols: &[String]) {
        let path = self.jobs_path.clone();
        let wrote = config::write_config(
            &path,
            &config::defaults(&path),
            Some(cols),
            config::file_sparkline(&path).as_ref(),
            config::file_pane(&path).as_ref(),
            config::file_start(&path).as_ref(),
            config::file_confirm_secs(&path),
        );
        self.status = match wrote {
            Ok(()) => {
                self.invalidate();
                format!("columns saved to {}", fleet::tilde(&path))
            }
            Err(e) => format!("{e:#}"),
        };
    }

    fn save_folders(&self) -> Result<()> {
        Ledger::new(&self.state).and_then(|l| l.write_folders(&self.data.folders))
    }

    /// True on the jobs screen's `new job` row.
    fn on_new_job(&self) -> bool {
        matches!(self.selected().map(|r| &r.kind), Some(Kind::NewJob))
    }

    /// The menu's `jobs` button: the jobs screen where the tables were, the cursor on its first
    /// row; esc comes back to the dashboard.
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

    /// enter on the `new job` row: the wizard on a new job, seeded with what the composer
    /// holds, its directory defaulting to the selected row's.
    fn new_job(&mut self) {
        let (base, fallback) = (self.jobs_dir(), self.target_dir());
        let seed = self.take_prompt();
        self.mode = Mode::Job(Box::new(JobForm::new(&base, &fallback, None, &seed)));
    }

    /// ctrl+e: the wizard on the selected job, filled in from the file as written.
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
                    )));
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
            Kind::Folder(_) => Some("remove"),
            _ => None,
        }
    }

    /// The model and provider the next session starts with, as words for the composer's
    /// prefix and the status: the harness, its model when one is set, `bedrock` when on.
    /// A field left to the system default says nothing.
    // ponytail: bedrock false shows nothing either; it and unset both run on the harness's
    // own endpoint unless the shell says otherwise.
    fn session_words(&self) -> Vec<String> {
        let kind = harness::KNOWN[self.harness];
        let p = self.session_policy();
        let model = match kind {
            HarnessKind::Claude => p.model,
            HarnessKind::Codex => p.codex_model,
            // Not in `harness::KNOWN`, so `tab` never lands here.
            HarnessKind::Pi => None,
        };
        let mut words = vec![kind.to_string()];
        words.extend(model);
        if p.bedrock == Some(true) {
            words.push("bedrock".into());
        }
        words
    }

    /// The composer: the harness `shift+tab` picked with the model and provider the next
    /// session starts with, then the instruction or a short placeholder.
    fn composer(&self) -> Line<'static> {
        let kind = harness::KNOWN[self.harness].to_string();
        let words = self.session_words();
        let mut spans = vec![Span::styled(
            format!("{} › ", logo(&kind)),
            brand(&kind).add_modifier(Modifier::BOLD),
        )];
        if words.len() > 1 {
            spans.push(Span::styled(
                format!("{} › ", words[1..].join(" · ")),
                dim(),
            ));
        }
        let label = |n: usize| format!("[Image #{}]", n + 1);
        let shown = expand(&self.text, label);
        let caret = expand(&self.text[..snap(&self.text, self.caret)], label).len();
        spans.extend(typed(&shown, caret, "Type an instruction…"));
        Line::from(spans)
    }

    /// The bottom line: the last action's status until the next key, else the keys.
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
        // Beside the list a focused viewer has no strip; the keys that leave it are here.
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

    /// The keys for the mode, unfocused. The Normal line is one row with no wrap, so the
    /// keys that act everywhere go, last first, until it fits the column it is drawn in
    /// less `taken` columns; the first key, the selected row's, and `esc quit` stay.
    fn mode_hints(&self, taken: usize) -> Line<'static> {
        let next = harness::KNOWN[(self.harness + 1) % harness::KNOWN.len()].to_string();
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
            Mode::Job(form) => {
                let mut keys = vec![];
                if form.step == Step::When {
                    keys.push(("← →", "pick"));
                }
                keys.push((
                    "enter",
                    match (form.step, WHEN[form.when]) {
                        (Step::Name, _) => "save",
                        (Step::When, "once") => "run now",
                        _ => "next",
                    },
                ));
                if form.step == Step::Where {
                    keys.push(("tab", "complete"));
                }
                if form.step != Step::What {
                    keys.push(("↑", "back"));
                }
                keys.push(("esc", "cancel"));
                hints(&keys)
            }
            Mode::Config(form) if form.open => {
                let mut keys = vec![];
                if form.field().picked(&form.values[form.row]) {
                    keys.push(("← →", "pick"));
                }
                keys.extend([("enter", "keep"), ("esc", "back")]);
                hints(&keys)
            }
            Mode::Config(_) => hints(&[("↑ ↓", "field"), ("enter", "edit"), ("esc", "done")]),
            Mode::Guide(_) => hints(&[("↑ ↓", "scroll"), ("esc", "back")]),
            Mode::Columns(f) => hints(&[
                ("← →", "column"),
                ("space", if f.at < f.shown { "hide" } else { "show" }),
                ("[ ]", "move"),
                ("enter", "keep"),
                ("esc", "cancel"),
            ]),
            Mode::Folder(_) => hints(&[
                ("enter", "add"),
                ("tab", "complete"),
                ("↑ ↓", "recent"),
                ("esc", "cancel"),
            ]),
            Mode::Rename(_) => hints(&[("enter", "rename"), ("esc", "cancel")]),
            Mode::Normal if !self.text.is_empty() => {
                hints(&[("enter", &start), ("shift+tab", &next)])
            }
            // Only what acts on the selected row, then the keys that act everywhere.
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
                // tab reaches a button's screen on view in the pane too.
                if self.shown().is_some() || self.panel_shown() {
                    keys.push(("tab", "pane"));
                }
                keys.push(("shift+tab", next.as_str()));
                // Last of the keys that act everywhere, so a narrow list drops it first:
                // arranging the columns is setup, the harness is picked every session.
                if !self.jobs_view {
                    keys.push(("ctrl+t", "columns"));
                }
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

    /// The columns the hint line has: the list column beside a viewer, else the frame.
    fn hint_width(&self) -> u16 {
        if self.split_active() {
            self.split_areas(self.frame())[0].width
        } else {
            self.size.1
        }
    }

    /// ctrl+x once arms and marks the row, ctrl+x again stops; any other key disarms, so the
    /// mark stays for as long as the user looks at it: the `claude agents` convention.
    fn stop(&mut self) {
        let id = match self.selected().map(|r| r.kind.clone()) {
            Some(Kind::Run(id, s)) if s != "started" => return self.hide_run(id),
            Some(Kind::Folder(dir)) => return self.remove_folder(dir),
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
                // A live `--remote … resume` client on the row keeps the daemon holding the
                // thread, so it gets the SIGTERM a plain Codex TUI gets; the thread stays
                // resumable in the daemon.
                let client = self
                    .data
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .is_some_and(|s| s.pid.is_some());
                self.queue_stop(id, verb, move || {
                    if verb == "forget" {
                        codex::forget(&state, &target)?;
                        Ledger::new(&state)?.hide(&target)?;
                        if client {
                            fleet::stop(&claude, &target)?;
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
                Ok(true) if matches!(action.verb, "delete" | "forget") => {
                    self.removed_sessions.insert(action.id.clone());
                    self.data.sessions.retain(|s| s.session_id != action.id);
                    if action.verb == "delete" {
                        format!("deleted {} · claude --resume still has it", action.label)
                    } else {
                        format!(
                            "forgot {} · hidden for good · codex resume still has it",
                            action.label
                        )
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
    /// until the next key. While a viewer has the keys every key but ctrl+z and ctrl+\ is its,
    /// in the classic encoding; ctrl+z leaves it running and comes back here, ctrl+\ toggles
    /// the viewer beside the list. Two states only, the list or a viewer: a third, the viewer
    /// inside over the split, and a ctrl+] that focused the pane in place or cycled viewers
    /// were taken out on 2026-09-15 as too much to hold in mind. ctrl+\ means the same thing
    /// in both states, the pane or the whole frame, so it is the one key both take; tab is
    /// the other, the bounce from the list into the pane and back out of it: into a viewer,
    /// which tab leaves again, or into a button's screen, whose own forms take tab, so ctrl+z
    /// and esc are the way out of that one.
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        if let Some(open) = self.focused() {
            if ctrl && code == KeyCode::Char('z') {
                self.unfocus();
                return Ok(false);
            }
            // tab bounces back to the list, as it bounces into the pane from there. shift+tab
            // is still the client's, so a harness that cycles modes with it keeps that key.
            if code == KeyCode::Tab && mods.is_empty() {
                self.unfocus();
                return Ok(false);
            }
            // ctrl+\ arrives as the byte 0x1c, which crossterm reports as ctrl+4.
            if ctrl && matches!(code, KeyCode::Char('\\' | '4')) {
                self.toggle_split();
                return Ok(false);
            }
            // shift+pgup/pgdn scroll the screen a page back, as the terminal itself would
            // before a program saw them.
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
        // A button's screen that had the whole frame gives it back when it closes; a viewer's
        // is cleared by unfocus. Not while a viewer is still opening for it.
        if self.full && !self.pane_focused() && self.opening.is_none() {
            self.full = false;
        }
        // ctrl+z leaves a button's screen one step at a time, as esc does: the key it shares
        // with a viewer.
        if ctrl && code == KeyCode::Char('z') && self.panel_focused() {
            if matches!(self.mode, Mode::Normal) {
                self.leave_jobs();
            } else {
                self.mode = Mode::Normal;
            }
            self.needs_clear = true;
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
            // Every key that changes the arrangement redraws the table under it, so the
            // set is picked against the rows and the width it has to fit.
            Mode::Columns(form) => match form.key(code) {
                Arranged::Stay => {}
                Arranged::Shown(cols) => {
                    self.data.columns = cols;
                    self.rebuild();
                }
                Arranged::Cancel(cols) => {
                    self.data.columns = cols;
                    self.mode = Mode::Normal;
                    self.rebuild();
                    self.status = "columns as they were".into();
                }
                Arranged::Keep(cols) => {
                    self.data.columns = cols.clone();
                    self.mode = Mode::Normal;
                    self.rebuild();
                    self.save_columns(&cols);
                }
            },
            Mode::Guide(top) => {
                let top = *top;
                match code {
                    KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Char('g') if ctrl => self.mode = Mode::Normal,
                    KeyCode::Up => self.mode = Mode::Guide(top.saturating_sub(1)),
                    // ponytail: clamped to the entry count, not the wrapped line count.
                    KeyCode::Down => self.mode = Mode::Guide((top + 1).min(GUIDE.len() - 1)),
                    _ => {}
                }
            }
            Mode::Folder(input) => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                // ↑ ↓ recall the folders sessions have been seen in, newest first, as a
                // shell's history does; the prompt's text is the one recalled.
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
                // One tab grows the path as far as it is unambiguous; a second, changing
                // nothing, lists what still matches, as bash and zsh do.
                KeyCode::Tab => self.status = input.complete(&self.cwd).join("  "),
                // The folder prompt: a directory, relative to the dashboard's own, checked
                // before it is taken; it gets a row and the cursor, so a launch goes there.
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
            Mode::Job(form) if code == KeyCode::Tab && form.step == Step::Where => {
                self.status = form.complete().join("  ");
            }
            Mode::Job(form) => match form.key(code, mods) {
                FormAction::Stay => {}
                FormAction::Cancel => self.mode = Mode::Normal,
                // `once`: a supervised run under the file's first job's policy, in the ledger
                // like any other, instead of a bare session.
                FormAction::RunOnce(prompt, dir) => {
                    self.mode = Mode::Normal;
                    let what = format!("started a run in {}", fleet::tilde(&dir));
                    self.spawn(&["run", "--prompt", &prompt], Some(&dir), &what);
                }
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
            // A closed field checks the whole block and replaces the file at once, and the
            // editor stays open for the next one; a bad value comes back inline on its field.
            Mode::Config(form) => match form.key(code, mods) {
                ConfigAction::Stay => {}
                ConfigAction::Cancel => self.mode = Mode::Normal,
                // The session form keeps its policy for the composer; nothing is written.
                ConfigAction::Save(policy, ..) if form.session => {
                    self.harness = Self::harness_at(policy.harness);
                    self.session = Some(*policy);
                    self.status = format!("next session: {}", self.session_words().join(" · "));
                }
                ConfigAction::Save(policy, columns, spark, pane, start, mark) => {
                    match config::write_config(
                        &self.jobs_path,
                        &policy,
                        Some(&columns),
                        spark.as_ref(),
                        pane.as_ref(),
                        start.as_ref(),
                        mark,
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
                // Any key but ctrl+x disarms an armed ctrl+x, so the mark stays until the user
                // does something else, as in `claude agents`.
                let armed = self.armed.take();
                // On the menu row with nothing typed, ← → pick a button; typed text keeps them.
                if self.text.is_empty()
                    && matches!(code, KeyCode::Left | KeyCode::Right)
                    && matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu))
                {
                    let n = MENU.len();
                    self.menu = (self.menu + if code == KeyCode::Right { 1 } else { n - 1 }) % n;
                    return Ok(false);
                }
                if let Some(at) = edit(&mut self.text, self.caret, code, mods) {
                    self.caret = at;
                    return Ok(false);
                }
                match code {
                    KeyCode::Char('c') if ctrl => {
                        if self
                            .quit_armed
                            .replace(Instant::now())
                            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM)
                        {
                            return Ok(true);
                        }
                        self.status = QUIT_HINT.into();
                    }
                    KeyCode::Char('x') if ctrl => {
                        self.armed = armed;
                        self.stop();
                    }
                    // esc backs out one thing at a time: the armed ctrl+x, the text, the jobs
                    // screen, the dashboard.
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
                    // tab bounces into the pane's viewer and back; shift+tab, its sibling,
                    // cycles the harness the next session starts under, which ctrl+o also
                    // sets with the model and provider. A viewer's own shift+tab is its
                    // client's, so the two never collide.
                    KeyCode::Tab => match self.shown() {
                        Some(i) => self.focus(i),
                        // A button's screen in the pane takes the keys as a viewer does, so
                        // tab reaches the config editor and the guide too. The jobs screen
                        // already has them, so there tab is the bounce back to the list.
                        None if self.jobs_view => self.leave_jobs(),
                        None if self.panel_shown() => self.open_menu(),
                        None => self.status = "nothing in the pane".into(),
                    },
                    KeyCode::BackTab => {
                        self.harness = (self.harness + 1) % harness::KNOWN.len();
                    }
                    // shift+enter attaches over the whole frame, pane or no pane, and leaves
                    // the layout as it was. Claude Code's terminal bindings send it as ESC CR,
                    // which crossterm reports as alt+enter; a kitty-protocol terminal reports
                    // the shift itself.
                    KeyCode::Enter
                        if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && self.text.trim().is_empty() =>
                    {
                        // ponytail: set before enter so a viewer focused later (a Codex
                        // client) gets the frame too; a refused attach leaves it set until
                        // the next enter or unfocus, and it bites only while focused.
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
                    // ctrl+\ arrives as the byte 0x1c, which crossterm reports as ctrl+4.
                    KeyCode::Char('\\' | '4') if ctrl => self.toggle_split(),
                    KeyCode::Char('e') if ctrl => self.edit_job(),
                    KeyCode::Char('f') if ctrl => self.mode = Mode::Filter,
                    KeyCode::Char('o') if ctrl => {
                        let mut policy = self.session_policy();
                        policy.harness = Some(harness::KNOWN[self.harness]);
                        self.mode = Mode::Config(Box::new(ConfigForm::session(&policy)));
                    }
                    KeyCode::Char('g') if ctrl => self.mode = Mode::Guide(0),
                    // The columns are the sessions', so the jobs screen says so rather than
                    // arranging a table that is not on screen.
                    KeyCode::Char('t') if ctrl => {
                        if self.jobs_view {
                            self.status = "the session columns; esc leaves the jobs screen".into();
                        } else {
                            self.mode =
                                Mode::Columns(Box::new(ColumnForm::new(&self.data.columns)));
                        }
                    }
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
        // The split: the dashboard in its part, a rule, and the viewer on view in the pane.
        // Focus changes the rule's color and where keys go, nothing else; the header has the
        // counts, so there is no strip.
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
            // The viewer has the pane less its last row, kept for the keys whether the
            // viewer has them or not, so neither taking them nor a redraw resizes it.
            let inner = self.pane;
            match self.shown() {
                Some(i) if self.viewers[i].viewer.first_paint().is_some() => {
                    self.draw_viewer(frame, i, inner)
                }
                // A viewer that has not painted yet is sized for when it does, and the pane
                // stays blank until it does: a quarter second of nothing reads as a terminal
                // opening, where a placeholder that is then replaced reads as a flicker.
                Some(i) => self.viewers[i].viewer.resize(inner.height, inner.width),
                None => {}
            }
            // The row under the viewer: the keys that leave it, read under the viewer they
            // act on, as a button's screen has its own. Clear while the list has the keys,
            // where the hint line is its.
            if self.focus.is_some() && pane.height > 1 {
                let row = Rect {
                    y: pane.bottom() - 1,
                    height: 1,
                    ..pane
                };
                frame.render_widget(Paragraph::new(self.hint_line()), row);
            }
            return;
        }
        // A focused viewer has every row but the last: its emulated screen, cell for cell.
        // The last row is the dashboard's strip, so the viewer's own status line sits right
        // above it. Nothing else of the dashboard is drawn.
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

    /// Viewer `i`'s emulated screen in `pane`, sized to it, and while it has the keys and is
    /// not scrolled back the terminal's own cursor where the screen puts it, off a wide
    /// character's second half.
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

    /// The prompt line of the mode with the keys, the composer in the normal one.
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
            Mode::Columns(f) => f.line(),
            Mode::Normal => self.composer(),
        }
    }

    /// `line` ruled above and below, as Claude Code frames its input, grown with the text as
    /// its input does: the paragraph and the rows it takes within `width`, rules included.
    /// Red while a first ctrl+c waits for its second: the whole composer says it, not one
    /// dim line.
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

    /// Button `name`'s screen in `pane`, as a session's viewer would be: its body, and under
    /// it the prompt line of the mode that has the keys, or the button's explanation while
    /// it is only picked.
    fn draw_panel(&mut self, frame: &mut Frame, name: &str, pane: Rect) {
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
        // The dashboard keeps its last row for the hint line, so the pane keeps one too: the
        // two prompt boxes then sit on the same rows, rule against rule, and the keys of the
        // screen in the pane are read under it rather than across the frame.
        let [body, foot, hint] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(rows),
            Constraint::Length(1),
        ])
        .areas(pane);
        let wrapped = |lines| Paragraph::new(lines).wrap(Wrap { trim: false });
        match (&self.mode, name) {
            (Mode::Job(form), _) => frame.render_widget(wrapped(form.lines()), body),
            (Mode::Config(form), _) => frame.render_widget(wrapped(form.lines(body.width)), body),
            (Mode::Guide(top), _) => frame.render_widget(guide(*top), body),
            (_, "help") => frame.render_widget(guide(0), body),
            // ponytail: jobs.yaml is read again every frame the button is picked; cache the
            // form in `rebuild` if that ever shows in a profile.
            (_, "config") => {
                frame.render_widget(wrapped(self.config_form().lines(body.width)), body)
            }
            (_, "jobs") if self.jobs_view => self.draw_list(frame, body),
            (_, "jobs") => {
                let all: Vec<usize> = (0..self.other.len()).collect();
                let lines = self.row_lines(&self.other, &all, None, 0, body.height as usize);
                frame.render_widget(Paragraph::new(lines), body);
            }
            _ => frame.render_widget(Paragraph::new(self.recent_lines()), body),
        }
        frame.render_widget(prompt, foot);
        if self.panel_focused() {
            frame.render_widget(Paragraph::new(self.hint_line()), hint);
        }
    }

    /// The `folder` button's body: the folders sessions have been seen in, newest first, the
    /// one the prompt holds marked.
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

    /// The rows the composer keeps under its box, so its lower rule lands on the row the
    /// harness draws its own on: the last rule on the viewer's screen, the rows the harness
    /// keeps under it, and the pane's own key row. One row, the hint line, with nothing on
    /// view or with the pane under the list, where the two boxes share no rows anyway.
    // ponytail: the rule is read off the screen every frame rather than counted per harness,
    // so a statusline of any height lines up; a frame where the harness draws no rule at all
    // puts the composer back on the hint line, one row lower.
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
                        .cell(y, x)
                        .is_some_and(|c| c.contents() == "\u{2500}")
                })
                .count() as u16
                * 2
                > cols
        };
        (0..rows)
            .rev()
            .find(|&y| ruled(y))
            .map_or(1, |y| rows - y)
            .clamp(1, 6)
    }

    /// The dashboard in `area`: header, list, composer and hint line. Beside a pane that has
    /// a button's screen, the list and the composer stay in place: the screen's body and
    /// prompt line are drawn in the pane.
    fn draw_dashboard(&mut self, frame: &mut Frame, area: Rect) {
        let in_pane =
            self.split_active() && self.panel().is_some() && !matches!(self.mode, Mode::Columns(_));
        let mut line = if in_pane {
            self.composer()
        } else {
            self.mode_line()
        };
        // While a viewer or a button's screen has the keys the terminal's cursor is in the
        // pane, so the input's own block cursor is off: one cursor on the frame.
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
            Paragraph::new(header_lines(
                self.data.summary(spinner_frame(self.tick)),
                &fleet::tilde(&self.cwd),
                head.width as usize,
            )),
            head,
        );
        if in_pane && self.jobs_view {
            // The jobs screen has the cursor in the pane; the main rows sit beside it with
            // the menu row reading as selected, `jobs` pressed.
            let all: Vec<usize> = (0..self.other.len()).collect();
            let lines = self.row_lines(&self.other, &all, None, 0, list.height as usize);
            frame.render_widget(Paragraph::new(lines), list);
        } else if in_pane {
            self.draw_list(frame, list);
        } else if let Mode::Guide(top) = self.mode {
            frame.render_widget(guide(top), list);
        } else if let Mode::Job(form) = &self.mode {
            frame.render_widget(
                Paragraph::new(form.lines()).wrap(Wrap { trim: false }),
                list,
            );
        } else if let Mode::Config(form) = &self.mode {
            frame.render_widget(
                Paragraph::new(form.lines(list.width)).wrap(Wrap { trim: false }),
                list,
            );
        } else {
            self.draw_list(frame, list);
        }
        frame.render_widget(input, prompt);
        // Whatever has the keys in the pane draws them under the pane, where they are read
        // with it; the list's row stays empty rather than saying it twice. One hint line on
        // the frame, on the side the keys are.
        if !(self.split_active() && self.pane_focused()) {
            frame.render_widget(Paragraph::new(self.hint_line()), foot);
        }
    }

    /// True on the menu row with the button `name` picked.
    fn menu_is(&self, name: &str) -> bool {
        matches!(self.selected().map(|r| &r.kind), Some(Kind::Menu)) && MENU[self.menu].0 == name
    }

    /// The menu row's cells: each button a key cap with a gap after it, and while the row is
    /// selected the picked one pressed, with its explanation dim after the buttons. Unselected,
    /// the row is the buttons alone.
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
        );
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// `visible`'s rows into `rows` from `scroll`, `height` of them, `cursor` the selected
    /// one. Without a cursor the menu row alone reads as selected: a list drawn that way sits
    /// beside the jobs screen, whose button is the one pressed.
    fn row_lines(
        &self,
        rows: &[Row],
        visible: &[usize],
        cursor: Option<usize>,
        scroll: usize,
        height: usize,
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
                if row.kind.selectable() {
                    spans.push(Span::styled(
                        if selected { "▌ " } else { "  " },
                        Style::default().fg(if armed { Color::Red } else { ORANGE }),
                    ));
                }
                let menu;
                let cells = if row.kind == Kind::Menu {
                    menu = self.menu_cells(selected);
                    &menu
                } else {
                    &row.cells
                };
                for (c, (text, style)) in cells.iter().enumerate() {
                    // The icon cell of a working row is the spinner's current frame.
                    let (text, style) = if c == 0 && row.working() {
                        (
                            text.replacen('▁', SPINNER[spinner_frame(self.tick)], 1),
                            *style,
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
    /// Every key the guide names is in the dashboard's docs, so the two never drift.
    #[test]
    fn config_explanation_keeps_the_rows_indent() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        let c = ConfigForm::new(&config::Policy::default(), None, None, None, None, None);
        let lines = c.lines(48);
        let shown: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(
            shown.iter().any(|l| l.starts_with("runs  ")),
            "group headers sit on the margin"
        );
        assert!(
            shown.iter().any(|l| l.as_str() == "  sparkline"),
            "a block's sub-head is indented by two"
        );
        assert!(
            shown.iter().any(|l| l.starts_with("    timeout_min")),
            "rows are indented by four, under their sub-head"
        );
        let mut tail: Vec<&String> = shown
            .iter()
            .skip_while(|l| !l.starts_with("    timeout_min  "))
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
    fn guide_keys_are_documented() {
        let docs = include_str!("../docs/dashboard.md");
        for (key, _) in super::GUIDE {
            for word in key.split_whitespace() {
                assert!(docs.contains(word), "{word} is not in docs/dashboard.md");
            }
        }
        // ctrl+g draws the guide where the list is; esc brings the list back.
        let d = dir();
        let mut app = app(d.path());
        // The pane off: this is about the frame alone.
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
    fn header_keeps_its_border_at_narrow_widths_and_with_wide_folder_names() {
        for width in [0, 1, 7, 23, 24, 40, 60, 80, 120] {
            let lines = header_lines(
                Line::raw("123 working  4 need input  5 idle  6 done  ·  7 jobs  8 runs"),
                "~/个人/projects/a-long-folder",
                width,
            );
            assert_eq!(lines.len(), 3);
            assert!(lines.iter().all(|line| line.width() <= width));
            if width >= 24 {
                assert!(lines.iter().all(|line| line.width() == width));
                for (line, border) in lines.iter().zip(['┐', '│', '┘']) {
                    assert!(line.to_string().ends_with(border));
                }
            }
            if width == 120 {
                let summary = lines[1].to_string();
                assert!(summary.contains("123 working") && summary.contains("8 runs"));
            }
        }
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

    /// The wizard's answers, the defaults editor's values, the filter and the folder prompt
    /// take readline's keys as the composer does, with the cursor where the next key acts;
    /// moving to another answer puts the cursor after it, and tab on a path does the same.
    #[test]
    fn every_prompt_edits_where_the_cursor_is() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("src")).unwrap();
        let mut f = JobForm::new(base.path(), base.path(), None, "fix the tests");
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
            f.step,
            Step::What,
            "backspace on an empty answer still steps back"
        );
        assert_eq!(f.prompt, "fix the ");
        typed(&mut f, "x");
        assert_eq!(
            f.prompt, "fix the x",
            "the cursor is after the answer stepped back to"
        );

        let mut c = ConfigForm::new(&config::Policy::default(), None, None, None, None, None);
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
        c.key(KeyCode::Down, KeyModifiers::NONE);
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.key(KeyCode::Char('2'), KeyModifiers::NONE);
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.key(KeyCode::Up, KeyModifiers::NONE);
        c.key(KeyCode::Enter, KeyModifiers::NONE);
        c.key(KeyCode::Char('7'), KeyModifiers::NONE);
        assert_eq!(
            c.values[field_at("timeout_min")],
            "1057",
            "another row puts the cursor after its value"
        );
        assert_eq!(c.values[field_at("budget_usd")], "2");

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
        // With `state` among the columns, the slot before the title is the state; the footer
        // still says own terminal. Without it, the words come back to the row.
        assert_eq!(marker(&data, "aaaa-interactive"), "working");
        assert_eq!(marker(&data, "bbbb-background"), "working");
        data.columns = vec!["model".into()];
        assert_eq!(marker(&data, "aaaa-interactive"), "own terminal");
        assert_eq!(marker(&data, "bbbb-background"), "");
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
        // The orange title marks the orchestrator; the state takes the slot before it.
        assert_eq!(row(&data, "aaaa-worker").cells[2].0.trim(), "working");
        assert_eq!(row(&data, "aaaa-worker").cells[3].1, plain());
        let marked = row(&data, "bbbb-orchestrator");
        assert_eq!(marked.cells[2].0.trim(), "working");
        assert_eq!(marked.cells[3].1, lit());
        // Without a state column the slot names the orchestrator and the terminal.
        data.columns = vec!["model".into()];
        let marked = row(&data, "bbbb-orchestrator");
        assert_eq!(marked.cells[2].0.trim(), "orchestrator");
        assert_eq!(marked.cells[2].1, lit());
        assert_eq!(
            row(&data, "cccc-typed").cells[2].0.trim(),
            "orchestrator · own terminal"
        );
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
    fn the_wizard_runs_once_or_schedules_a_claude_job() {
        let base = dir();
        let mut f = JobForm::new(base.path(), base.path(), None, "");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(
            (f.step, f.error.is_some()),
            (Step::What, true),
            "an empty task stays"
        );
        typed(&mut f, "triage the TODOs");
        assert_eq!(f.error, None, "the next key clears the error");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Where);
        // An empty directory means the placeholder, kept in ~ form.
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::When);
        let canon = base.path().canonicalize().unwrap();
        assert_eq!(f.dir, fleet::tilde(&canon));
        assert_eq!(
            enter(&mut f),
            FormAction::RunOnce("triage the TODOs".into(), canon.clone()),
            "once runs now; no name is asked"
        );
        let shown = f.lines().iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(shown[1].starts_with("new job"), "{shown:?}");
        assert!(shown[3].contains("what   triage the TODOs"), "{shown:?}");
        assert!(shown[5].contains("[once] hourly"), "{shown:?}");
        assert_eq!(shown.len(), 6, "once asks nothing more: {shown:?}");
        for _ in 0..3 {
            f.key(KeyCode::Right, KeyModifiers::NONE);
        }
        assert_eq!(WHEN[f.when], "weekdays");
        assert_eq!(f.lines().len(), 8, "weekdays asks a time and a name");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::At);
        typed(&mut f, "25:00");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert!(f.error.is_some(), "a bad time stays");
        f.at.clear();
        typed(&mut f, "8:30");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(
            (f.step, f.name.as_str()),
            (Step::Name, "triage-the-todos"),
            "the name is suggested from the task"
        );
        // ↑ steps back, and forward again keeps the answers.
        f.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(f.step, Step::At);
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert_eq!(f.step, Step::Name);
        for _ in 0..16 {
            f.key(KeyCode::Backspace, KeyModifiers::NONE);
        }
        typed(&mut f, "bad name");
        assert_eq!(enter(&mut f), FormAction::Stay);
        assert!(f.error.is_some());
        f.name = "nightly".into();
        match enter(&mut f) {
            FormAction::Save(None, job) => {
                assert_eq!(job.name, "nightly");
                assert_eq!(job.harness, None, "the file's default harness applies");
                assert_eq!(job.schedule, "30 8 * * 1-5");
                assert_eq!(job.cwd, PathBuf::from(&f.dir));
                assert_eq!(job.prompt, "triage the TODOs");
                assert_eq!(job.model, None);
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
    fn editing_keeps_the_fields_the_wizard_does_not_ask_about() {
        let base = dir();
        let mut j = config::Job::new("one", "0 9 * * *", Path::new("."), "first");
        j.model = Some("sonnet".into());
        j.budget_usd = Some(0.5);
        let mut f = JobForm::new(base.path(), base.path(), Some(j), "ignored seed");
        assert_eq!(
            (f.name.as_str(), f.dir.as_str(), WHEN[f.when], f.at.as_str()),
            ("one", ".", "daily", "09:00"),
            "the schedule opens on the picks that made it"
        );
        assert!(f.lines()[1].to_string().starts_with("edit one"));
        typed(&mut f, ", revised");
        for _ in 0..4 {
            assert_eq!(enter(&mut f), FormAction::Stay);
        }
        assert_eq!(f.step, Step::Name);
        match enter(&mut f) {
            FormAction::Save(Some(old), job) => {
                assert_eq!(old, "one");
                assert_eq!(job.prompt, "first, revised");
                assert_eq!(job.schedule, "0 9 * * *");
                assert_eq!(job.model.as_deref(), Some("sonnet"));
                assert_eq!(job.budget_usd, Some(0.5));
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

    /// A background session as Claude's daemon lists it: kind `bg` with a job id.
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

    /// A Codex thread the dashboard launched is listed from cones' own record, after the
    /// process table and the registry are read; it still sorts by age among them.
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
        // Forgotten and hidden: the row stays away even if the daemon holds the thread again.
        Ledger::new(&state).unwrap().hide("dddd").unwrap();
        let ids: Vec<String> = fleet_rows(&claude, &state, &[])
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, [A], "a hidden thread has no row");
    }

    /// Folder groups sort by name with case set aside, and a pinned folder nothing runs in
    /// sits among them, not after them; grouped by state it follows the session groups.
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
            ["needs input", "idle", names[0].as_str()],
            "by state the pinned folder trails"
        );
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

    /// A session the read lists for the first time takes the cursor: one opened in another
    /// terminal, or the registry's row taking over a composer placeholder. Not while an
    /// instruction is being typed, since `enter` would then start it somewhere else.
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
        // A composer placeholder is selected at once and followed to the registry's row.
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
        // Moved off the placeholder before the registry lists it, the cursor stays put.
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
        assert_eq!(Kind::Menu.key(), Some("menu"));
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
        app.filter = Input::new("two");
        // The pane off: this is about the frame alone.
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
        // Without the filter, A is back too and C's absence still leaves a selection.
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
        // The pane off: this is about the frame alone.
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
                "enter start job · ctrl+x delete · ctrl+e edit · shift+tab codex · esc back"
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
        // The composer's rules and the hint go red, so the arm is seen, not read.
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
        // Past the window the arm and its hint leave on their own.
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
        // The arm marks the row red; it stays until the next key or `confirm_secs` of none.
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
        // Left alone past the mark's time the row is kept, and the hint says so; the next
        // ctrl+x arms again rather than acting. With `confirm_secs: 0` the mark has no clock.
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

    /// The keys macOS terminals send for option+delete, cmd+delete, cmd+left, cmd+right and
    /// option+left, as `edit` documents them, edit the instruction where the cursor is.
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
        // The pane off: this is about the frame alone.
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
        // The pane off: this is about the frame alone.
        app.split = false;
        app.refresh().unwrap();
        let text = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(text(app.composer()), "✻ claude › Type an instruction…");
        let hint = text(app.hint_line());
        assert!(
            hint.starts_with(
                "enter add folder · ← → pick · shift+tab codex · ctrl+t columns · esc quit"
            ),
            "an empty dashboard opens on the menu row, folder picked: {hint}"
        );
        app.key(KeyCode::BackTab, KeyModifiers::SHIFT).unwrap();
        assert!(text(app.composer()).starts_with(">_ codex › "));
        assert!(text(app.hint_line()).contains("shift+tab claude"));
        app.text = "fix the tests".into();
        assert!(text(app.hint_line()).starts_with("enter start codex in "));
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

    /// The menu sits above the tables: a fresh dashboard opens on the first table and `↑` from
    /// there lands on the menu row, which launches into the dashboard's own directory. The
    /// `folder` prompt adds a row for a directory nothing runs in and moves the cursor onto it,
    /// so a session can start there; the menu's own target does not move.
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
        // The folder sorts by name among the groups, so the menu is one or more steps up.
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
        // ↑ ↓ in the prompt recall the folders sessions have been seen in, newest first.
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

    /// Jobs are off the dashboard: the menu's `jobs` button opens a screen with the jobs as one
    /// table, each with its directory, and a `new job` row that opens the wizard; esc returns.
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
        // The pane off: the jobs screen takes the list's place and keeps the menu row.
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
        // The menu's jobs button: the jobs alone, the cursor on the first, a new job row last.
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

    /// A folder the prompt picks has a row from then on, with nothing running there, across
    /// reloads and restarts, until ctrl+x twice removes it; a session in the folder takes its
    /// group over and the placeholder comes back when the session leaves.
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

        // ctrl+p on a session's row pins its folder, so the folder outlives the session.
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
    fn a_focused_viewer_takes_the_frame_and_ctrl_z_brings_the_composer_back() {
        let d = dir();
        let mut app = app(d.path());
        // The pane off: this is about the frame alone.
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
        assert!(app.status.starts_with("left attach"), "{}", app.status);
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
        // The pane off: this is about the frame alone.
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
        // The counts are on it, cut from their right where the keys begin: at 80 columns the
        // split key now sits beside `tab back` whatever the width, so there is less middle.
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
        // The byte 0x1d, ctrl+] or ctrl+5 to crossterm, goes to the viewer like any other.
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

    /// A session in `state` with `title`, `secs` seconds ago.
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
        // The focused session itself is blocked and the most recent; it is never the alert.
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

        // Too narrow for the alert: it is dropped whole, and the middle is cut from its right.
        // The split is offered at any width, so it is the last key to go.
        let text = app.strip(0, 60).to_string();
        assert!(!text.contains("needs"), "no partial note: {text}");
        assert!(text.starts_with("▲ cones · the one on screen"), "{text}");
        assert!(
            text.trim_end().ends_with("tab back · ctrl+\\ split"),
            "{text}"
        );
        assert_eq!(app.strip(0, 60).width(), 60);

        // Narrower than both ends: `tab back` goes too.
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
        // The pane off: this is about the frame alone.
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

    /// Pump viewer `i` until row 0 of its screen starts with `text`.
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

    /// The cells of row `y` from column `x.start` to `x.end`, as text.
    fn cells(
        t: &Terminal<ratatui::backend::TestBackend>,
        y: u16,
        x: std::ops::Range<u16>,
    ) -> String {
        let buf = t.backend().buffer();
        x.map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
            .collect()
    }

    /// A dashboard on a background session whose viewer is alive but not focused, drawn once
    /// on a frame `width` columns wide and 30 rows tall.
    /// shift+enter on a session row attaches over the whole frame, pane or no pane, and the
    /// pane is back on ctrl+z; the ESC CR Claude Code's terminal bindings send for it,
    /// alt+enter to crossterm, does the same. ctrl+\ inside that viewer puts it beside the
    /// list. Pane off, shift+enter is enter.
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
        // ctrl+\ inside a viewer shift+enter opened goes beside the list, whatever the layout.
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

    /// Beside the list the composer's lower rule lands on the row the harness draws its own
    /// on: the composer keeps as many rows under its box as the harness keeps under its, so
    /// the two input boxes read as one across the frame.
    #[test]
    fn the_composers_rule_lands_on_the_harnesss_own() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        let rule = "\u{2500}".repeat(60);
        // The bottom of a Claude screen: an input box with two rows under it, painted only
        // once the first draw has sized the viewer to the pane, 29 rows of 30.
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            &format!(
                "printf 'VIEW'; read x; \
                 printf '\\033[25;1H{rule}\\033[26;1H> \\033[27;1H{rule}\\033[28;1Hstatus\\033[29;1Hmode'; \
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
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 99));
        app.viewers[0].viewer.write(b"\n");
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.foot_rows() == 1 {
            app.pump();
            assert!(Instant::now() < deadline, "the viewer never drew its box");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            app.foot_rows(),
            3,
            "two rows under the harness's rule, and the key row"
        );
        t.draw(|f| app.draw(f)).unwrap();
        let screen = rows(&t, 200);
        assert_eq!(
            cells(&t, 26, 0..1),
            "\u{2500}",
            "the composer's lower rule is on the harness's row: {screen:#?}"
        );
        assert_eq!(
            cells(&t, 26, 101..105),
            "\u{2500}\u{2500}\u{2500}\u{2500}",
            "which is the row the harness ruled: {screen:#?}"
        );
        assert!(
            cells(&t, 27, 0..100).starts_with("enter"),
            "the hint line is right under it: {screen:#?}"
        );
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
        // LIST = clamp(200 / 2, 60, 100).
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
        // A row of the pane is the viewer's keys, kept clear until it has them, so taking
        // them never resizes it.
        assert_eq!(
            app.viewers[0].viewer.screen().size(),
            (29, 200 - list - 1),
            "the viewer is sized to the pane"
        );
        assert_eq!(app.pane, Rect::new(list + 1, 0, 200 - list - 1, 29));
        let rule = t.backend().buffer().cell((list, 0)).unwrap().clone();
        assert_eq!(rule.symbol(), "│");
        assert_ne!(rule.fg, ORANGE, "the rule is dim while nothing is focused");
        assert!(
            !left[29].contains("ctrl+]") && !left[29].contains("ctrl+\\"),
            "unfocused, the hint line has no pane keys: {:?}",
            left[29]
        );

        // tab bounces into the pane's viewer and back out; enter focuses it too.
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, Some(0));
        app.key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        assert_eq!(app.focus, None, "tab in the viewer comes back to the list");
        app.status.clear();
        app.enter().unwrap();
        assert_eq!(app.focus, Some(0));
        assert_eq!(
            app.viewers[0].viewer.screen().size(),
            (29, 200 - list - 1),
            "focusing beside the list does not resize the viewer"
        );
        t.draw(|f| app.draw(f)).unwrap();
        // The keys that leave the viewer are under the viewer, not across the frame in the
        // list's hint row, which goes empty while the pane has the keys.
        let hint = cells(&t, 29, list + 1..200);
        assert!(hint.contains("tab back"), "{hint:?}");
        assert!(hint.contains("ctrl+\\ full screen"), "{hint:?}");
        assert!(!hint.contains("ctrl+]"), "{hint:?}");
        assert!(
            cells(&t, 29, 0..list).trim().is_empty(),
            "one hint line on the frame: {:?}",
            cells(&t, 29, 0..list)
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
            Some((list + 1, 28)),
            "a release off the frame lands on the viewer's last row, the keys' row below it"
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
            cells(&t, 29, 0..list).starts_with("left attach"),
            "the status has the hint line: {:?}",
            cells(&t, 29, 0..list)
        );
        assert!(
            reversed(&t),
            "unfocused, the composer's block cursor is back"
        );
        // Focused, ctrl+\ picks the full-frame layout: the viewer takes the frame over the
        // strip, the strip offers the split back, and the layout stays after ctrl+z.
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
        // enter takes the frame again; ctrl+\ brings the split back, with the focus kept.
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
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 200 - list - 1));
    }

    #[test]
    fn the_hint_line_drops_keys_from_its_end_to_fit_the_list_column() {
        let d = dir();
        registry_bg(d.path(), A, "/src/one", "idle", 1);
        let mut app = app(d.path());
        app.refresh().unwrap();
        assert_eq!(key(&app).as_deref(), Some(A));
        // With the pane off the line has the whole frame and fits it.
        app.size = (30, 130);
        app.split = false;
        let wide = app.hint_line().to_string();
        assert!(wide.ends_with("ctrl+t columns · esc quit"), "{wide}");
        let keys = |line: &str| line.split(" · ").map(str::to_owned).collect::<Vec<_>>();
        // 140 columns with the pane on: the list column is 70; a long filter in front leaves
        // the keys no room.
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
        // Up past the table lands on the menu, which opens no viewer: the pane has the picked
        // button's screen instead, `folder` on a fresh dashboard.
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

    /// On a wide frame the picked menu button's screen is in the pane while the cursor is on
    /// the row, as a session's viewer would be: enter gives it the keys there, with the list
    /// and its composer still beside it; shift+enter gives it the whole frame; ctrl+z or esc
    /// come back to the list, the pane showing the button again. The jobs screen takes the
    /// cursor into the pane and leaves the menu row on the list with `jobs` pressed.
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
        // enter: the guide has the keys in the pane, the list stays beside it.
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        assert!(matches!(app.mode, Mode::Guide(0)));
        assert!(app.split_active());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(pane(&t).contains("move between rows"), "{}", pane(&t));
        assert!(left(&t).contains(&A[..8]), "{}", left(&t));
        assert!(left(&t).contains("Type an instruction…"), "{}", left(&t));
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
        // shift+enter: the whole frame, as it was before the pane; ctrl+z brings the pane back.
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
        // jobs: the rows and the cursor move into the pane; the list keeps the menu with
        // `jobs` pressed; esc puts the cursor back on the menu row.
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
        // With the pane off the screen takes the list's place, at any width.
        app.split = false;
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(!app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap());
        t.draw(|f| app.draw(f)).unwrap();
        assert!(app.rows.iter().any(|r| r.kind == Kind::Menu));
        assert!(rows(&t, 120).join("\n").contains("new job"));
    }

    /// `tab` hands a button's screen in the pane the keys, as it hands them to a viewer
    /// there: the rule turns orange, the screen's own keys are drawn under the pane where
    /// they are read with it, the list's hint row goes empty rather than saying it twice,
    /// and the two prompt boxes sit on the same rows. `tab` inside the screen belongs to its
    /// form, so ctrl+z is the way back out.
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
        // From under the header, whose own box the pane has no counterpart for.
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
            matches!(app.mode, Mode::Config(_)),
            "tab in the editor is the form's own"
        );
        assert!(!app.key(KeyCode::Char('z'), KeyModifiers::CONTROL).unwrap());
        assert!(matches!(app.mode, Mode::Normal), "ctrl+z comes back out");
    }

    /// The menu is one row of buttons: ← → pick one with nothing typed, only the picked one
    /// explains itself, enter presses it, and `help` is the guide.
    /// The menu's `config` button opens the config editor where the list is: the fields under
    /// their groups, `cones`, `harnesses` and `runs`, one row per field with its value and a
    /// words, the selected field explained under the list. Enter opens a field: typing edits,
    /// ← → pick, enter keeps the value and writes the `defaults` block and the `columns:`
    /// line there and then; a bad value comes back on its field and nothing is written.
    /// `ctrl+o` opens the `SESSION` rows alone as the next session's settings, seeded from the
    /// defaults; `-` reads `system default` there; each field kept takes without writing
    /// the file, the composer's prefix shows them, and the session starts under them.
    #[test]
    fn ctrl_o_picks_the_next_session_s_model_and_provider() {
        let d = dir();
        let mut app = app(d.path());
        app.refresh().unwrap();
        let ctrl = KeyModifiers::CONTROL;
        app.key(KeyCode::Char('o'), ctrl).unwrap();
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = rows(&t, 160).join("\n");
        assert!(s.contains("next session"), "{s}");
        assert!(s.contains("bedrock") && s.contains("codex_model"), "{s}");
        assert!(!s.contains("timeout_min") && !s.contains("notify"), "{s}");
        assert!(
            s.contains("harness            claude"),
            "the harness row shows tab's pick, not the built-in dim: {s}"
        );
        // Down from the last shown row stays; the hidden rows are never visited.
        for _ in 0..7 {
            app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        }
        assert!(matches!(&app.mode, Mode::Config(f) if f.row == field_at("harness")));
        for _ in 0..7 {
            app.key(KeyCode::Up, KeyModifiers::NONE).unwrap();
        }
        assert!(matches!(&app.mode, Mode::Config(f) if f.row == field_at("bedrock")));
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = rows(&t, 160).join("\n");
        assert!(s.contains("bedrock › [system default] false  true "), "{s}");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        // Bedrock with nothing behind it does not take: the cursor lands on the profile it
        // needs, with the reason, and the composer still shows the harness alone.
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        match &app.mode {
            Mode::Config(f) => {
                assert_eq!(f.row, field_at("aws_profile"));
                assert!(
                    f.error
                        .as_deref()
                        .unwrap()
                        .starts_with("aws_profile: needed by bedrock: true"),
                    "{:?}",
                    f.error
                );
            }
            _ => panic!("stays in the form"),
        }
        assert_eq!(app.session_words(), ["claude"], "nothing took");
        // Bounded, so a row that leaves the form fails the test instead of hanging it.
        let go = |app: &mut App, name: &str| {
            for _ in 0..=FIELDS.len() {
                let Mode::Config(f) = &app.mode else {
                    panic!("not in the form, looking for {name}")
                };
                if f.row == field_at(name) {
                    return;
                }
                let code = if f.row < field_at(name) {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                app.key(code, KeyModifiers::NONE).unwrap();
            }
            panic!("{name} is not a row the form visits");
        };
        for (name, text) in [("aws_profile", "claude"), ("aws_region", "us-east-1")] {
            go(&mut app, name);
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            for c in text.chars() {
                app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
            }
            app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        }
        go(&mut app, "model");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        // Each field kept takes on the spot, so esc only closes the form.
        assert_eq!(app.status, "next session: claude · opus · bedrock");
        let p = app.session_policy();
        assert_eq!(
            (p.aws_profile.as_deref(), p.aws_region.as_deref()),
            (Some("claude"), Some("us-east-1")),
            "the session carries what bedrock needs"
        );
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!d.path().join("none.yaml").exists(), "nothing is written");
        let composer: String = app
            .composer()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(composer.contains("› opus · bedrock › "), "{composer}");
        let args = harness::session_args(HarnessKind::Claude, None, "hi", &app.session_policy());
        assert!(args.contains(&"--model".into()) && args.contains(&"opus".into()));
        // Codex shows its own model, none set, and the provider still.
        app.harness = (app.harness + 1) % harness::KNOWN.len();
        assert_eq!(app.session_words(), ["codex", "bedrock"]);
        // The form's harness row carries the pick, back round to claude.
        app.harness = (app.harness + 1) % harness::KNOWN.len();
        app.key(KeyCode::Char('o'), ctrl).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[field_at("harness")] == "claude"));
        go(&mut app, "harness");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert_eq!(app.harness, 1, "codex");
        assert_eq!(app.session_words(), ["codex", "bedrock"]);
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
        assert!(s.contains("timeout_min"), "{s}");
        assert!(s.contains("sparkline.bound"), "{s}");
        assert!(s.contains("time limit (min)"), "{s}");
        // The words beside the rows sit in one column, and the explanation block keeps its
        // height, whichever row is selected.
        let column = |s: &str, what: &str| {
            s.lines()
                .find(|l| l.contains(what))
                .and_then(|l| l.find(what).map(|b| l[..b].chars().count()))
                .unwrap_or_else(|| panic!("{what}: {s}"))
        };
        let height = |s: &str| {
            let mut it = s.lines().skip_while(|l| !l.contains("sparkline.bound"));
            it.next();
            it.take_while(|l| !l.contains("›")).count()
        };
        let (col, tall) = (column(&s, "time limit (min)"), height(&s));
        assert_eq!(column(&s, "count per bar"), col, "{s}");
        assert_eq!(column(&s, "turns per run"), col, "{s}");
        let go = |app: &mut App, name: &str| {
            while let Mode::Config(f) = &app.mode
                && f.row != field_at(name)
            {
                let code = if f.row < field_at(name) {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                app.key(code, KeyModifiers::NONE).unwrap();
            }
        };
        go(&mut app, "sparkline.metric");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(column(&s, "count per bar"), col, "no bounce: {s}");
        assert_eq!(height(&s), tall, "no bounce: {s}");
        assert!(
            s.contains("sparkline.metric › lines  enter edits"),
            "closed, the prompt line shows the value: {s}"
        );
        assert!(s.contains("enter edit"), "{s}");
        assert!(s.contains("esc done"), "{s}");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("sparkline.metric › [-] lines"),
            "open, the options sit on the prompt line: {s}"
        );
        assert!(
            !s.contains("[-] lines  messages  tools  tokens   count per bar"),
            "not in the row: {s}"
        );
        // Right picks lines; esc puts the built-in back and closes the field.
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row] == "lines"));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open && f.values[f.row].is_empty()));
        // bound offers its words and also takes a typed number, which replaces a pick.
        go(&mut app, "sparkline.bound");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("sparkline.bound › [-] fleet  row  log"), "{s}");
        assert!(s.contains("or a number"), "{s}");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row] == "row"));
        for c in "20".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("sparkline.bound › 20"), "typed, no picks: {s}");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open && f.values[f.row] == "20"));
        // model takes a full id the same way.
        go(&mut app, "model");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        for c in "claude-opus-5".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(
            matches!(&app.mode, Mode::Config(f) if !f.open && f.values[f.row] == "claude-opus-5")
        );
        // On a pure pick a letter jumps to its option and - to the built-in.
        go(&mut app, "write");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char('t'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row] == "true"));
        app.key(KeyCode::Char('-'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row].is_empty()));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        go(&mut app, "codex_full_access");
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("no sandbox"), "{s}");
        assert!(s.contains("alias or model id"), "{s}");
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        app.enter().unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("Seconds an armed ctrl+x waits"),
            "the selected field, the first one, is explained: {s}"
        );
        assert!(!s.contains("Maximum cost"), "only the selected one: {s}");
        assert!(s.contains("2.00"), "built-ins show dim: {s}");
        assert!(
            s.matches("system default").count() >= 3,
            "harness-owned fields read system default when empty: {s}"
        );
        let at = |what: &str| s.find(what).unwrap_or_else(|| panic!("{what}: {s}"));
        assert!(
            at("\ncones  the dashboard itself") < at("    confirm_secs")
                && at("    confirm_secs") < at("\n  start")
                && at("\n  start") < at("    start.harness")
                && at("    start.harness") < at("\n  pane")
                && at("\n  pane") < at("    pane.at")
                && at("    pane.at") < at("\n  sparkline")
                && at("\n  sparkline") < at("    sparkline.bars")
                && at("    sparkline.bars") < at("\nharnesses  how claude and codex are run")
                && at("\nharnesses  how claude and codex are run") < at("    bedrock")
                && at("    bedrock") < at("\n  claude")
                && at("\n  claude") < at("    model ")
                && at("    model ") < at("\n  codex ")
                && at("\n  codex ") < at("    codex_model")
                && at("    codex_model") < at("\nruns  every supervised run")
                && at("\nruns  every supervised run") < at("    harness ")
                && at("    harness ") < at("    timeout_min")
                && at("    timeout_min") < at("    notify"),
            "the fields sit under their groups and blocks: {s}"
        );
        go(&mut app, "timeout_min");
        // Closed, typing does nothing.
        app.key(KeyCode::Char('9'), KeyModifiers::NONE).unwrap();
        assert!(matches!(&app.mode, Mode::Config(f) if f.values[f.row].is_empty()));

        // A word where a number goes: the error on its field, which stays open, the file
        // untouched.
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
        // The field that closes on a good value writes the block itself; the editor stays.
        assert!(matches!(&app.mode, Mode::Config(f) if !f.open));
        assert!(app.status.starts_with("config saved"), "{}", app.status);
        assert_eq!(config::defaults(&app.jobs_path).timeout_min, Some(5.0));
        app.key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        for c in "0.25".chars() {
            app.key(KeyCode::Char(c), KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        go(&mut app, "write");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let s = (0..60)
            .map(|y| cells(&t, y, 81..160))
            .chain([cells(&t, 59, 0..80)])
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("write › [-] false  true"), "picks on write: {s}");
        assert!(s.contains("← → pick"), "{s}");
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        go(&mut app, "model");
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char('u'), KeyModifiers::CONTROL).unwrap();
        for _ in 0..3 {
            app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();

        // The columns are ctrl+t's, arranged on the table; the editor has no row for them
        // and a field closing writes the block without touching the line.
        assert_eq!(config::file_columns(&app.jobs_path), None);
        let saved = config::defaults(&app.jobs_path);
        assert_eq!(
            (saved.timeout_min, saved.budget_usd, saved.write),
            (Some(5.0), Some(0.25), Some(true))
        );
        assert_eq!(saved.model.as_deref(), Some("sonnet"));
        assert_eq!(
            saved.max_turns, None,
            "empty leaves the built-in out of the file"
        );

        // esc closes the editor with everything already written, and reopening shows it.
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal), "{}", app.status);
        app.enter().unwrap();
        match &app.mode {
            Mode::Config(f) => assert_eq!(
                ["timeout_min", "budget_usd", "write", "model"]
                    .map(|n| f.values[field_at(n)].as_str()),
                ["5", "0.25", "true", "sonnet"]
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
        // ← from the first button wraps to the last; typed text keeps ← → for the caret.
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
        // Off the menu row nothing explains itself.
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
        // Focused, ctrl+\ flips the layout and keeps the focus.
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split);
        assert_eq!(app.focus, Some(0));
        // crossterm reports the byte 0x1c a terminal sends for ctrl+\ as ctrl+4.
        assert!(!app.key(KeyCode::Char('4'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split);
        assert_eq!(app.focus, Some(0));
        assert!(app.text.is_empty(), "nothing typed into the composer");
        // From the list the key turns the pane off and on, and the list keeps the keys.
        app.unfocus();
        app.status.clear();
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split, "the pane is off");
        assert_eq!(app.focus, None);
        assert!(!app.key(KeyCode::Char('4'), KeyModifiers::CONTROL).unwrap());
        assert!(app.split, "and on again");
        assert!(app.text.is_empty(), "nothing typed into the composer");
        // A narrow frame toggles the same: there is no minimum width.
        app.focus(0);
        app.size = (30, 80);
        app.needs_clear = false;
        assert!(!app.key(KeyCode::Char('\\'), KeyModifiers::CONTROL).unwrap());
        assert!(!app.split, "a narrow frame toggles too");
        assert!(app.needs_clear);
    }

    #[test]
    fn ctrl_t_arranges_the_columns_on_the_table_and_keeps_them_in_jobs_yaml() {
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
        let text = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
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
        // The strip is every column there is: the ones the table draws, in their order, then
        // a separator and the ones it does not.
        app.key(KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(app.mode, Mode::Columns(_)));
        let line = text(app.mode_line());
        assert!(
            line.starts_with("columns ›  state   model   context  ·  age "),
            "{line}"
        );
        let hint = text(app.hint_line());
        assert!(
            hint.contains("space hide") && hint.contains("[ ] move"),
            "{hint}"
        );
        // ] moves a column along the row and the table follows at once, with no save.
        app.key(KeyCode::Right, KeyModifiers::NONE).unwrap();
        app.key(KeyCode::Char(']'), KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["state", "title", "context", "model"]);
        assert_eq!(
            config::columns(&jobs),
            ["state", "model", "context"],
            "the file is untouched until enter"
        );
        // space takes the one under the cursor off the table; a reload mid-pick keeps the
        // arrangement rather than snapping it back to the file.
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["state", "title", "context"]);
        app.refresh().unwrap();
        assert_eq!(names(&app), ["state", "title", "context"]);
        assert!(
            text(app.hint_line()).contains("space show"),
            "under the cursor is off now"
        );
        // enter writes the columns line and leaves the rest of the file where it was.
        app.key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.starts_with("columns saved"), "{}", app.status);
        let text_file = fs::read_to_string(&jobs).unwrap();
        assert!(
            text_file.contains("columns: [state, context]")
                && text_file.contains("confirm_secs: 3"),
            "{text_file}"
        );
        // esc puts back the set it opened on.
        app.key(KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();
        app.key(KeyCode::Char(' '), KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["title", "context"]);
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert_eq!(names(&app), ["state", "title", "context"]);
        assert_eq!(app.status, "columns as they were");
        assert_eq!(config::columns(&jobs), ["state", "context"]);
        // A button's screen in the pane keeps its own prompt line; the strip still takes
        // the list's, over the table it arranges.
        app.size = (30, 200);
        app.split = true;
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        while !matches!(app.selected().map(|r| &r.kind), Some(Kind::Menu)) {
            app.step(-1);
        }
        app.key(KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let frame = (0..30).map(|y| cells(&t, y, 0..100)).collect::<Vec<_>>();
        assert!(
            frame.iter().any(|l| l.contains("columns ›")),
            "the strip is on the list side: {frame:#?}"
        );
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        // The jobs screen's table is not the sessions', so the key says so there.
        app.jobs_view = true;
        app.key(KeyCode::Char('t'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(app.mode, Mode::Normal), "no arranging from there");
        assert!(
            app.status.starts_with("the session columns"),
            "{}",
            app.status
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
        // bottom: the list on top at half the height, a rule row, the pane under it.
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
        // right, at 80 columns: the list has 40, the pane 39. No minimum.
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
        // A wheel on the list is the dashboard's.
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 0);
        // Focused, shift+pgup goes a page back, the pane shows the lines that had left, the
        // cursor is off the frame, and a key comes back to the bottom.
        app.focus(0);
        assert!(!app.key(KeyCode::PageUp, KeyModifiers::SHIFT).unwrap());
        // A page is 28 rows but only twelve lines have left a 29-row pane.
        assert_eq!(app.viewers[0].viewer.screen().scrollback(), 12);
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
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 200 - list - 1));
        // A second click is the viewer's, nothing more.
        app.mouse(click(list + 5, 3));
        assert_eq!(app.focus, Some(0));
        assert!(app.split);
        // In the full-frame layout the list is not on screen, so a click at its old place is
        // the viewer's.
        app.toggle_split();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 0..200).starts_with("VIEW"));
        app.mouse(click(10, 5));
        assert_eq!(app.focus, Some(0));
        // Beside the list again, a click on a row selects it and takes the keys back.
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
        // A second click on the row is not enter.
        app.mouse(click(10, row));
        assert_eq!(app.focus, None);
        assert_eq!(key(&app).as_deref(), Some(A));
        // A click on the hint line selects nothing.
        app.mouse(click(10, 29));
        assert_eq!(key(&app).as_deref(), Some(A));
        assert_eq!(app.focus, None);
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
        // Esc leaves the transcript alone.
        app.key(KeyCode::Char('n'), KeyModifiers::CONTROL).unwrap();
        assert!(matches!(&app.mode, Mode::Rename(i) if i.text == "Ship it"));
        app.key(KeyCode::Esc, KeyModifiers::NONE).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read_to_string(&transcript).unwrap(), text);
    }

    #[test]
    fn the_pane_stays_blank_for_a_session_in_its_own_terminal() {
        let d = dir();
        // An interactive Claude runs in its own terminal: nothing is coming to the pane.
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
        // A Claude background session the resting cursor will open: nothing, not the
        // transcript and not the hint, since the screen is on its way.
        t.draw(|f| app.draw(f)).unwrap();
        assert!(blank(&t));
        // The speculative viewer spawned and has not painted: still nothing, sized to the pane.
        app.viewers.push(speculative_open(A));
        t.draw(|f| app.draw(f)).unwrap();
        assert!(blank(&t));
        assert_eq!(app.viewers[0].viewer.screen().size(), (29, 99));
        // Once it paints, the screen is there.
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
        // A's viewer painted and was the one focused last.
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
        // The new Codex row took the cursor; back on A for the viewer.
        app.step(-1);
        assert_eq!(key(&app).as_deref(), Some(A));
        // A's viewer painted and was the one focused last.
        app.viewers.push(viewer_open(A, "attach", "VIEW"));
        wait_paint(&mut app, 0, "VIEW");
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(200, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        assert!(cells(&t, 0, 101..200).starts_with("VIEW"));
        // The cursor moves onto the Codex row, which has no viewer and no transcript: the pane
        // shows nothing of A.
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
        // A real viewer can still take the pane once it has painted.
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

    /// `silent_open` marked as opened ahead of `enter`.
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
        // The pane off: this is about the frame alone.
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
        // A stop in flight on the row, and a removal the registry has not caught up with.
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

    /// A client the user walked into Claude's own agent view is not the session's screen, so
    /// leaving it drops the viewer instead of parking that list in the pane; a client still in
    /// its session stays alive for `enter` to return to.
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
        assert!(app.status.contains("agent view"), "{}", app.status);
        titled(&mut app, "◑ a session");
        app.unfocus();
        assert_eq!(app.viewers.len(), 1, "a session's client stays alive");
        assert!(app.status.contains("enter returns to it"), "{}", app.status);
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
        // The oldest by focus time is the first one, older than the speculative viewer.
        app.viewers[0].last_focused = Instant::now() - Duration::from_secs(60);
        let mut c = Command::new("/bin/sleep");
        c.arg("5");
        app.open((12, 80), c, "attach", "four".into(), None);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["two", "three", A, "four"], "{keys:?}");
        assert!(app.viewers[2].speculative, "the speculative one survived");
        assert_eq!(app.live_viewers(), MAX_VIEWERS);
        assert_eq!(app.focus, Some(3));
    }

    /// A Codex client opened with `enter` has no quiet way back, so a fourth viewer closes the
    /// oldest attach around it, and with no attach to close the cap is exceeded, not the client.
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
        // Entering a speculative attach evicts the one attach, never a client.
        app.viewers.push(speculative_open(A));
        app.focus(4);
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["codex-1", "codex-2", "codex-3", A], "{keys:?}");
        assert_eq!(app.focus, Some(3));
    }

    /// The speculative viewer did not count while hidden; once `enter` takes it, the limit
    /// holds again, and the least recently focused viewer goes, not the one just entered.
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
        assert_eq!(app.live_viewers(), MAX_VIEWERS);
    }

    /// The same with the speculative viewer last, so closing shifts its index.
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
        // The loop's turn after the reload landed; the rest is fresh, so nothing spawns.
        app.prespawn_tick();
        let keys: Vec<&str> = app.viewers.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![B],
            "the speculative viewer went; a viewer the user has been in stays"
        );
    }
}
