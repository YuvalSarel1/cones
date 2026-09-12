//! Minimal live dashboard: jobs, runs, and the selected run's output.
use crate::{
    config::{self, ResolvedJob},
    ledger::{Ledger, Run},
    output, runner,
};
use anyhow::Result;
use chrono::Utc;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Jobs,
    Runs,
}

struct App {
    exe: PathBuf,
    jobs_path: PathBuf,
    state: PathBuf,
    ledger: Ledger,
    jobs: Vec<ResolvedJob>,
    jobs_error: Option<String>,
    runs: Vec<Run>,
    pane: Pane,
    jobs_state: TableState,
    runs_state: TableState,
    children: Vec<Child>,
    notice: String,
}

pub fn run(jobs_path: &Path, state: &Path) -> Result<()> {
    let mut app = App {
        exe: std::env::current_exe()?,
        jobs_path: jobs_path.to_owned(),
        state: state.to_owned(),
        ledger: Ledger::new(state)?,
        jobs: Vec::new(),
        jobs_error: None,
        runs: Vec::new(),
        pane: Pane::Runs,
        jobs_state: TableState::default().with_selected(0),
        runs_state: TableState::default().with_selected(0),
        children: Vec::new(),
        notice: String::new(),
    };
    app.refresh();
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(1000))?
                && let Event::Key(key) = event::read()?
                && key.is_press()
            {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Char('q') || ctrl_c {
                    return Ok(());
                }
                app.key(key.code, &mut terminal)?;
            }
            app.refresh();
        }
    })();
    ratatui::restore();
    result
}

impl App {
    fn refresh(&mut self) {
        match config::read_jobs(&self.jobs_path) {
            Ok(jobs) => {
                self.jobs = jobs;
                self.jobs_error = None;
            }
            Err(e) => self.jobs_error = Some(format!("{e:#}")),
        }
        if let Ok(mut runs) = self.ledger.runs() {
            runs.reverse();
            self.runs = runs;
        }
        self.children
            .retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        clamp(&mut self.jobs_state, self.jobs.len());
        clamp(&mut self.runs_state, self.runs.len());
    }

    fn selected_run(&self) -> Option<&Run> {
        self.runs_state.selected().and_then(|i| self.runs.get(i))
    }

    fn selected_job(&self) -> Option<String> {
        match self.pane {
            Pane::Jobs => self
                .jobs_state
                .selected()
                .and_then(|i| self.jobs.get(i))
                .map(|j| j.name.clone()),
            Pane::Runs => self.selected_run().and_then(|r| r.started.job.clone()),
        }
    }

