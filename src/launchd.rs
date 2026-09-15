use crate::{config::ResolvedJob, harness, private_file};
use anyhow::{Context, Result, bail, ensure};
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
        for (_, bytes) in prepared {
            print!("{}", String::from_utf8(bytes)?);
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
        let path = agents.join(format!("{}.plist", label(&job.name)));
        let same = fs::read(&path).is_ok_and(|existing| existing == bytes);
        let is_loaded = loaded(&label(&job.name))?;
        if !same {
            if is_loaded {
                launchctl(&["bootout", &format!("{}/{}", domain(), label(&job.name))])?;
            }
            let tmp = agents.join(format!(".cones-{}.tmp", uuid::Uuid::new_v4()));
            let mut file = private_file(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(tmp, &path)?;
        }
        for suffix in ["out", "err"] {
            private_file(
                &state
                    .join("logs")
                    .join(format!("{}.{suffix}.log", job.name)),
            )?;
        }
        if !same || !is_loaded {
            launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
        }
        println!("installed {}", job.name);
    }
    // A disabled definition must not leave an older enabled LaunchAgent firing.
    for job in jobs.iter().filter(|j| !j.enabled) {
        uninstall_one(&job.name, &agents)?;
    }
    Ok(())
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
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("local.cones."))
            .and_then(|s| s.strip_suffix(".plist"))
        {
            let plist = Value::from_file(&path)?;
            let expected = label(name);
            if plist
                .as_dictionary()
                .and_then(|d| d.get("Label"))
                .and_then(Value::as_string)
                != Some(&expected)
            {
                bail!("refusing unexpected LaunchAgent {}", path.display());
            }
            uninstall_one(name, &dir)?;
        }
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
}
