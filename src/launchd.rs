use crate::{
    config::{CatchUp, ResolvedJob},
    harness, private_file,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};
use plist::{Dictionary, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

type Interval = BTreeMap<String, u32>;

fn field(s: &str, min: u32, max: u32, sunday: bool) -> Result<Option<Vec<u32>>> {
    if s == "*" {
        return Ok(None);
    }
    let mut values = BTreeSet::new();
    for item in s.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, n)) => (r, n.parse::<u32>().context("invalid cron step")?),
            None => (item, 1),
        };
        ensure!(step > 0 && step <= max - min + 1, "cron step out of range");
        let (a, b) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse::<u32>().context("invalid cron range")?,
                b.parse::<u32>().context("invalid cron range")?,
            )
        } else {
            let a = range
                .parse::<u32>()
                .context("cron fields must be numeric")?;
            (a, if item.contains('/') { max } else { a })
        };
        ensure!(
            a >= min && b <= max && a <= b,
            "cron value outside {min}..{max}"
        );
        for n in (a..=b).step_by(step as usize) {
            values.insert(if sunday && n == 7 { 0 } else { n });
        }
    }
    ensure!(!values.is_empty(), "empty cron field");
    Ok(Some(values.into_iter().collect()))
}

pub fn calendar_intervals(schedule: &str) -> Result<Vec<Interval>> {
    let parts: Vec<_> = schedule.split_whitespace().collect();
    ensure!(
        parts.len() == 5,
        "schedule must be five-field local-time cron: minute hour day month weekday"
    );
    let fields = [
        field(parts[0], 0, 59, false)?,
        field(parts[1], 0, 23, false)?,
        field(parts[2], 1, 31, false)?,
        field(parts[3], 1, 12, false)?,
        field(parts[4], 0, 7, true)?,
    ];
    // launchd.plist(5) explicitly ORs Day and Weekday when both are specified.
    // Cron uses AND when either day field starts with a wildcard step. That intersection
    // cannot be represented faithfully by launchd's calendar dictionary.
    ensure!(
        !(fields[2].is_some()
            && fields[4].is_some()
            && (parts[2].starts_with('*') || parts[4].starts_with('*'))),
        "a wildcard day step combined with a restricted other day field cannot be represented by launchd; simplify the day/weekday constraint"
    );
    let mut all = BTreeSet::new();
    for variant in [fields] {
        let mut rows = vec![Interval::new()];
        for (key, values) in ["Minute", "Hour", "Day", "Month", "Weekday"]
            .into_iter()
            .zip(variant)
        {
            if let Some(values) = values {
                ensure!(
                    rows.len() * values.len() <= 4096,
                    "cron expands beyond 4096 launchd intervals; simplify the schedule"
                );
                rows = rows
                    .into_iter()
                    .flat_map(|row| {
                        values.iter().map(move |n| {
                            let mut row = row.clone();
                            row.insert(key.to_owned(), *n);
                            row
                        })
                    })
                    .collect();
            }
        }
        all.extend(rows);
    }
    ensure!(
        all.len() <= 4096,
        "cron expands beyond 4096 launchd intervals"
    );
    Ok(all.into_iter().collect())
}

pub fn label(job: &str) -> String {
    format!("local.cones.{job}")
}

/// The one agent that is not a job. No job may take this name.
pub const CATCHUP: &str = "catchup";

/// True when launchd would fire one of these intervals at `t`, per launchd.plist(5): an absent
/// key is a wildcard, and Day and Weekday are ORed when both are set. `calendar_intervals`
/// refuses the cron shapes where that OR cannot stand in for cron's AND, so agreeing with
/// launchd here is agreeing with the cron the owner wrote.
fn fires_at(intervals: &[Interval], t: &DateTime<Local>) -> bool {
    let weekday = t.weekday().num_days_from_sunday();
    intervals.iter().any(|i| {
        let at = |k: &str| i.get(k).copied();
        let days = match (at("Day"), at("Weekday")) {
            (Some(d), Some(w)) => d == t.day() || w == weekday,
            (d, w) => d.is_none_or(|d| d == t.day()) && w.is_none_or(|w| w == weekday),
        };
        at("Minute").is_none_or(|m| m == t.minute())
            && at("Hour").is_none_or(|h| h == t.hour())
            && at("Month").is_none_or(|m| m == t.month())
            && days
    })
}

