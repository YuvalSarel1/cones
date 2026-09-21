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
#[derive(Subcommand)]
enum CoordinatorTask {
    /// Record this session as the folder's coordinator, or hand the folder back with --release.
    Claim {
        #[arg(long)]
        release: bool,
    },
    /// Block until a worker arrives or a worker writes; nothing else is worth a model call.
    Wait {
        /// Give up after this many seconds and exit 2, rather than waiting indefinitely.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Replies nobody has acted on, or --ack N once you have acted on them.
    Mail {
        #[arg(long)]
        ack: Option<usize>,
    },
    /// One note to a live worker, through its own harness's delivery command.
    Send {
        /// The session id as the roster prints it.
        id: String,
        text: String,
        /// The once-per-session introduction. Repeating it is a no-op, not a second message.
        #[arg(long)]
        greet: bool,
    },
    /// Head, tree, roster and pending mail in one read: everything to check before acting.
    Tick,
}

#[derive(Clone, Copy, ValueEnum)]
enum Trigger {
    Manual,
    Schedule,
}
#[derive(Subcommand)]
enum Action {
    #[command(name = "__terminal-host", hide = true)]
    TerminalHost,
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
    /// Start a harness session in a folder, the way the dashboard's composer does.
    Launch {
        /// The first instruction. Omitted, the session opens waiting for input.
        prompt: Option<String>,
        /// Folder to start in. Required: an agent started in the ambient directory edits
        /// whatever happens to be there.
        #[arg(long)]
        dir: PathBuf,
        /// Harness name; defaults to `defaults.harness`, else the first one configuration enables.
        #[arg(long)]
        harness: Option<String>,
        /// Native model id, as the dashboard's ctrl+o picks; defaults to the one in `defaults`.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort, for a harness that takes one; defaults to the one in `defaults`.
        #[arg(long)]
        effort: Option<String>,
        /// Print the shell command instead of starting anything.
        #[arg(long)]
        print_command: bool,
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
    StartCoordinator { dir: Option<PathBuf> },
    /// What a coordinator needs a program for: its claim on a folder, its wake gate, its mail
    /// and its notes. Coordination itself is the skill's judgment, not a command.
    Coordinator {
        /// The coordinated folder; defaults to the current directory. Worktrees under it belong
        /// to it, so a worker that moves into one stays on the same roster.
        #[arg(long, global = true)]
        dir: Option<PathBuf>,
        #[command(subcommand)]
        task: CoordinatorTask,
    },
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
    if matches!(cli.command, Some(Action::TerminalHost)) {
        return Ok(cones::terminal_host::serve()?);
    }
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
        // An install replaces the binary the agents name, which leaves launchd holding a code
        // requirement no new build can satisfy. Starting the dashboard is the moment after an
        // install that cones gets to run, so it is where the agents are made launchable again.
        if let Err(e) = launchd::relearn_signatures() {
            eprintln!("cones: schedules may not fire: {e:#}");
        }
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
        Action::TerminalHost => unreachable!(),
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
                let since = launchd::catchup_since(
                    last.with_timezone(&Local),
                    launchd::installed_at(&job.name)?,
                    now,
                );
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
        Action::Launch {
            prompt,
            dir,
            harness: named,
            model,
            effort,
            print_command,
        } => {
            let dir = cones::expand_path(&dir, &cwd)?
                .canonicalize()
                .context("launch directory")?;
            ensure!(dir.is_dir(), "{} is not a folder", dir.display());
            let mut policy = config::defaults(&jobs_path);
            let launchable = harness::launchable();
            let kind = match &named {
                Some(name) => {
                    let kind = harness::by_name(name)
                        .with_context(|| format!("unknown harness {name}"))?
                        .kind;
                    ensure!(launchable.contains(&kind), "{name} cannot start a session");
                    kind
                }
                None => policy
                    .harness
                    .filter(|k| policy.enabled_for(*k))
                    .or_else(|| launchable.iter().copied().find(|k| policy.enabled_for(*k)))
                    .context("configuration enables no harness")?,
            };
            ensure!(
                policy.enabled_for(kind),
                "{kind} is turned off in the configuration"
            );
            if let Some(model) = model {
                ensure!(!model.is_empty(), "--model cannot be empty");
                ensure!(policy.set_model_for(kind, model), "{kind} takes no --model");
            }
            if let Some(effort) = effort {
                ensure!(!effort.is_empty(), "--effort cannot be empty");
                ensure!(
                    policy.set_effort_for(kind, effort),
                    "{kind} takes no --effort"
                );
            }
            let prompt = prompt.unwrap_or_default();
            match harness::start(kind, &dir, prompt.trim(), &policy)? {
                // Claude's launch returns once the background session is recorded; its
                // identifier is the command's own output, as it is for the dashboard.
                harness::Start::Background(mut command) => {
                    if print_command {
                        println!("{}", shell_command(dir.as_os_str(), &command));
                        return Ok(0);
                    }
                    let status = command
                        .stdin(std::process::Stdio::null())
                        .status()
                        .with_context(|| format!("start {kind}"))?;
                    Ok(status.code().unwrap_or(1))
                }
                // Every other harness is its own terminal client, so it takes this terminal.
                harness::Start::Foreground(mut command) => {
                    command = harness::restore_stdin_prompt(command)?;
                    if print_command {
                        println!("{}", shell_command(dir.as_os_str(), &command));
                        return Ok(0);
                    }
                    attach_real_tty(&mut command);
                    let error = command.exec();
                    bail!("{kind} failed to start: {error}")
                }
            }
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
                        println!("{}", shell_command(s.cwd.as_os_str(), &command));
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
                println!("{}", shell_command(cwd.as_os_str(), &command));
                return Ok(0);
            }
            attach_real_tty(&mut command);
            let error = command.exec();
            bail!("native resume failed: {error}")
        }
        Action::StartCoordinator { dir } => {
            let dir = coordinated(dir, &cwd)?;
            if let Some(status) = harness::coordinator_status(&state, &dir) {
                println!(
                    "coordinator already running in {} (pid {}, session {})",
                    dir.display(),
                    status["pid"],
                    status["session"].as_str().unwrap_or("-")
                );
                return Ok(0);
            }
            let status = harness::coordinator(&dir, &state)?
                .status()
                .context("start claude")?;
            Ok(status.code().unwrap_or(1))
        }
        Action::Coordinator { dir, task } => {
            cones::cost::init(&state, false);
            let folder = cones::coordinator::Folder {
                state: state.clone(),
                claude: claude.clone(),
                jobs: jobs_path.clone(),
                path: coordinated(dir, &cwd)?,
            };
            match task {
                CoordinatorTask::Claim { release } => {
                    println!("{}", cones::coordinator::claim(&folder, release)?);
                }
                CoordinatorTask::Wait { timeout } => {
                    let limit = timeout.map(std::time::Duration::from_secs);
                    let Some(woken) = cones::coordinator::wait(&folder, limit)? else {
                        println!("timeout");
                        return Ok(2);
                    };
                    print!("{woken}");
                }
                CoordinatorTask::Mail { ack } => {
                    print!("{}", cones::coordinator::mail(&folder, ack)?);
                }
                CoordinatorTask::Send { id, text, greet } => {
                    print!("{}", cones::coordinator::send(&folder, &id, &text, greet)?);
                }
                CoordinatorTask::Tick => print!("{}", cones::coordinator::tick(&folder)?),
            }
            Ok(0)
        }
        Action::Worker { .. } => unreachable!(),
    }
}

/// The folder a coordinator command acts on. It must already exist: a coordinator claims a
/// folder agents are working in, never one it would create.
fn coordinated(dir: Option<PathBuf>, cwd: &std::path::Path) -> Result<PathBuf> {
    cones::expand_path(&dir.unwrap_or_else(|| PathBuf::from(".")), cwd)?
        .canonicalize()
        .context("coordinator directory")
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

/// The command as a line to paste, folder and set variables included. Variables the
/// command clears are left out: a pasted line inherits whatever the shell already has.
fn shell_command(cwd: &std::ffi::OsStr, command: &Command) -> String {
    let env: String = command
        .get_envs()
        .filter_map(|(k, v)| Some(format!("{}={} ", k.to_string_lossy(), quote(v?))))
        .collect();
    format!(
        "cd {} && {env}{} {}",
        quote(cwd),
        quote(command.get_program()),
        command.get_args().map(quote).collect::<Vec<_>>().join(" ")
    )
}

fn quote(s: &std::ffi::OsStr) -> String {
    format!("'{}'", s.to_string_lossy().replace('\'', "'\\''"))
}
