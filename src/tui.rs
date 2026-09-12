//! `cones tui` is fzf over the ledger. fzf owns navigation, filtering and keys;
//! cones only supplies the list, the preview and the actions.
use crate::{
    config,
    fleet::{self, Session},
    ledger::{Ledger, Run},
};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    path::Path,
    process::{Command, Stdio},
};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";
const ORANGE: &str = "\x1b[38;5;208m";
const WHITE: &str = "\x1b[97m";

/// Lines for fzf: hidden key (`job`, `hdr`, session UUID or run UUID), hidden aux (job name,
/// session state or run status), then the display text. Only the third field is shown.
/// The first line is the summary fzf pins as a header; `hdr` rows are section titles.
pub fn list(jobs_path: &Path, state: &Path) -> Result<String> {
    let runs = Ledger::new(state)?.runs()?;
    let jobs = config::read_jobs(jobs_path).unwrap_or_default();
    let sessions = fleet_rows(state, &runs)?;
    let count = |st: &str| sessions.iter().filter(|s| s.state == st).count();
    // Three pinned header lines: a pixel cone, the summary beside it, the keys below.
    let mut out = format!(
        "hdr\t-\t{ORANGE}  ▲  {RESET}  {BOLD}cones{RESET}\n\
         hdr\t-\t{WHITE} ▟█▙ {RESET}  {} working · {} need input · {} idle · {} jobs · {} runs\n\
         hdr\t-\t{ORANGE}▟███▙{RESET}  {DIM}enter run job / follow logs · ctrl-s stop · ctrl-a attach · ctrl-r refresh · esc quit{RESET}\n",
        count("active"),
        count("blocked"),
        count("idle"),
        jobs.len(),
        runs.len()
    );

    if !jobs.is_empty() {
        let _ = writeln!(out, "hdr\t-\t\nhdr\t-\t{BOLD}jobs{RESET}");
        let rows = jobs
            .iter()
            .map(|j| {
                let last = runs
                    .iter()
                    .rev()
                    .find(|r| r.started.job.as_deref() == Some(&j.name))
                    .map_or("-".to_owned(), |r| r.status());
                vec![
                    (if j.enabled { "◆" } else { "◇" }.into(), color(&last)),
                    (j.name.clone(), ""),
                    (j.schedule.clone(), DIM),
                    (j.harness.to_string(), DIM),
                    (if j.enabled { "on" } else { "off" }.into(), ""),
                    (format!("last: {last}"), color(&last)),
                ]
            })
            .collect();
        for (j, line) in jobs.iter().zip(table(rows)) {
            let _ = writeln!(out, "job\t{}\t  {line}", j.name);
        }
    }

    // Sessions grouped by directory, like Claude's own agents view.
    let mut by_dir: BTreeMap<String, Vec<&Session>> = BTreeMap::new();
    for s in &sessions {
        by_dir.entry(fleet::tilde(&s.cwd)).or_default().push(s);
    }
    for (dir, group) in by_dir {
        let _ = writeln!(out, "hdr\t-\t\nhdr\t-\t{BOLD}{dir}{RESET}");
        let rows = group
            .iter()
            .map(|s| {
                vec![
                    (icon(&s.state).into(), color(&s.state)),
                    (
                        s.title
                            .clone()
                            .unwrap_or_else(|| s.session_id.chars().take(8).collect()),
                        "",
                    ),
                    (label(&s.state).into(), color(&s.state)),
                    (fleet::age(s.updated), DIM),
                    (fleet::tokens(s), DIM),
                    (
                        s.last.as_deref().map(|l| clip(l, 100)).unwrap_or_default(),
                        DIM,
                    ),
                ]
            })
            .collect();
        for (s, line) in group.iter().zip(table(rows)) {
            let _ = writeln!(out, "{}\t{}\t  {line}", s.session_id, s.state);
        }
    }

    if !runs.is_empty() {
        let _ = writeln!(out, "hdr\t-\t\nhdr\t-\t{BOLD}runs{RESET}");
        let rows = runs
            .iter()
            .rev()
            .map(|r| {
                let last = r.terminal.as_ref().unwrap_or(&r.started);
                let status = r.status();
                vec![
                    (icon(&status).into(), color(&status)),
                    (r.started.job.clone().unwrap_or_else(|| "-".into()), ""),
                    (status.clone(), color(&status)),
                    (
                        r.started
                            .fired_at
                            .map(|t| t.format("%m-%d %H:%M:%S").to_string())
                            .unwrap_or_default(),
                        DIM,
                    ),
                    (
                        last.duration_s
                            .map(|d| format!("{d:.0}s"))
                            .unwrap_or_default(),
                        DIM,
                    ),
                    (
                        last.cost_usd
                            .map(|c| format!("${c:.4}"))
                            .unwrap_or_default(),
                        DIM,
                    ),
                    (last.reason.clone().unwrap_or_default(), DIM),
                ]
            })
            .collect();
        for (r, line) in runs.iter().rev().zip(table(rows)) {
            let _ = writeln!(out, "{}\t{}\t  {line}", r.started.run_id, r.status());
        }
    }
    Ok(out)
}