/// The first tick after `since` that launchd should already have fired by `now`, in the local
/// wall-clock minutes cron is written in. A spring-forward has no 02:30 to find, and an hour
/// that repeats on fall-back returns its first pass; cron on the same Mac reads them the same way.
// ponytail: a minute walk, bounded by the caller's lookback. A next-tick solver would only pay
// off for a window measured in years, and a tick that old is not worth running.
pub fn first_missed(
    schedule: &str,
    since: DateTime<Local>,
    now: DateTime<Local>,
) -> Result<Option<DateTime<Local>>> {
    let intervals = calendar_intervals(schedule)?;
    let mut t = since
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .context("unrepresentable local time")?
        + Duration::minutes(1);
    while t <= now {
        if fires_at(&intervals, &t) {
            return Ok(Some(t));
        }
        t += Duration::minutes(1);
    }
    Ok(None)
}

/// Next configured tick, strictly after `after`. Enumerate matching calendar dates
/// before wall-clock times, so rare schedules do not require years of minute scans.
/// Nonexistent local times are skipped; either occurrence of a repeated time can match.
pub fn next_fire<Tz: TimeZone>(
    schedule: &str,
    after: DateTime<Tz>,
) -> Result<Option<DateTime<Tz>>> {
    let intervals = calendar_intervals(schedule)?;
    let zone = after.timezone();
    let mut date = after.date_naive();
    // Eight years include the next leap day even across a non-leap century.
    for _ in 0..366 * 8 {
        let mut next = None;
        for interval in &intervals {
            let at = |key: &str| interval.get(key).copied();
            let weekday = date.weekday().num_days_from_sunday();
            let days = match (at("Day"), at("Weekday")) {
                (Some(day), Some(week)) => day == date.day() || week == weekday,
                (day, week) => {
                    day.is_none_or(|d| d == date.day()) && week.is_none_or(|w| w == weekday)
                }
            };
            if !days || at("Month").is_some_and(|month| month != date.month()) {
                continue;
            }
            for hour in 0..24 {
                if at("Hour").is_some_and(|h| h != hour) {
                    continue;
                }
                for minute in 0..60 {
                    if at("Minute").is_some_and(|m| m != minute) {
                        continue;
                    }
                    let local = zone.from_local_datetime(
                        &date.and_hms_opt(hour, minute, 0).expect("valid time"),
                    );
                    for candidate in [local.clone().earliest(), local.latest()]
                        .into_iter()
                        .flatten()
                    {
                        if candidate > after && next.as_ref().is_none_or(|next| candidate < *next) {
                            next = Some(candidate);
                        }
                    }
                }
            }
        }
        if next.is_some() {
            return Ok(next);
        }
        let Some(tomorrow) = date.succ_opt() else {
            break;
        };
        date = tomorrow;
    }
    Ok(None)
}

pub fn render(
    job: &ResolvedJob,
    executable: &Path,
    jobs_file: &Path,
    state: &Path,
) -> Result<Vec<u8>> {
    ensure!(
        executable.is_absolute() && jobs_file.is_absolute() && state.is_absolute(),
        "launchd paths must be absolute"
    );
    let mut d = Dictionary::new();
    d.insert("Label".into(), label(&job.name).into());
    d.insert(
        "ProgramArguments".into(),
        Value::Array(
            [
                executable.to_string_lossy().into_owned(),
                "--jobs".into(),
                jobs_file.to_string_lossy().into_owned(),
                "--state-dir".into(),
                state.to_string_lossy().into_owned(),
                "run".into(),
                job.name.clone(),
                "--trigger".into(),
                "schedule".into(),
            ]
            .into_iter()
            .map(Value::String)
            .collect(),
        ),
    );
    d.insert(
        "WorkingDirectory".into(),
        job.cwd.to_string_lossy().to_string().into(),
    );
    d.insert(
        "StartCalendarInterval".into(),
        Value::Array(
            calendar_intervals(&job.schedule)?
                .into_iter()
                .map(|row| {
                    Value::Dictionary(
                        row.into_iter()
                            .map(|(k, v)| (k, Value::Integer(v.into())))
                            .collect(),
                    )
                })
                .collect(),
        ),
    );
    d.insert(
        "EnvironmentVariables".into(),
        Value::Dictionary(
            harness::environment(job)?
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        ),
    );
    d.insert("RunAtLoad".into(), Value::Boolean(false));
    d.insert("ProcessType".into(), "Background".into());
    // launchd cleans its own process group. The separate worker group is supervised
    // independently and also stops when its parent disappears.
    d.insert("AbandonProcessGroup".into(), Value::Boolean(false));
    for (key, suffix) in [("StandardOutPath", "out"), ("StandardErrorPath", "err")] {
        d.insert(
            key.into(),
            state
                .join("logs")
                .join(format!("{}.{suffix}.log", job.name))
                .to_string_lossy()
                .to_string()
                .into(),
        );
    }
    let mut out = Vec::new();
    Value::Dictionary(d).to_writer_xml(&mut out)?;
    Ok(out)
}

