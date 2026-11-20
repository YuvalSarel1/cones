//! Linux scheduling: one systemd user timer and service per job, and a login service for
//! catch-up, mirroring the LaunchAgents `launchd` installs on macOS. The cron reading, the
//! catch-up window and the next-tick math stay in `launchd`; this module only writes units and
//! drives `systemctl --user`.
use crate::{
    config::{CatchUp, ResolvedJob},
    harness,
    launchd::{CATCHUP, calendar_intervals},
    private_file,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Local};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

/// The unit name stem for a job: `cones-<job>.service` and `cones-<job>.timer`.
pub fn unit(job: &str) -> String {
    format!("cones-{job}")
}

/// The key every cones unit carries in `[Unit]`, naming its job. systemd ignores `X-` keys, and
/// a file under the cones name without it belongs to someone else.
const OWNER: &str = "X-Cones-Job";

/// systemd's `OnCalendar` weekday names, Sunday first as cron and launchd count.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// One `OnCalendar` line per launchd interval. launchd ORs Day and Weekday when both are set
/// and systemd ANDs them, so such an interval becomes two lines.
fn on_calendar(schedule: &str) -> Result<Vec<String>> {
    let mut lines = BTreeSet::new();
    for interval in calendar_intervals(schedule)? {
        let at = |key: &str, width: usize| match interval.get(key) {
            Some(n) => format!("{n:0width$}"),
            None => "*".into(),
        };
        let time = format!("{}:{}:00", at("Hour", 2), at("Minute", 2));
        let date = |day: String| format!("*-{}-{day}", at("Month", 2));
        let weekday = interval.get("Weekday").map(|&w| WEEKDAYS[w as usize]);
        match (interval.contains_key("Day"), weekday) {
            (true, Some(weekday)) => {
                lines.insert(format!("{} {time}", date(at("Day", 2))));
                lines.insert(format!("{weekday} {} {time}", date("*".into())));
            }
            (_, Some(weekday)) => {
                lines.insert(format!("{weekday} {} {time}", date(at("Day", 2))));
            }
            (_, None) => {
                lines.insert(format!("{} {time}", date(at("Day", 2))));
            }
        }
    }
    Ok(lines.into_iter().collect())
}

/// A value systemd reads literally: `%` starts a specifier, and a line break would end the
/// setting early, so it is refused rather than escaped.
fn literal(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\n', '\r']),
        "systemd unit values cannot hold line breaks: {value:?}"
    );
    Ok(value.replace('%', "%%"))
}

/// One double-quoted word of `ExecStart=` or `Environment=`.
fn quoted(value: &str) -> Result<String> {
    let value = literal(value)?.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{value}\""))
}