/// Preview for a fleet session: its facts, then what the assistant said recently.
pub fn show(state: &Path, session_id: &str) -> Result<String> {
    let s = fleet::find(state, session_id)?.context("unknown session")?;
    let mut out = format!(
        "{BOLD}{}{RESET}\n{}\n{} {} · {} · {} tokens · pid {}\n",
        s.title.as_deref().unwrap_or(&s.session_id),
        fleet::tilde(&s.cwd),
        label(&s.state),
        s.event.as_deref().unwrap_or(""),
        fleet::age(s.updated),
        fleet::tokens(&s),
        s.pid.map(|p| p.to_string()).unwrap_or_default()
    );
    if let Some(t) = &s.transcript_path {
        for text in fleet::tail(t, 8).1 {
            let _ = write!(out, "\n· {text}");
        }
    }
    Ok(out)
}

/// Pad each column to its widest cell; the style wraps the text so it does not count.
fn table(rows: Vec<Vec<(String, &str)>>) -> Vec<String> {
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
            let mut line = String::new();
            for (c, (text, style)) in r.into_iter().enumerate() {
                let pad = widths[c] - text.chars().count();
                let style = if text.is_empty() { "" } else { style };
                let _ = write!(line, "{style}{text}{RESET}{:pad$}  ", "");
            }
            line.trim_end().to_owned()
        })
        .collect()
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

fn label(state: &str) -> &str {
    match state {
        "active" => "working",
        "blocked" => "needs input",
        s => s,
    }
}

fn color(status: &str) -> &'static str {
    match status {
        "active" | "started" | "ok" => GREEN,
        "blocked" | "skipped" => YELLOW,
        "idle" | "exited" | "-" => DIM,
        _ => RED,
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
        .filter(|s| !owned.contains(s.session_id.as_str()) && s.pid.is_none_or(alive))
        .collect())
}

fn alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means it exists under another user.
    unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM }
}

pub fn run(exe: &Path, jobs_path: &Path, state: &Path) -> Result<i32> {
    let me = format!(
        "{} --jobs {} --state-dir {}",
        sh(exe),
        sh(jobs_path),
        sh(state)
    );
    let reload = format!("reload({me} __list)");
    let status = Command::new("fzf")
        .args([
            "--delimiter=\t",
            "--with-nth=3",
            "--ansi",
            "--no-sort",
            "--layout=reverse",
            "--header-lines=3",
            "--info=inline-right",
            "--prompt=  ",
            "--pointer=▌",
            "--color=header:-1,pointer:208,fg+:bold,bg+:-1,gutter:-1",
            "--preview-window=down,40%,wrap",
            &format!(
                "--preview=case {{1}} in hdr) ;; job) {me} ls --job {{2}};; *) {me} logs {{1}} 2>/dev/null || {me} __show {{1}};; esac"
            ),
            &format!("--bind=start:{reload}"),
            &format!("--bind=ctrl-r:{reload}"),
            // Pick the action by row kind so the screen is only cleared when something interactive runs.
            &format!(
                "--bind=enter:transform:case {{1}}/{{2}} in hdr/*) ;; job/*) echo \"execute-silent({me} run {{2}} >/dev/null 2>&1 &)+{reload}\";; */started) echo \"execute({me} logs {{1}} --follow)+{reload}\";; */active|*/idle|*/blocked|*/exited) echo \"{reload}\";; *) echo \"execute({me} logs {{1}} | less -R)\";; esac"
            ),
            &format!(
                "--bind=ctrl-s:execute-silent(case {{1}} in hdr|job) ;; *) {me} stop {{1}};; esac)+{reload}"
            ),
            &format!(
                "--bind=ctrl-a:execute(case {{1}} in hdr|job) ;; *) {me} attach {{1}} || {{ printf 'press enter'; read _; }};; esac)+{reload}"
            ),
        ])
        .stdin(Stdio::null())
        .status()
        .context("fzf is required for the dashboard: brew install fzf")?;
    // fzf exits 130 on esc and 1 on an empty list; neither is a cones failure.
    Ok(if status.code().is_some_and(|c| c > 1 && c != 130) {
        1
    } else {
        0
    })
}

fn sh(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}