    fn key(&mut self, code: KeyCode, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let (state, len) = match self.pane {
            Pane::Jobs => (&mut self.jobs_state, self.jobs.len()),
            Pane::Runs => (&mut self.runs_state, self.runs.len()),
        };
        match code {
            KeyCode::Tab => {
                self.pane = if self.pane == Pane::Jobs {
                    Pane::Runs
                } else {
                    Pane::Jobs
                }
            }
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                state.select(Some((state.selected().unwrap_or(0) + 1).min(len - 1)))
            }
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
            }
            KeyCode::Char('r') | KeyCode::Enter
                if self.pane == Pane::Jobs || code == KeyCode::Char('r') =>
            {
                if let Some(job) = self.selected_job() {
                    let child = self
                        .cones()
                        .args(["run", &job])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .process_group(0)
                        .spawn()?;
                    self.children.push(child);
                    self.notice = format!("started {job}");
                    self.pane = Pane::Runs;
                    self.runs_state.select(Some(0));
                }
            }
            KeyCode::Char('s') => {
                if let Some(id) = self.selected_run().map(|r| r.started.run_id.clone()) {
                    self.notice = match runner::stop(&self.ledger, &id) {
                        Ok(true) => format!("stop requested for {}", &id[..8]),
                        Ok(false) => format!("{} already finished", &id[..8]),
                        Err(e) => format!("stop failed: {e:#}"),
                    };
                }
            }
            KeyCode::Char('a') | KeyCode::Char('l') => {
                if let Some(id) = self.selected_run().map(|r| r.started.run_id.clone()) {
                    let args: &[&str] = if code == KeyCode::Char('a') {
                        &["attach", &id]
                    } else {
                        &["logs", &id, "--follow"]
                    };
                    ratatui::restore();
                    let status = self.cones().args(args).status();
                    *terminal = ratatui::init();
                    self.notice = match status {
                        Ok(s) if s.success() => String::new(),
                        Ok(s) => format!("{} exited with {s}", args[0]),
                        Err(e) => format!("{} failed: {e}", args[0]),
                    };
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn cones(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("--jobs")
            .arg(&self.jobs_path)
            .arg("--state-dir")
            .arg(&self.state);
        c
    }

    fn draw(&mut self, f: &mut Frame) {
        let jobs_h = (self.jobs.len().max(1) as u16 + 3).min(9);
        let [jobs_area, runs_area, out_area, help_area] = Layout::vertical([
            Constraint::Length(jobs_h),
            Constraint::Percentage(45),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_jobs(f, jobs_area);
        self.draw_runs(f, runs_area);
        self.draw_output(f, out_area);
        let help = if self.notice.is_empty() {
            "tab pane  j/k move  r run  s stop  l logs  a attach  q quit".to_owned()
        } else {
            self.notice.clone()
        };
        f.render_widget(
            Paragraph::new(help).style(Style::new().fg(Color::DarkGray)),
            help_area,
        );
    }

    fn draw_jobs(&mut self, f: &mut Frame, area: Rect) {
        let last: std::collections::HashMap<&str, &Run> = self
            .runs
            .iter()
            .rev()
            .filter_map(|r| r.started.job.as_deref().map(|j| (j, r)))
            .collect();
        let rows = self.jobs.iter().map(|j| {
            let status = last
                .get(j.name.as_str())
                .map_or("-".to_owned(), |r| r.status());
            Row::new(vec![
                j.name.clone(),
                j.schedule.clone(),
                j.harness.to_string(),
                if j.enabled { "on".into() } else { "off".into() },
                status.clone(),
            ])
            .style(status_style(&status))
        });
        let title = match &self.jobs_error {
            Some(e) => format!(" jobs: {e} "),
            None => format!(" jobs ({}) ", self.jobs_path.display()),
        };
        let table = Table::new(
            rows,
            [
                Constraint::Min(16),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(4),
                Constraint::Length(9),
            ],
        )
        .header(Row::new(["job", "schedule", "harness", "", "last"]).style(header()))
        .block(block(title, self.pane == Pane::Jobs))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, area, &mut self.jobs_state);
    }

    fn draw_runs(&mut self, f: &mut Frame, area: Rect) {
        let rows = self.runs.iter().map(|r| {
            let last = r.terminal.as_ref().unwrap_or(&r.started);
            let status = r.status();
            Row::new(vec![
                r.started.job.clone().unwrap_or_else(|| "-".into()),
                status.clone(),
                last.reason.clone().unwrap_or_default(),
                r.started.fired_at.map(ago).unwrap_or_default(),
                last.duration_s
                    .or_else(|| {
                        (status == "started")
                            .then_some(r.started.fired_at)
                            .flatten()
                            .map(|t| (Utc::now() - t).num_seconds() as f64)
                    })
                    .map(|d| format!("{d:.0}s"))
                    .unwrap_or_default(),
                last.cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_default(),
                r.started.run_id[..8].to_owned(),
            ])
            .style(status_style(&status))
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(16),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(["job", "status", "reason", "fired", "took", "cost", "run"]).style(header()),
        )
        .block(block(
            format!(" runs ({}) ", self.runs.len()),
            self.pane == Pane::Runs,
        ))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, area, &mut self.runs_state);
    }

    fn draw_output(&self, f: &mut Frame, area: Rect) {
        let (title, lines) = match self.selected_run() {
            Some(r) => {
                let mut lines =
                    output::snapshot(r.started.output.as_deref(), r.started.stderr.as_deref());
                if lines.is_empty() {
                    lines.push("no captured output".into());
                }
                // ponytail: show the tail that fits; scrolling comes when someone needs it.
                let keep = area.height.saturating_sub(2) as usize;
                let skip = lines.len().saturating_sub(keep);
                (
                    format!(" output {} ", &r.started.run_id[..8]),
                    lines.split_off(skip),
                )
            }
            None => (
                " output ".to_owned(),
                vec!["no runs yet; press r on a job".to_owned()],
            ),
        };
        let text: Vec<Line> = lines.into_iter().map(Line::from).collect();
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(block(title, false)),
            area,
        );
    }
}

fn clamp(state: &mut TableState, len: usize) {
    if len == 0 {
        state.select(None);
    } else if state.selected().is_none_or(|i| i >= len) {
        state.select(Some(len - 1));
    }
}

fn ago(t: chrono::DateTime<Utc>) -> String {
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

fn status_style(status: &str) -> Style {
    Style::new().fg(match status {
        "ok" => Color::Green,
        "started" => Color::Cyan,
        "skipped" => Color::Yellow,
        "failed" | "timeout" | "crashed" => Color::Red,
        _ => Color::Reset,
    })
}

fn header() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn block(title: String, focused: bool) -> Block<'static> {
    let b = Block::default().borders(Borders::ALL).title(title);
    if focused {
        b.border_style(Style::new().fg(Color::Blue))
    } else {
        b
    }
}