/// `ExecStart=` words, where `$` would also expand a variable.
fn exec(args: &[String]) -> Result<String> {
    Ok(args
        .iter()
        .map(|a| quoted(&a.replace('$', "$$")))
        .collect::<Result<Vec<_>>>()?
        .join(" "))
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn service(
    name: &str,
    description: &str,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    state: &Path,
    install: Option<&str>,
) -> Result<String> {
    let mut text = format!(
        "[Unit]\nDescription={}\n{OWNER}={name}\n\n[Service]\nType=oneshot\nExecStart={}\n\
         WorkingDirectory={}\n",
        literal(description)?,
        exec(args)?,
        literal(&path(cwd))?,
    );
    for (key, value) in environment {
        text += &format!("Environment={}\n", quoted(&format!("{key}={value}"))?);
    }
    // timeout_min is the only limit cones puts on a run; systemd adds none of its own.
    text += "TimeoutStartSec=infinity\n";
    for (key, suffix) in [("StandardOutput", "out"), ("StandardError", "err")] {
        let log = state.join("logs").join(format!("{name}.{suffix}.log"));
        text += &format!("{key}=append:{}\n", literal(&path(&log))?);
    }
    if let Some(target) = install {
        text += &format!("\n[Install]\nWantedBy={target}\n");
    }
    Ok(text)
}

fn arguments(exe: &Path, jobs_file: &Path, state: &Path, tail: &[&str]) -> Vec<String> {
    [path(exe), "--jobs".into(), path(jobs_file)]
        .into_iter()
        .chain(["--state-dir".into(), path(state)])
        .chain(tail.iter().map(|s| s.to_string()))
        .collect()
}

/// A job's service and timer. The service runs one tick; the timer fires it. Catch-up is
/// cones' own policy, so the timer is not `Persistent`.
pub fn render(
    job: &ResolvedJob,
    exe: &Path,
    jobs_file: &Path,
    state: &Path,
) -> Result<[(String, String); 2]> {
    ensure!(
        exe.is_absolute() && jobs_file.is_absolute() && state.is_absolute(),
        "systemd paths must be absolute"
    );
    let args = arguments(
        exe,
        jobs_file,
        state,
        &["run", &job.name, "--trigger", "schedule"],
    );
    let service = service(
        &job.name,
        &format!("cones job {}", job.name),
        &args,
        &job.cwd,
        &harness::environment(job)?,
        state,
        None,
    )?;
    let mut timer = format!(
        "[Unit]\nDescription=cones schedule {}\n{OWNER}={}\n\n[Timer]\n",
        literal(&job.name)?,
        job.name
    );
    for line in on_calendar(&job.schedule)? {
        timer += &format!("OnCalendar={line}\n");
    }
    // systemd batches timers within a minute by default; a cron tick fires on its second.
    timer += "AccuracySec=1s\n\n[Install]\nWantedBy=timers.target\n";
    let unit = unit(&job.name);
    Ok([
        (format!("{unit}.service"), service),
        (format!("{unit}.timer"), timer),
    ])
}

/// The login service: it decides which jobs slept through a tick and starts their services, so
/// each run still carries the environment `install` captured for its job.
pub fn render_catchup(exe: &Path, jobs_file: &Path, state: &Path) -> Result<(String, String)> {
    ensure!(
        exe.is_absolute() && jobs_file.is_absolute() && state.is_absolute(),
        "systemd paths must be absolute"
    );
    let args = arguments(exe, jobs_file, state, &[CATCHUP]);
    let service = service(
        CATCHUP,
        "cones catch-up",
        &args,
        state,
        &BTreeMap::new(),
        state,
        Some("default.target"),
    )?;
    Ok((format!("{}.service", unit(CATCHUP)), service))
}

fn units_dir() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir()
            .context("missing home directory")?
            .join(".config"),
    };
    Ok(config.join("systemd/user"))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("systemctl --user; scheduling on Linux needs a systemd user session")?;
    ensure!(
        output.status.success(),
        "systemctl --user {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Write a unit file only when its bytes change. Returns whether it did.
fn write_unit(dir: &Path, file: &str, text: &str) -> Result<bool> {
    let path = dir.join(file);
    if fs::read(&path).is_ok_and(|existing| existing == text.as_bytes()) {
        return Ok(false);
    }
    let tmp = dir.join(format!(".cones-{}.tmp", uuid::Uuid::new_v4()));
    let mut f = private_file(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    fs::rename(tmp, &path)?;
    Ok(true)
}

/// Whether systemd has the unit enabled, so an unchanged unit is left alone.
fn enabled(file: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "--quiet", "is-enabled", file])
        .status()
        .is_ok_and(|s| s.success())
}

pub fn install(
    jobs: &[ResolvedJob],
    exe: &Path,
    jobs_path: &Path,
    state: &Path,
    dry_run: bool,
) -> Result<()> {
    let prepared: Vec<_> = jobs
        .iter()
        .filter(|j| j.enabled)
        .map(|job| {
            harness::adapter(job.harness)?.compile(job)?;
            Ok((job, render(job, exe, jobs_path, state)?))
        })
        .collect::<Result<_>>()?;
    let catchup = jobs
        .iter()
        .any(|j| j.enabled && j.catch_up != CatchUp::Skip);
    if dry_run {
        if prepared.iter().any(|(job, _)| !job.env.is_empty()) {
            eprintln!(
                "cones: warning: dry-run units include values of named environment variables and may contain secrets"
            );
        }
        for (_, units) in &prepared {
            for (file, text) in units {
                print!("# {file}\n{text}\n");
            }
        }
        if catchup {
            let (file, text) = render_catchup(exe, jobs_path, state)?;
            print!("# {file}\n{text}\n");
        }
        return Ok(());
    }
    crate::private_dir(state)?;
    crate::private_dir(&state.join("logs"))?;
    let dir = units_dir()?;
    fs::create_dir_all(&dir)?;
    let mut changed = Vec::new();
    for (job, units) in &prepared {
        let mut any = false;
        for (file, text) in units {
            any |= write_unit(&dir, file, text)?;
        }
        for suffix in ["out", "err"] {
            private_file(
                &state
                    .join("logs")
                    .join(format!("{}.{suffix}.log", job.name)),
            )?;
        }
        changed.push((job.name.as_str(), format!("{}.timer", unit(&job.name)), any));
    }
    let login = format!("{}.service", unit(CATCHUP));
    if catchup {
        let (file, text) = render_catchup(exe, jobs_path, state)?;
        let written = write_unit(&dir, &file, &text)?;
        for suffix in ["out", "err"] {
            private_file(&state.join("logs").join(format!("{CATCHUP}.{suffix}.log")))?;
        }
        changed.push((CATCHUP, file, written));
    }
    let stale: Vec<String> = stale_jobs(installed_jobs(&dir)?, jobs)
        .into_iter()
        .chain((!catchup && dir.join(&login).exists()).then(|| CATCHUP.to_owned()))
        .collect();
    for name in &stale {
        remove(name, &dir)?;
    }
    systemctl(&["daemon-reload"])?;
    for (name, file, written) in changed {
        if written || !enabled(&file) {
            systemctl(&["enable", &file])?;
            // A timer recomputes its next tick; the login service runs now, as launchd's
            // RunAtLoad does when an agent is bootstrapped.
            systemctl(&["restart", &file])?;
        }
        println!("installed {name}");
    }
    Ok(())
}

