use anyhow::{Context, Result, bail, ensure};
use chrono::Local;
use clap::{Parser, Subcommand, ValueEnum};
use cones::{
    config, harness, launchd,
    ledger::{Ledger, Status},
    output, runner,
};
use std::{
    fs,
    io::{IsTerminal, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(version, about = "A terminal workspace for coding agents")]
struct Cli {
    #[arg(long, global = true, default_value = "~/.cones/jobs.yaml")]
    jobs: PathBuf,
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Log dashboard transitions, errors and timing summaries to STATE_DIR/tui-debug.log.
    #[arg(long, global = true)]
    debug: bool,
    /// Include input text and every timing sample in dashboard diagnostics.
    #[arg(long, global = true)]
    trace: bool,
    /// The dashboard, with no subcommand at all.
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Clone, Copy, ValueEnum)]
enum Trigger {
    Manual,
    Schedule,
}
#[derive(Subcommand)]
enum Action {
    /// Run a job now, or `--prompt` for a one-off task in the current directory.
    Run {
        /// Job name; with --prompt, the job whose policy the task borrows (default: the first).
        job: Option<String>,
        /// Run this prompt once as an ad-hoc job instead of a job from the file.
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, value_enum, default_value = "manual")]
        trigger: Trigger,
    },
    /// Start jobs whose ticks passed while the Mac was off or logged out. The login agent runs this.
    Catchup {
        /// Name the ticks that were missed without starting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write and load the LaunchAgents for the jobs; the dashboard runs this when a job is saved.
    #[command(name = "__install", hide = true)]
    Install {
        #[arg(long)]
        dry_run: bool,
    },
    /// Runs and live sessions as rows; the JSON format is documented in docs/cli.md.
    Ls {
        #[arg(long)]
        job: Option<String>,
        #[arg(long,value_parser=["started","ok","failed","timeout","skipped","crashed","active","idle","blocked","done","stopped","exited"])]
        status: Option<String>,
        /// Only rows in this folder or under it, so a project's worktrees stay with the project.
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// A run's captured output; the dashboard opens this in a viewer.
    #[command(name = "__logs", hide = true)]
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        raw: bool,
    },
    /// A background session or a finished run in this terminal; the dashboard opens this.
    #[command(name = "__attach", hide = true)]
    Attach {
        id: String,
        #[arg(long)]
        print_command: bool,
    },
    /// The folder's coordinator: the embedded start-orchestrator skill in one background session.
    #[command(name = "__coordinator", hide = true)]
    Coordinator { dir: Option<PathBuf> },
    #[command(name = "__list", hide = true)]
    List,
    #[command(name = "__worker", hide = true)]
    Worker {
        #[arg(long)]
        run_id: String,
    },
}

fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cones: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
fn execute(cli: Cli) -> Result<i32> {
    if let Some(Action::Worker { run_id }) = &cli.command {
        return runner::worker(run_id);
    }
    let cwd = std::env::current_dir()?;
    let state = cones::expand_path(
        &cli.state_dir.unwrap_or_else(|| PathBuf::from("~/.cones")),
        &cwd,
    )?;
    let jobs_path = cones::expand_path(&cli.jobs, &cwd)?;
    let claude = cones::fleet::claude_dir()?;
    // No subcommand is the dashboard: `cones` is the dashboard, and the rest is machinery.
    let Some(command) = cli.command else {
        // Now that a bare `cones` is the dashboard, a pipe or a cron line reaches it by accident.
        ensure!(
            std::io::stdout().is_terminal(),
            "the dashboard is what plain `cones` does, and it needs a terminal"
        );
        return cones::tui::run(
            &std::env::current_exe()?,
            &jobs_path,
            &state,
            &claude,
            cli.debug,
            cli.trace,
        );
    };
    match command {
        Action::Install { dry_run } => {
            launchd::install(
                &config::read_jobs(&jobs_path)?,
                &std::env::current_exe()?,
                &fs::canonicalize(jobs_path)?,
                &state,
                dry_run,
            )?;
            Ok(0)
        }
        Action::Catchup { dry_run } => {
            let jobs = config::read_jobs(&jobs_path)?;
            let runs = Ledger::new(&state)?.runs()?;
            let now = Local::now();
            let mut failed = false;
            for job in jobs
                .iter()
                .filter(|j| j.enabled && j.catch_up == config::CatchUp::Once)
            {
                // The mark is the last tick launchd actually delivered, skip or run alike. A job
                // with no scheduled run behind it has missed nothing: there is no window yet.
                let Some(last) = runs
                    .iter()
                    .rev()
                    .find(|r| {
                        r.started.job.as_deref() == Some(job.name.as_str())
                            && r.started.trigger.as_deref() == Some("schedule")
                    })
                    .and_then(|r| r.started.fired_at)
                else {
                    continue;
                };
                // ponytail: a month of lookback, so a Mac off since spring starts one run and not
                // a season of them. Make it a policy field if a job ever needs a different memory.
                let since = last
                    .with_timezone(&Local)
                    .max(now - chrono::Duration::days(31));
                let Some(missed) = launchd::first_missed(&job.schedule, since, now)? else {
                    continue;
                };
                println!("{}\tmissed\t{}", job.name, missed.format("%Y-%m-%d %H:%M"));
                // One run per job however many ticks passed: `catch_up: once`. Admission still
                // decides, so a catch-up onto a running job is skipped like any other tick.
                if !dry_run && let Err(e) = launchd::kickstart(&job.name) {
                    // One job without an agent must not keep the others from catching up.
                    eprintln!("cones: {}: {e:#}", job.name);
                    failed = true;
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        Action::Run {
            job,
            prompt,
            trigger,
        } => {
            let jobs = config::read_jobs(&jobs_path).unwrap_or_default();
            let named = match &job {
                Some(name) => Some(
                    jobs.iter()
                        .find(|j| &j.name == name)
                        .context("unknown job name")?,
                ),
                None => None,
            };
            let adhoc;
            let job = match prompt {
                Some(p) => {
                    adhoc = config::adhoc(named.or(jobs.first()), &p, &cwd)?;
                    &adhoc
                }
                None => named.context("a job name or --prompt is required")?,
            };
            let ledger = Ledger::new(&state)?;
            let status = runner::run(
                job,
                &ledger,
                &std::env::current_exe()?,
                match trigger {
                    Trigger::Manual => "manual",
                    Trigger::Schedule => "schedule",
                },
            )?;
            Ok(match status {
                Status::Ok | Status::Skipped => 0,
                Status::Timeout => 124,
                _ => 1,
            })
        }
        Action::Ls {
            job,
            status,
            dir,
            json,
        } => {
            cones::cost::init(&state, false);
            let ledger = Ledger::new(&state)?;
            // ponytail: canonicalize both sides, so /var and /private/var compare equal on macOS
            // and a worktree reached through a symlink still belongs to its folder. A path that
            // cannot be resolved stays as given.
            let real = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            let scope = dir.as_deref().map(real);
            // No folder at all cannot be placed, so a scoped read leaves it out.
            let inside = |p: Option<&std::path::Path>| match (&scope, p) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(scope), Some(p)) => {
                    let p = real(p);
                    p == *scope || p.starts_with(scope)
                }
            };
            for run in ledger.runs()?.into_iter().rev().filter(|r| {
                job.as_ref()
                    .is_none_or(|j| r.started.job.as_ref() == Some(j))
                    && status.as_ref().is_none_or(|s| r.status() == *s)
                    && inside(
                        r.started
                            .cwd
                            .as_deref()
                            .or_else(|| r.terminal.as_ref()?.cwd.as_deref()),
                    )
            }) {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"kind":"run","status":run.status(),"started":run.started,"terminal":run.terminal})
                    );
                } else {
                    let last = run.terminal.as_ref().unwrap_or(&run.started);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        run.started.run_id,
                        run.started.job.as_deref().unwrap_or("-"),
                        run.status(),
                        run.started
                            .fired_at
                            .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                            .unwrap_or_else(|| "-".into()),
                        run.started
                            .harness
                            .map(|h| h.to_string())
                            .unwrap_or_else(|| "-".into()),
                        last.cost_usd
                            .map(cones::fleet::cost)
                            .unwrap_or_else(|| "-".into()),
                        last.reason.as_deref().unwrap_or("-")
                    );
                }
            }
            // Append unowned fleet sessions using the run table's columns.
            for s in cones::tui::fleet_rows(
                &claude,
                &state,
                &ledger.runs()?,
                &config::defaults(&jobs_path),
            )?
            .into_iter()
            .filter(|s| {
                job.is_none()
                    && status.as_ref().is_none_or(|st| s.state == *st)
                    && inside(Some(&s.cwd))
            }) {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"kind":"session","status":s.state,"session":s})
                    );
                } else {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        s.session_id,
                        cones::fleet::tilde(&s.cwd),
                        s.state,
                        s.started
                            .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                            .unwrap_or_else(|| "-".into()),
                        s.harness,
                        cones::cost::display(s.cost_usd, s.cost_info.as_ref()),
                        cones::fleet::tokens(&s)
                    );
                }
            }
            Ok(0)
        }
        Action::List => {
            cones::cost::init(&state, false);
            print!("{}", cones::tui::list(&jobs_path, &state, &claude)?);
            Ok(0)
        }
        Action::Logs { id, follow, raw } => {
            let ledger = Ledger::new(&state)?;
            if ledger.resolve(&id).is_err()
                && let Some(s) = cones::fleet::find(&claude, &id)?
            {
                let t = s
                    .transcript_path
                    .as_deref()
                    .context("session has no transcript")?;
                cones::fleet::follow(t, follow)?;
                return Ok(0);
            }
            output::logs(&ledger, &id, follow, raw)?;
            Ok(0)
        }
        Action::Attach { id, print_command } => {
            let ledger = Ledger::new(&state)?;
            let run = match ledger.resolve(&id) {
                Ok(run) => run,
                Err(e) => {
                    let s = cones::fleet::find(&claude, &id)?.ok_or(e)?;
                    ensure!(
                        s.harness == "claude",
                        "{} sessions are listed but cannot be attached; open them in their own terminal",
                        s.harness
                    );
                    let kind = serde_json::from_value(serde_json::Value::String(s.harness.clone()))
                        .context("unknown harness in fleet state")?;
                    let adapter = harness::adapter(kind)?;
                    let mut command = if s.pid.is_some_and(cones::fleet::alive) {
                        ensure!(
                            !s.own_terminal(),
                            "this claude runs interactively in its own terminal; `claude attach` takes background sessions only"
                        );
                        adapter.attach(&s.session_id, &s.cwd)?
                    } else {
                        adapter.resume(&s.session_id, &s.cwd)?
                    };
                    if print_command {
                        println!(
                            "cd {} && {} {}",
                            quote(s.cwd.as_os_str()),
                            quote(command.get_program()),
                            command.get_args().map(quote).collect::<Vec<_>>().join(" ")
                        );
                        return Ok(0);
                    }
                    attach_real_tty(&mut command);
                    let error = command.exec();
                    bail!("native resume failed: {error}")
                }
            };
            let adapter = harness::adapter(run.started.harness.context("run has no harness")?)?;
            let mut command = runner::resume_finished(&run, adapter.as_ref())?;
            let session = run
                .started
                .session_id
                .as_deref()
                .context("run has no session ID")?;
            let cwd = run
                .started
                .cwd
                .as_deref()
                .context("run has no working directory")?;
            let native = adapter.transcript(session, cwd)?;
            if !native.is_file() {
                let archive = run
                    .terminal
                    .as_ref()
                    .and_then(|t| t.transcript.as_ref())
                    .context("native transcript is missing and no archive was recorded")?;
                ensure!(
                    archive.is_file(),
                    "archived transcript is missing: {}",
                    archive.display()
                );
                if !print_command {
                    cones::private_dir(native.parent().context("invalid transcript path")?)?;
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&native)?;
                    file.write_all(&fs::read(archive)?)?;
                    file.sync_all()?;
                }
            }
            if print_command {
                println!(
                    "cd {} && {} {}",
                    quote(cwd.as_os_str()),
                    quote(command.get_program()),
                    command.get_args().map(quote).collect::<Vec<_>>().join(" ")
                );
                return Ok(0);
            }
            attach_real_tty(&mut command);
            let error = command.exec();
            bail!("native resume failed: {error}")
        }
        Action::Coordinator { dir } => {
            let dir = cones::expand_path(&dir.unwrap_or_else(|| PathBuf::from(".")), &cwd)?
                .canonicalize()
                .context("coordinator directory")?;
            if let Some(status) = harness::coordinator_status(&dir) {
                println!(
                    "coordinator already running in {} (pid {}, session {})",
                    dir.display(),
                    status["pid"],
                    status["jobId"].as_str().unwrap_or("-")
                );
                return Ok(0);
            }
            let status = harness::coordinator(&dir, &state)?
                .status()
                .context("start claude")?;
            Ok(status.code().unwrap_or(1))
        }
        Action::Worker { .. } => unreachable!(),
    }
}

/// Launchers such as fzf hand children the `/dev/tty` clone device. Bun-based harnesses
/// cannot kqueue that device (EINVAL), so point stdio at the real terminal node instead.
fn attach_real_tty(command: &mut Command) {
    let Ok(out) = Command::new("ps")
        .args(["-o", "tty=", "-p", &std::process::id().to_string()])
        .output()
    else {
        return;
    };
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !name.starts_with("tty") {
        return;
    }
    let open = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/{name}"))
    };
    if let (Ok(i), Ok(o)) = (open(), open()) {
        command.stdin(i).stdout(o);
    }
    // A piped stderr belongs to a caller that wants the error text, such as the dashboard.
    if std::io::IsTerminal::is_terminal(&std::io::stderr())
        && let Ok(e) = open()
    {
        command.stderr(e);
    }
}

fn quote(s: &std::ffi::OsStr) -> String {
    format!("'{}'", s.to_string_lossy().replace('\'', "'\\''"))
}