/// The catch-up agent carries no schedule and no job environment: it only decides which jobs
/// slept through a tick and asks launchd to start those, so each run still comes from the job's
/// own agent with the environment `install` captured for it. `RunAtLoad` is what makes it a
/// login agent, and it runs nothing when nothing was missed.
pub fn render_catchup(exe: &Path, jobs_path: &Path, state: &Path) -> Result<Vec<u8>> {
    ensure!(
        exe.is_absolute() && jobs_path.is_absolute() && state.is_absolute(),
        "launchd paths must be absolute"
    );
    let mut d = Dictionary::new();
    d.insert("Label".into(), label(CATCHUP).into());
    d.insert(
        "ProgramArguments".into(),
        Value::Array(
            [
                exe.to_string_lossy().into_owned(),
                "--jobs".into(),
                jobs_path.to_string_lossy().into_owned(),
                "--state-dir".into(),
                state.to_string_lossy().into_owned(),
                CATCHUP.into(),
            ]
            .into_iter()
            .map(Value::String)
            .collect(),
        ),
    );
    d.insert(
        "WorkingDirectory".into(),
        state.to_string_lossy().to_string().into(),
    );
    d.insert("RunAtLoad".into(), Value::Boolean(true));
    d.insert("ProcessType".into(), "Background".into());
    d.insert("AbandonProcessGroup".into(), Value::Boolean(false));
    for (key, suffix) in [("StandardOutPath", "out"), ("StandardErrorPath", "err")] {
        d.insert(
            key.into(),
            state
                .join("logs")
                .join(format!("{CATCHUP}.{suffix}.log"))
                .to_string_lossy()
                .to_string()
                .into(),
        );
    }
    let mut out = Vec::new();
    Value::Dictionary(d).to_writer_xml(&mut out)?;
    Ok(out)
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}
fn loaded(label: &str) -> Result<bool> {
    Ok(Command::new("/bin/launchctl")
        .args(["print", &format!("{}/{}", domain(), label)])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success())
}
fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("/bin/launchctl").args(args).output()?;
    ensure!(
        output.status.success(),
        "launchctl {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Writes and loads one LaunchAgent per job.
///
/// `exe` becomes `ProgramArguments[0]` in every plist and callers pass
/// `std::env::current_exe()`, so the agents name whichever copy of the binary ran the install.
/// Installing from a stray copy therefore schedules that copy: delete it later and the job cannot
/// launch, silently, until the next install. Run this from the binary on `PATH`.
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
            harness::adapter(job.harness)?.compile(job, &uuid::Uuid::new_v4().to_string())?;
            Ok((job, render(job, exe, jobs_path, state)?))
        })
        .collect::<Result<_>>()?;
    if dry_run {
        if prepared.iter().any(|(job, _)| !job.env.is_empty()) {
            eprintln!(
                "cones: warning: dry-run XML includes values of named environment variables and may contain secrets"
            );
        }
        for (_, bytes) in &prepared {
            print!("{}", String::from_utf8(bytes.clone())?);
        }
        if prepared
            .iter()
            .any(|(job, _)| job.catch_up != CatchUp::Skip)
        {
            print!(
                "{}",
                String::from_utf8(render_catchup(exe, jobs_path, state)?)?
            );
        }
        return Ok(());
    }
    ensure!(
        cfg!(target_os = "macos"),
        "install requires macOS launchd; --dry-run works on Unix"
    );
    crate::private_dir(state)?;
    crate::private_dir(&state.join("logs"))?;
    let agents = dirs::home_dir()
        .context("missing home directory")?
        .join("Library/LaunchAgents");
    fs::create_dir_all(&agents)?;
    for (job, bytes) in prepared {
        install_agent(&agents, state, &job.name, &bytes)?;
        println!("installed {}", job.name);
    }
    // A definition that is disabled, renamed or deleted outright must not leave a LaunchAgent
    // firing for a job the file no longer has. Installed agents are the truth, not the file's
    // disabled entries: a deleted job leaves no entry to read.
    for name in stale_agents(installed_agents(&agents)?, jobs) {
        uninstall_one(&name, &agents)?;
    }
    // The same rule for the login agent: no job asks to catch up, no agent waits at login.
    if jobs
        .iter()
        .any(|j| j.enabled && j.catch_up != CatchUp::Skip)
    {
        install_agent(
            &agents,
            state,
            CATCHUP,
            &render_catchup(exe, jobs_path, state)?,
        )?;
        println!("installed {CATCHUP}");
    } else {
        uninstall_one(CATCHUP, &agents)?;
    }
    Ok(())
}

