//! `cones tui` is fzf over the ledger. fzf owns navigation, filtering and keys;
//! cones only supplies the list, the preview and the actions.
use crate::{config, ledger::Ledger};
use anyhow::{Context, Result};
use std::{
    path::Path,
    process::{Command, Stdio},
};

/// Lines for fzf: hidden key (`job` or run UUID), hidden aux (job name or run status),
/// then the display columns. Only the third field is shown.
pub fn list(jobs_path: &Path, state: &Path) -> Result<String> {
    let mut out = String::new();
    let runs = Ledger::new(state)?.runs()?;
    if let Ok(jobs) = config::read_jobs(jobs_path) {
        for j in jobs {
            let last = runs
                .iter()
                .rev()
                .find(|r| r.started.job.as_deref() == Some(&j.name))
                .map_or("-".to_owned(), |r| r.status());
            out += &format!(
                "job\t{}\t{:<24} {:<16} {:<8} {:<8} last: {last}\n",
                j.name,
                j.name,
                j.schedule,
                j.harness,
                if j.enabled { "on" } else { "off" }
            );
        }
    }
    for r in runs.iter().rev() {
        let last = r.terminal.as_ref().unwrap_or(&r.started);
        out += &format!(
            "{}\t{}\t{:<24} {:<8} {:<16} {:<8} {:<9} {}\n",
            r.started.run_id,
            r.status(),
            r.started.job.as_deref().unwrap_or("-"),
            r.status(),
            r.started
                .fired_at
                .map(|t| t.format("%m-%d %H:%M:%S").to_string())
                .unwrap_or_default(),
            last.duration_s
                .map(|d| format!("{d:.0}s"))
                .unwrap_or_default(),
            last.cost_usd
                .map(|c| format!("${c:.4}"))
                .unwrap_or_default(),
            last.reason.as_deref().unwrap_or("")
        );
    }
    Ok(out)
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
            "--no-sort",
            "--layout=reverse",
            "--header=enter: run job / view logs   ctrl-s: stop   ctrl-a: attach   ctrl-r: refresh   esc: quit",
            "--preview-window=down,60%,wrap",
            &format!("--preview=[ {{1}} = job ] && {me} ls --job {{2}} || {me} logs {{1}}"),
            &format!("--bind=start:{reload}"),
            &format!("--bind=ctrl-r:{reload}"),
            // Pick the action by row kind so the screen is only cleared when something interactive runs.
            &format!(
                "--bind=enter:transform:case {{1}}/{{2}} in job/*) echo \"execute-silent({me} run {{2}} >/dev/null 2>&1 &)+{reload}\";; */started) echo \"execute({me} logs {{1}} --follow)+{reload}\";; *) echo \"execute({me} logs {{1}} | less -R)\";; esac"
            ),
            &format!("--bind=ctrl-s:execute-silent([ {{1}} = job ] || {me} stop {{1}})+{reload}"),
            &format!(
                "--bind=ctrl-a:execute([ {{1}} = job ] || {me} attach {{1}} || {{ printf 'press enter'; read _; }})+{reload}"
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