/// Names of the jobs whose cones units are in `dir`. A unit under the cones name whose owner key
/// says otherwise belongs to someone else, so refuse it rather than remove it.
fn installed_jobs(dir: &Path) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("cones-"))
            .and_then(|s| {
                s.strip_suffix(".timer")
                    .or_else(|| s.strip_suffix(".service"))
            })
        else {
            continue;
        };
        let text = fs::read_to_string(&path)?;
        if !text.lines().any(|l| l == format!("{OWNER}={name}")) {
            bail!("refusing unexpected systemd unit {}", path.display());
        }
        names.insert(name.to_owned());
    }
    Ok(names.into_iter().collect())
}

fn stale_jobs(installed: Vec<String>, jobs: &[ResolvedJob]) -> Vec<String> {
    let keep: BTreeSet<&str> = jobs
        .iter()
        .filter(|j| j.enabled)
        .map(|j| j.name.as_str())
        .collect();
    installed
        .into_iter()
        .filter(|n| n != CATCHUP && !keep.contains(n.as_str()))
        .collect()
}

/// Disable and delete one job's units. A run in flight keeps going: stopping the timer does not
/// stop the service it already started.
fn remove(name: &str, dir: &Path) -> Result<()> {
    let mut removed = false;
    for kind in ["timer", "service"] {
        let file = format!("{}.{kind}", unit(name));
        let path = dir.join(&file);
        if !path.exists() {
            continue;
        }
        if kind == "timer" || name == CATCHUP {
            // Units systemd never loaded cannot be disabled; their files still go.
            let _ = systemctl(&["disable", "--now", &file]);
        }
        fs::remove_file(path)?;
        removed = true;
    }
    if removed {
        println!("uninstalled {name}");
    }
    Ok(())
}

pub fn uninstall_all() -> Result<()> {
    let dir = units_dir()?;
    for name in installed_jobs(&dir)? {
        remove(&name, &dir)?;
    }
    systemctl(&["daemon-reload"])
}

/// Start a job's service now, as a missed tick would have, with the environment its unit holds.
pub fn kickstart(job: &str) -> Result<()> {
    let file = format!("{}.service", unit(job));
    ensure!(
        units_dir()?.join(&file).exists(),
        "job {job} has no systemd timer installed; save it in the dashboard to install it"
    );
    systemctl(&["start", "--no-block", &file])
}

/// When a job's schedule was installed, from its timer's modification time.
pub fn installed_at(name: &str) -> Result<Option<DateTime<Local>>> {
    match fs::metadata(units_dir()?.join(format!("{}.timer", unit(name)))) {
        Ok(meta) => Ok(Some(meta.modified()?.into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    //! The calendar translation and the quoting run before systemd sees anything, so no E2E test
    //! on a machine without a user manager reaches them. Ways they fail:
    //! - launchd's Day-or-Weekday read as systemd's Day-and-Weekday, so a job fires on fewer days;
    //! - Sunday written as 7 or Sat-first, shifting every weekday job by one;
    //! - a path or prompt with `%`, `$`, `"` or `\` expanded or split by systemd;
    //! - a line break in a value ending the setting and injecting another.
    use super::*;

    #[test]
    fn cron_becomes_on_calendar_lines_with_day_or_weekday_kept_as_or() {
        assert_eq!(on_calendar("0 2 * * *").unwrap(), ["*-*-* 02:00:00"]);
        assert_eq!(on_calendar("* * * * *").unwrap(), ["*-*-* *:*:00"]);
        assert_eq!(
            on_calendar("30 9 * * 1-5").unwrap(),
            [
                "Fri *-*-* 09:30:00",
                "Mon *-*-* 09:30:00",
                "Thu *-*-* 09:30:00",
                "Tue *-*-* 09:30:00",
                "Wed *-*-* 09:30:00",
            ]
        );
        assert_eq!(
            on_calendar("0 9 1 * 0").unwrap(),
            ["*-*-01 09:00:00", "Sun *-*-* 09:00:00"],
            "cron fires on the 1st or on Sundays"
        );
        assert_eq!(on_calendar("5 4 * 3 7").unwrap(), ["Sun *-03-* 04:05:00"]);
    }

    #[test]
    fn values_reach_systemd_literally_and_line_breaks_are_refused() {
        assert_eq!(
            quoted(r#"a "b" $HOME 100% c:\d"#).unwrap(),
            r#""a \"b\" $HOME 100%% c:\\d""#,
            "Environment= does not expand $"
        );
        assert_eq!(
            exec(&["/tmp/a b/cones".into(), "$HOME".into()]).unwrap(),
            r#""/tmp/a b/cones" "$$HOME""#,
            "ExecStart= does"
        );
        assert!(quoted("a\nExecStart=/bin/false").is_err());
        assert!(literal("/tmp/x\ny").is_err());
    }
}