/// Write one agent and load it, replacing a stale copy. Idempotent: an unchanged plist that is
/// already loaded is left alone, so `install` never restarts an agent it did not change.
fn install_agent(agents: &Path, state: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = agents.join(format!("{}.plist", label(name)));
    let same = fs::read(&path).is_ok_and(|existing| existing == bytes);
    let is_loaded = loaded(&label(name))?;
    if !same {
        if is_loaded {
            launchctl(&["bootout", &format!("{}/{}", domain(), label(name))])?;
        }
        let tmp = agents.join(format!(".cones-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(tmp, &path)?;
    }
    for suffix in ["out", "err"] {
        private_file(&state.join("logs").join(format!("{name}.{suffix}.log")))?;
    }
    if !same || !is_loaded {
        launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    }
    Ok(())
}

/// Ask launchd to run a job's agent now, as a missed tick would have. The run comes from the
/// job's own plist, so its environment is the one `install` captured rather than this process's.
pub fn kickstart(job: &str) -> Result<()> {
    ensure!(
        loaded(&label(job))?,
        "job {job} has no LaunchAgent loaded; save it in the dashboard to install it"
    );
    launchctl(&["kickstart", &format!("{}/{}", domain(), label(job))])
}

/// Names of the cones LaunchAgents in `dir`. A file under the cones name whose Label says
/// otherwise belongs to someone else, so refuse it rather than remove it.
fn installed_agents(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if !dir.exists() {
        return Ok(names);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("local.cones."))
            .and_then(|s| s.strip_suffix(".plist"))
        else {
            continue;
        };
        let expected = label(name);
        if Value::from_file(&path)?
            .as_dictionary()
            .and_then(|d| d.get("Label"))
            .and_then(Value::as_string)
            != Some(&expected)
        {
            bail!("refusing unexpected LaunchAgent {}", path.display());
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// Installed agents no enabled job claims. The login agent is not a job and is decided by
/// whether any job asks to catch up, so leave it out of this.
fn stale_agents(installed: Vec<String>, jobs: &[ResolvedJob]) -> Vec<String> {
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

fn uninstall_one(name: &str, dir: &Path) -> Result<()> {
    let path = dir.join(format!("{}.plist", label(name)));
    if loaded(&label(name))? {
        launchctl(&["bootout", &format!("{}/{}", domain(), label(name))])?;
    }
    match fs::remove_file(path) {
        Ok(()) => println!("uninstalled {name}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn uninstall_all() -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "uninstall requires macOS launchd"
    );
    let dir = dirs::home_dir()
        .context("missing home directory")?
        .join("Library/LaunchAgents");
    for name in installed_agents(&dir)? {
        uninstall_one(&name, &dir)?;
    }
    Ok(())
}

pub fn exported_plist_path(name: &str) -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(name))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_schedules_one_interval_per_tick_and_never_runs_at_load() {
        let mut job = crate::config::adhoc(None, "hi", Path::new("/tmp")).unwrap();
        job.schedule = "0 2,14 * * *".into();
        let bytes = render(
            &job,
            Path::new("/usr/local/bin/cones"),
            Path::new("/tmp/jobs.yaml"),
            Path::new("/tmp/state"),
        )
        .unwrap();
        let value = Value::from_reader_xml(bytes.as_slice()).unwrap();
        let d = value.as_dictionary().unwrap();
        assert_eq!(d["RunAtLoad"].as_boolean(), Some(false));
        assert!(!d.contains_key("StartInterval"));
        let ticks = d["StartCalendarInterval"].as_array().unwrap();
        assert_eq!(ticks.len(), 2);
        let hours: Vec<_> = ticks
            .iter()
            .map(|t| t.as_dictionary().unwrap()["Hour"].as_signed_integer())
            .collect();
        assert_eq!(hours, [Some(2), Some(14)]);
        assert_eq!(
            ticks[0].as_dictionary().unwrap()["Minute"].as_signed_integer(),
            Some(0)
        );
    }

    #[test]
    fn install_removes_the_agent_of_a_job_the_file_no_longer_has() {
        let d = tempfile::tempdir().unwrap();
        let agent = |name: &str| {
            let mut plist = Dictionary::new();
            plist.insert("Label".into(), Value::String(label(name)));
            Value::Dictionary(plist)
                .to_file_xml(d.path().join(format!("{}.plist", label(name))))
                .unwrap();
        };
        for name in ["update-workbench", "readme-check", CATCHUP] {
            agent(name);
        }
        fs::write(d.path().join("com.example.other.plist"), "not ours").unwrap();
        let mut installed = installed_agents(d.path()).unwrap();
        installed.sort();
        assert_eq!(
            installed,
            ["catchup", "readme-check", "update-workbench"],
            "a foreign LaunchAgent is not one of ours"
        );
        let job = |name: &str, enabled: bool| {
            let mut job = crate::config::adhoc(None, "hi", Path::new("/tmp")).unwrap();
            job.name = name.to_owned();
            job.enabled = enabled;
            job
        };
        let jobs = [job("update-workbench", true), job("nightly", false)];
        assert_eq!(
            stale_agents(installed, &jobs),
            ["readme-check"],
            "the deleted job's agent goes; the enabled job keeps its own and the login agent is \
             decided elsewhere"
        );
        assert_eq!(
            stale_agents(vec!["nightly".to_owned()], &jobs),
            ["nightly"],
            "a disabled job still loses its agent"
        );
    }

    #[test]
    fn an_agent_under_the_cones_name_with_a_foreign_label_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut plist = Dictionary::new();
        plist.insert("Label".into(), Value::String("com.someone.else".into()));
        Value::Dictionary(plist)
            .to_file_xml(d.path().join("local.cones.impostor.plist"))
            .unwrap();
        let error = installed_agents(d.path()).unwrap_err().to_string();
        assert!(
            error.contains("refusing unexpected LaunchAgent")
                && error.contains("local.cones.impostor.plist"),
            "{error}"
        );
    }

    #[test]
    fn the_catchup_agent_runs_at_load_and_carries_no_schedule() {
        let bytes = render_catchup(
            Path::new("/usr/local/bin/cones"),
            Path::new("/tmp/jobs.yaml"),
            Path::new("/tmp/state"),
        )
        .unwrap();
        let value = Value::from_reader_xml(bytes.as_slice()).unwrap();
        let d = value.as_dictionary().unwrap();
        assert_eq!(d["Label"].as_string(), Some("local.cones.catchup"));
        assert_eq!(d["RunAtLoad"].as_boolean(), Some(true));
        assert!(!d.contains_key("StartCalendarInterval"));
        assert!(!d.contains_key("StartInterval"));
        // No job environment: each run comes from the job's own agent, not from this one.
        assert!(!d.contains_key("EnvironmentVariables"));
        let args: Vec<_> = d["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_string().unwrap())
            .collect();
        assert_eq!(args.last(), Some(&"catchup"));
    }

    /// Local midnight on a fixed date, plus hours and minutes: cron is written in wall-clock
    /// time, so the walk has to agree with the zone the Mac is in, whichever that is.
    fn at(date: (i32, u32, u32), h: u32, m: u32) -> DateTime<Local> {
        chrono::NaiveDate::from_ymd_opt(date.0, date.1, date.2)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .expect("test dates avoid DST folds")
    }

    #[test]
    fn next_fire_uses_calendar_constraints_and_searches_across_leap_years() {
        let next = |schedule: &str, after: &str| {
            next_fire(schedule, DateTime::parse_from_rfc3339(after).unwrap())
                .unwrap()
                .map(|at| at.to_rfc3339())
        };
        assert_eq!(
            next("* * * * *", "2026-09-17T09:00:59+03:00").as_deref(),
            Some("2026-09-17T09:01:00+03:00")
        );
        assert_eq!(
            next("0 2 * * *", "2026-09-17T02:00:00+03:00").as_deref(),
            Some("2026-09-18T02:00:00+03:00")
        );
        assert_eq!(
            next("0 9 1 * 1", "2026-09-17T12:00:00+03:00").as_deref(),
            Some("2026-09-21T09:00:00+03:00"),
            "day and weekday are ORed"
        );
        assert_eq!(
            next("0 0 29 2 *", "2096-03-01T00:00:00+00:00").as_deref(),
            Some("2104-02-29T00:00:00+00:00")
        );
        assert_eq!(next("0 0 31 2 *", "2026-09-17T12:00:00+03:00"), None);
    }

    #[test]
    fn next_fire_respects_local_clock_gaps_and_repeated_minutes() {
        let mut date = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        // Use the machine's timezone, including zones with no daylight saving.
        for _ in 0..365 {
            for hour in 0..24 {
                let naive = date.and_hms_opt(hour, 0, 0).unwrap();
                let schedule = format!("0 {hour} * * *");
                match Local.from_local_datetime(&naive) {
                    chrono::LocalResult::None => {
                        if let Some(before) = Local
                            .from_local_datetime(&(naive - Duration::hours(2)))
                            .earliest()
                        {
                            let found = next_fire(&schedule, before).unwrap().unwrap();
                            assert!(
                                found.date_naive() > date,
                                "a skipped hour cannot fire: {found}"
                            );
                            assert_eq!(found.hour(), hour);
                        }
                    }
                    chrono::LocalResult::Ambiguous(first, second) => {
                        let (first, second) = if first < second {
                            (first, second)
                        } else {
                            (second, first)
                        };
                        assert_eq!(
                            next_fire(&schedule, first).unwrap(),
                            Some(second),
                            "the later occurrence is still in the future"
                        );
                    }
                    _ => {}
                }
            }
            date = date.succ_opt().unwrap();
        }
    }

    #[test]
    fn a_missed_tick_is_the_first_one_past_the_last_run() {
        const D: (i32, u32, u32) = (2026, 9, 16);
        let at = |h, m| at(D, h, m);
        // A daily 02:00 job last run at 03:00 yesterday has missed today's tick by 05:00.
        assert_eq!(
            first_missed("0 2 * * *", at(3, 0) - Duration::days(1), at(5, 0)).unwrap(),
            Some(at(2, 0))
        );
        // Nothing between 02:00 and 05:00 the same morning.
        assert_eq!(first_missed("0 2 * * *", at(2, 0), at(5, 0)).unwrap(), None);
        // The tick the job just ran is behind `since`, not missed.
        assert_eq!(
            first_missed("0 2 * * *", at(2, 0), at(2, 30)).unwrap(),
            None
        );
        // Every minute: the very next minute counts.
        assert_eq!(
            first_missed("* * * * *", at(9, 0), at(9, 30)).unwrap(),
            Some(at(9, 1))
        );
        // A future window has nothing to catch up.
        assert_eq!(first_missed("0 2 * * *", at(5, 0), at(5, 0)).unwrap(), None);
    }

    #[test]
    fn weekday_and_month_constraints_hold_across_a_long_window() {
        const MON: (i32, u32, u32) = (2026, 9, 14);
        assert_eq!(at(MON, 0, 0).weekday(), chrono::Weekday::Mon);
        let day = |days: u64, h| at(MON, h, 0) + Duration::days(days as i64);
        // Weekday 1 is Monday: from Tuesday, the first tick missed is the following Monday.
        let found = first_missed("0 3 * * 1", day(1, 0), day(9, 0))
            .unwrap()
            .unwrap();
        assert_eq!(found.weekday(), chrono::Weekday::Mon);
        assert_eq!((found.hour(), found.minute()), (3, 0));
        assert_eq!(found, day(7, 3));
        // A January job is not missed in September, however long the window.
        assert_eq!(
            first_missed("0 3 1 1 *", day(0, 0), day(9, 0)).unwrap(),
            None
        );
    }
}
