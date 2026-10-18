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
#[command(
    version,
    about = "A terminal workspace for coding agents",
    after_help = "Agents changing settings: run `cones config` for the file and its reference."
)]
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
enum ConfigTask {
    /// Set one setting by its dotted path, such as `columns`, `highlight` or `defaults.model`.
    /// The value is YAML, so a list is `'[state, bypass, model]'`. A value the file would refuse
    /// is refused and the file is left as it was.
    Set { key: String, value: String },
    /// Remove one setting, restoring its built-in.
    Unset { key: String },
}

/// Talking to the agents working in one folder, and noticing when one of them stops. The same
/// commands answer to `cones comms` and to the older `cones coordinator` spelling, against the
/// same folder state, because a session started before an upgrade still has the old skill loaded.
#[derive(Subcommand)]
enum CommsTask {
    /// Block until something worth a model call happens; nothing else wakes it.
    Wait {
        /// Watch only these workers, repeated once per id or unambiguous prefix. Then arrivals
        /// are somebody else's business and the wake reasons are a native input request, a
        /// native failure and a worker leaving the roster, each reported once. None of them is
        /// a finished task.
        #[arg(long = "id")]
        ids: Vec<String>,
        /// Give up after this many seconds, fractions allowed, and exit 2, rather than waiting
        /// indefinitely. This is your own recovery boundary; it does not limit or stop any worker.
        #[arg(long, value_parser = seconds)]
        timeout: Option<std::time::Duration>,
    },
    /// Replies nobody has acted on, or --ack N once you have acted on them.
    Mail {
        #[arg(long)]
        ack: Option<usize>,
    },
    /// One note to a live worker, through its own harness's delivery command.
    Send {
        /// The session id as the roster prints it, or an unambiguous prefix of one.
        id: String,
        text: String,
        /// The once-per-session introduction. Repeating it is a no-op, not a second message.
        #[arg(long)]
        greet: bool,
    },
}

#[derive(Subcommand)]
enum CoordinatorTask {
    /// Start the folder's coordinator: one background Claude session on the embedded skill.
    /// A folder another live coordinator holds is refused, not joined.
    Start,
    /// Record this session as the folder's coordinator, or hand the folder back with --release.
    Claim {
        #[arg(long)]
        release: bool,
    },
    /// Head, tree, roster and pending mail in one read: everything to check before acting.
    Tick,
    /// `send`, `mail` and `wait`, which `cones comms` also spells.
    #[command(flatten)]
    Comms(CommsTask),
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
    /// Start a harness session in a folder, the way the dashboard's composer does. It runs
    /// detached and needs no terminal; its id is printed once the harness reports it.
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
        /// The session's name, through the harness's own launch flag or its rename.
        #[arg(long)]
        name: Option<String>,
        /// Print the shell command instead of starting anything.
        #[arg(long)]
        print_command: bool,
        /// Keep the prompt out of cones' own launch record, for one that carries a secret.
        #[arg(long)]
        no_prompt_record: bool,
        /// After printing the id, block until the harness reports the session's turn ended, it
        /// asks for input, failed, stopped or left, and print `ID<TAB>STATE`. Run it as a
        /// background task, so its exit wakes you when the worker stops. That is not a
        /// finished task: read the worker's report.
        #[arg(long, requires = "prompt")]
        wait: bool,
        /// With --wait, give up after this many seconds, fractions allowed, and exit 2. It does
        /// not limit or stop the session.
        #[arg(long, value_parser = seconds, requires = "wait")]
        timeout: Option<std::time::Duration>,
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
        #[arg(long,value_parser=["started","ok","failed","timeout","skipped","crashed","input","active","shell","idle","blocked","done","stopped","exited"])]
        status: Option<String>,
        /// Only rows in this folder or under it, so a project's worktrees stay with the project.
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Reopen a conversation from history, detached, as the dashboard's Enter on a history row
    /// does. Prints its id; `cones send` gives it work.
    Resume {
        /// The conversation's id as `cones search` or `cones ls` prints it, or a prefix of one.
        id: String,
    },
    /// Fork a conversation through its harness's native fork, detached, as the dashboard's
    /// fork does. Prints the fork's id; `cones send` gives it work.
    Fork {
        /// The conversation's id as `cones search` or `cones ls` prints it, or a prefix of one.
        id: String,
    },
    /// End a session's work and keep its conversation; see docs/cli.md.
    Stop {
        /// The session id as `cones ls` prints it, or an unambiguous prefix of one.
        id: String,
    },
    /// Block until one of the sessions stops moving on its own, and print `ID<TAB>STATE`. Run
    /// it as a background task, so its exit wakes you; see docs/cli.md.
    Wait {
        /// Session ids as `cones ls` prints them, a job's session included.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Give up after this many seconds, fractions allowed, and exit 2. It does not limit or
        /// stop any session.
        #[arg(long, value_parser = seconds)]
        timeout: Option<std::time::Duration>,
    },
    /// One note to any live session, through its own harness's delivery; see docs/cli.md.
    Send {
        /// The session id as `cones ls` prints it, or an unambiguous prefix of one.
        id: String,
        text: String,
    },
    /// Name a session through its harness's own rename, as the dashboard's ctrl+n does; see
    /// docs/cli.md.
    Rename {
        /// The session id as `cones ls` prints it, or an unambiguous prefix of one.
        id: String,
        name: String,
    },
    /// Ask a conversation one question in a read-only fork its harness keeps nothing of, and
    /// print the answer. The conversation itself is not written to; see docs/cli.md.
    Btw {
        /// The conversation's id as `cones search` or `cones ls` prints it, or a prefix of one.
        id: String,
        question: String,
        /// The model the fork answers on, instead of the configured default.
        #[arg(long)]
        model: Option<String>,
        /// Give up after this many seconds, fractions allowed, end the fork and exit 2.
        #[arg(long, value_parser = seconds)]
        timeout: Option<std::time::Duration>,
    },
    /// Compact a live session's context through its harness's own compaction, and return once
    /// it is done; see docs/cli.md.
    Compact {
        /// The session id as `cones ls` prints it, or an unambiguous prefix of one.
        id: String,
    },
    /// What the dashboard's ctrl+x twice does to a row: stop it, remove it or close it; see
    /// docs/cli.md.
    Rm {
        /// A run or session id as `cones ls` prints it or an unambiguous prefix of one, a job
        /// name, or an open folder's path.
        target: String,
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
    /// Notes to the agents working in a folder, their replies, and the gate that waits for
    /// either. Who to write to and what it means is the caller's judgment, not a command.
    Comms {
        /// The folder the agents share; defaults to the current directory. Worktrees under it
        /// belong to it, so a worker that moves into one stays on the same roster.
        #[arg(long, global = true)]
        dir: Option<PathBuf>,
        #[command(subcommand)]
        task: CommsTask,
    },
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
    /// A bundled skill's prose on stdout, for an agent that is already running: nothing loads a
    /// skill into a live session, and every harness can read a command's output.
    Skill {
        /// Skill name; omitted, the bundled names are listed one per line.
        name: Option<String>,
    },
    /// How to change settings: the jobs.yaml path and its full field reference, for you or your
    /// agent to edit the file directly. --check validates the file instead.
    Config {
        #[arg(long)]
        check: bool,
        #[command(subcommand)]
        task: Option<ConfigTask>,
    },
    #[command(name = "__list", hide = true)]
    List,
    #[command(name = "__worker", hide = true)]
    Worker {
        #[arg(long)]
        run_id: String,
    },
    /// A session's conversation as text; see docs/cli.md. Reading starts nothing.
    Show {
        /// The session id as the roster prints it, or an unambiguous prefix of one.
        id: String,
        /// Export this many of the most recent messages instead of the default 40.
        #[arg(long, conflicts_with = "all")]
        tail: Option<usize>,
        /// The whole conversation the harness recorded, however long it is.
        #[arg(long)]
        all: bool,
        /// A search hit's `anchor.offset`: the messages around it instead of the tail.
        #[arg(long, conflicts_with_all = ["tail", "all"])]
        at: Option<u64>,
        /// Messages on each side of `--at`, default 5.
        #[arg(long, requires = "at")]
        context: Option<usize>,
        /// Machine-readable identity, messages and omission counts.
        #[arg(long)]
        json: bool,
        /// Restrict identity resolution to this harness.
        #[arg(long)]
        harness: Option<String>,
        /// Restrict identity resolution to this configured native home.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Search native conversation history without starting or resuming a session.
    Search {
        query: String,
        #[arg(long, value_enum, default_value = "words")]
        mode: cones::history_api::Mode,
        /// Only this folder and its descendants.
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        harness: Option<String>,
        /// Only this configured native home.
        #[arg(long)]
        home: Option<PathBuf>,
        /// Earliest recorded last activity, as an RFC 3339 timestamp.
        #[arg(long)]
        since: Option<chrono::DateTime<chrono::Utc>>,
        /// When you remember it happening. Hits in that window rank first; the rest follow.
        #[arg(long, value_enum, default_value = "any")]
        when: cones::history_api::When,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Wait this long for semantic indexing; incomplete results are labelled.
        #[arg(long, default_value_t = 30)]
        wait_seconds: u64,
        #[arg(long)]
        json: bool,
    },
    /// Build the meaning-search index for every conversation, with progress; see docs/cli.md.
    Index {
        /// Stop after this many seconds; finished batches are kept.
        #[arg(long)]
        timeout: Option<u64>,
        /// One JSON status line per change on stdout instead of a progress bar.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        task: Option<IndexTask>,
    },
    /// Serve history search and the control commands over MCP stdio until stdin closes.
    Mcp,
}

#[derive(Subcommand)]
enum IndexTask {
    /// Counts, model and cache, without loading a model or embedding anything.
    Status {
        #[arg(long)]
        json: bool,
    },
}

/// A positive, finite number of seconds, which may be fractional.
fn seconds(value: &str) -> Result<std::time::Duration, String> {
    match value.parse::<f64>() {
        Ok(s) if s.is_finite() && s > 0.0 => Ok(std::time::Duration::from_secs_f64(s)),
        _ => Err(format!("{value} is not a positive number of seconds")),
    }
}

fn index_progress(s: &cones::history_api::IndexStatus) -> String {
    use cones::history_api::Phase;
    let label = match s.phase {
        Phase::Syncing => return "Reading conversations…".into(),
        Phase::Waiting => "Another cones process is indexing",
        Phase::DownloadingModel => "Downloading model",
        Phase::Embedding | Phase::Failed => "Indexing",
        Phase::Done => "Indexed",
    };
    let width = 24;
    let filled = if s.passages > 0 {
        (s.embedded * width / s.passages) as usize
    } else {
        width as usize
    };
    let mut line = format!(
        "{label} ▕{}{}▏ {}/{}",
        "█".repeat(filled),
        "░".repeat(width as usize - filled),
        s.embedded,
        s.passages
    );
    if let Some(rate) = s.rate {
        line += &format!(" · {rate:.0}/s");
    }
    if let Some(eta) = s.eta_seconds.filter(|_| s.remaining > 0) {
        line += &format!(" · ~{}m{:02}s left", eta / 60, eta % 60);
    }
    line
}

fn index_summary(s: &cones::history_api::IndexStatus) -> String {
    format!(
        "{}/{} passages embedded · {} remaining · {} conversations · model {} · cache {} ({:.1} MB)",
        s.embedded,
        s.passages,
        s.remaining,
        s.conversations,
        if s.model_downloaded {
            "downloaded"
        } else {
            "not downloaded"
        },
        s.cache
            .as_ref()
            .map_or("none".into(), |p| p.display().to_string()),
        s.cache_bytes as f64 / 1e6
    )
}

/// `cones config` prints this ahead of docs/jobs.md, so the reference stays the one in docs/.
const CONFIG_GUIDE: &str = "\
# Changing cones settings

Every setting lives in one YAML file: __JOBS__

`cones config set KEY VALUE` sets one value by its dotted path and `cones config unset KEY`
restores its built-in; both refuse a value the file would not accept. VALUE is YAML:

    cones config set columns '[state, bypass, context, activity, model, age, last_active, folder, last_reply]'
    cones config set highlight cyan
    cones config set defaults.model sonnet

Jobs and anything else can be edited in the file directly:

- Keep the comments, layout and fields you were not asked to change.
- A missing file starts as `version: 4` and `jobs: []`.
- Setting a column list replaces the defaults, so copy the default list from the table
  below and add to it, as the columns example above does.
- Run `cones config --check` after editing by hand. An invalid file is not loaded and the dashboard
  falls back to built-in defaults without saying why.
- A running dashboard rereads the file on its next refresh. Scheduled jobs also need their
  LaunchAgents rewritten: run `cones __install` after adding, changing or removing a job.
- Links below are relative to https://github.com/YuvalSarel1/cones/tree/main/docs.

";

fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            // A dashboard can fail because its terminal closed; reporting that must not panic.
            let _ = writeln!(std::io::stderr(), "cones: {e:#}");
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
    cones::harness::spec::configure_homes(&config::homes(&jobs_path));
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
        let home = dirs::home_dir().context("missing home directory")?;
        if let Err(e) = cones::harness::install_skill(&state, &claude, &home) {
            eprintln!("cones: the cones skill was not installed: {e:#}");
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
                Status::Ok | Status::Skipped | Status::Input => 0,
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
            name,
            print_command,
            no_prompt_record,
            wait,
            timeout,
        } => {
            ensure!(
                !(wait && print_command),
                "--print-command starts nothing to wait for"
            );
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
            let name = name.map(|n| n.trim().to_owned());
            let spec = harness::spec(kind);
            let flag = spec.launch.as_ref().is_some_and(|l| l.name.is_some());
            if let Some(name) = &name {
                ensure!(!name.is_empty(), "--name cannot be empty");
                ensure!(
                    !name.chars().any(char::is_control),
                    "--name must be a single line without control characters"
                );
                ensure!(
                    flag || spec.operations.rename,
                    "{kind} keeps no session name cones can set"
                );
                ensure!(
                    flag || !print_command,
                    "{kind} has no launch flag for a name, so --print-command cannot carry one; \
                     run the command, then `cones rename`"
                );
            }
            let prompt = prompt.unwrap_or_default();
            // Codex's command names its daemon's address, which only a running daemon reports.
            ensure!(
                !print_command
                    || harness::spec(kind).launch.as_ref().map(|l| l.handler)
                        != Some(harness::spec::LaunchHandler::CodexRemote)
                    || cones::codex::daemon_pid(&cones::codex::home(&claude)).is_some(),
                "{kind}'s command names its daemon's address and no daemon is running;                  --print-command starts nothing, so run `{kind} app-server daemon start` first"
            );
            let start = harness::start(
                kind,
                &dir,
                prompt.trim(),
                name.as_deref().filter(|_| flag),
                &policy,
            )?;
            if print_command {
                let command = match start {
                    harness::Start::Background(c) => c,
                    harness::Start::Foreground(c) => harness::restore_stdin_prompt(c)?,
                };
                println!("{}", shell_command(dir.as_os_str(), &command));
                return Ok(0);
            }
            let since = chrono::Utc::now();
            // A detached launch outlives this shell and prints one id for the other
            // commands. Which kind of id that is depends on the harness; see docs/cli.md.
            let started = cones::launch::detached(
                &state,
                &claude,
                kind,
                &dir,
                prompt.trim(),
                !no_prompt_record,
                &policy,
                start,
            )?;
            eprintln!(
                "{} running detached, {}",
                started.harness,
                started.identity.describe()
            );
            println!("{}", started.id);
            if let Some(name) = name.filter(|_| !flag) {
                // The session is running whatever happens here, so its id is out first.
                let session = cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?
                    .into_iter()
                    .find(|s| s.session_id == started.id)
                    .with_context(|| {
                        format!("{} is not on the roster yet to be named", started.id)
                    })?;
                harness::rename(
                    &session,
                    &harness::home_of(&session, &claude),
                    &state,
                    &name,
                )
                .with_context(|| format!("{} started but was not named", started.id))?;
            }
            if wait {
                use std::io::Write;
                std::io::stdout().flush()?;
                match cones::launch::wait(
                    &state,
                    &claude,
                    &jobs_path,
                    std::slice::from_ref(&started.id),
                    Some(since),
                    timeout,
                )? {
                    Some(settled) => print!("{settled}"),
                    None => {
                        println!("timeout");
                        return Ok(2);
                    }
                }
            }
            Ok(0)
        }
        Action::Ls {
            job,
            status,
            dir,
            json,
        } => {
            cones::cost::init(&state, false);
            let ledger = Ledger::new(&state)?;
            if let Err(e) = cones::runner::reap_orphans(&ledger) {
                eprintln!("cones: {e:#}");
            }
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
            // The columns are named on stderr: a person or an agent reading the rows sees which
            // is which, and a script cutting fields from stdout gets rows only.
            if !json {
                eprintln!("id\tjob or folder\tstatus\tstarted\tharness\tcost\treason or tokens");
            }
            // A run the dashboard hid is out of its rows here too; the ledger keeps it.
            let hidden = ledger.hidden()?;
            // A run that started a session is that session in JSON, carrying its run.
            let live = if json {
                cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?
            } else {
                Vec::new()
            };
            for run in ledger.runs()?.into_iter().rev().filter(|r| {
                !hidden.contains(&r.started.run_id)
                    && job
                        .as_ref()
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
                    let projected = run.started.session_id.as_deref().map(|id| {
                        let session = live
                            .iter()
                            .find(|s| s.session_id == id || s.native_id.as_deref() == Some(id));
                        let state =
                            session.map_or_else(|| run.status().to_string(), |s| s.state.clone());
                        let session = session.map_or_else(
                            || {
                                serde_json::json!({
                                    "session_id": id, "harness": run.started.harness,
                                    "cwd": run.started.cwd, "state": "-",
                                })
                            },
                            |s| serde_json::json!(s),
                        );
                        serde_json::json!({
                            "kind": "session", "status": state, "session": session,
                            "run": {"run_id": run.started.run_id, "job": run.started.job,
                                    "status": run.status(), "started": run.started,
                                    "terminal": run.terminal},
                        })
                    });
                    println!(
                        "{}",
                        projected.unwrap_or_else(|| serde_json::json!({"kind":"run","status":run.status(),"started":run.started,"terminal":run.terminal}))
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
        Action::Resume { id } => {
            let entry = cones::show::entry(&claude, &id)?;
            let policy = config::defaults(&jobs_path);
            let rows = cones::tui::sessions(&claude, &state, &policy)?;
            let started = cones::launch::resume(&state, &policy, &entry, &rows)?;
            eprintln!("{} running detached", started.harness);
            println!("{}", started.id);
            Ok(0)
        }
        Action::Fork { id } => {
            let entry = cones::show::entry(&claude, &id)?;
            let policy = config::defaults(&jobs_path);
            let started = cones::launch::fork(&state, &claude, &policy, &entry)?;
            eprintln!(
                "{} fork running detached, {}",
                started.harness,
                started.identity.describe()
            );
            println!("{}", started.id);
            Ok(0)
        }
        Action::Stop { id } => {
            let rows = if id.len() >= 4 {
                cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?
            } else {
                Vec::new()
            };
            let ids = rows.into_iter().map(|s| s.session_id);
            let id = by_prefix(ids, &id)?;
            print!("{}", cones::stop::session(&state, &claude, &id)?);
            Ok(0)
        }
        Action::Wait { ids, timeout } => {
            match cones::launch::wait(&state, &claude, &jobs_path, &ids, None, timeout)? {
                Some(settled) => print!("{settled}"),
                None => {
                    println!("timeout");
                    return Ok(2);
                }
            }
            Ok(0)
        }
        Action::Send { id, text } => {
            let rows = cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?;
            let ids = rows.iter().map(|s| s.session_id.clone());
            let id = if id.len() >= 4 {
                by_prefix(ids, &id)?
            } else {
                id
            };
            print!(
                "{}",
                cones::coordinator::send_direct(&state, &claude, &rows, &id, &text)?
            );
            Ok(0)
        }
        Action::Rename { id, name } => {
            let rows = cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?;
            let ids = rows.iter().map(|s| s.session_id.clone());
            let id = if id.len() >= 4 {
                by_prefix(ids, &id)?
            } else {
                id
            };
            let session = rows
                .iter()
                .find(|s| s.session_id == id)
                .with_context(|| format!("no session {id}; `cones ls` lists them"))?;
            harness::rename(session, &harness::home_of(session, &claude), &state, &name)?;
            println!("renamed {id} ({}) to {}", session.harness, name.trim());
            Ok(0)
        }
        Action::Btw {
            id,
            question,
            model,
            timeout,
        } => {
            let entry = cones::show::entry(&claude, &id)?;
            let policy = config::defaults(&jobs_path);
            match harness::btw(&entry, &policy, &question, model.as_deref(), timeout)? {
                Some(answer) => println!("{answer}"),
                None => {
                    println!("timeout");
                    return Ok(2);
                }
            }
            Ok(0)
        }
        Action::Compact { id } => {
            let rows = cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?;
            let ids = rows.iter().map(|s| s.session_id.clone());
            let id = if id.len() >= 4 {
                by_prefix(ids, &id)?
            } else {
                id
            };
            let session = rows
                .iter()
                .find(|s| s.session_id == id)
                .with_context(|| format!("no session {id}; `cones ls` lists them"))?;
            harness::compact(session, &harness::home_of(session, &claude), &state)?;
            println!("compacted {id} ({})", session.harness);
            Ok(0)
        }
        Action::Rm { target } => {
            let ledger = Ledger::new(&state)?;
            let runs = ledger.runs()?;
            let jobs = config::read_jobs(&jobs_path).unwrap_or_default();
            if jobs.iter().any(|j| j.name == target) {
                // A running job's row stops its run, as in the dashboard; an idle one is deleted.
                if let Some(run) = runs
                    .iter()
                    .rev()
                    .find(|r| r.started.job.as_deref() == Some(&target) && r.status() == "started")
                {
                    let removed = cones::runner::remove(&ledger, &claude, &run.started.run_id)?;
                    print!("{}", removed.unwrap_or_default());
                    return Ok(0);
                }
                config::write_job(&jobs_path, Some(&target), None)?;
                launchd::install(
                    &config::read_jobs(&jobs_path)?,
                    &std::env::current_exe()?,
                    &fs::canonicalize(&jobs_path)?,
                    &state,
                    false,
                )
                .with_context(|| {
                    format!("job {target} deleted, but reinstalling the schedules failed")
                })?;
                println!("deleted job {target}; schedules reinstalled");
                return Ok(0);
            }
            // No id holds a slash, so a path names a folder.
            if target.contains('/') || target.starts_with('~') || target == "." || target == ".." {
                let dir = match target.strip_prefix('~') {
                    Some(rest) => {
                        PathBuf::from(std::env::var("HOME")?).join(rest.trim_start_matches('/'))
                    }
                    None => PathBuf::from(&target),
                };
                cones::tui::close_folder(&state, &dir)?;
                println!("closed {target}; the folder itself and any pin are untouched");
                return Ok(0);
            }
            let rows = cones::tui::sessions(&claude, &state, &config::defaults(&jobs_path))?;
            let ids = rows
                .iter()
                .map(|s| s.session_id.clone())
                .chain(runs.iter().map(|r| r.started.run_id.clone()));
            let id = if target.len() >= 4 {
                by_prefix(ids, &target)?
            } else {
                target
            };
            // A run is the jobs client's row; every other id is a session.
            match cones::runner::remove(&ledger, &claude, &id)? {
                Some(removed) => print!("{removed}"),
                None => print!("{}", cones::stop::remove(&state, &claude, &rows, &id)?),
            }
            Ok(0)
        }
        Action::Skill { name } => {
            let Some(name) = name else {
                for (bundled, _) in cones::harness::skills() {
                    println!("{bundled}");
                }
                return Ok(0);
            };
            let Some((_, text)) = cones::harness::skills().find(|(b, _)| *b == name) else {
                let bundled: Vec<_> = cones::harness::skills().map(|(b, _)| b).collect();
                bail!(
                    "no bundled skill {name}; there is {}",
                    bundled.join(" and ")
                );
            };
            print!("{text}");
            Ok(0)
        }
        Action::Config { check, task } => {
            if let Some(task) = task {
                let (key, value) = match &task {
                    ConfigTask::Set { key, value } => (key, Some(value.as_str())),
                    ConfigTask::Unset { key } => (key, None),
                };
                config::write_setting(&jobs_path, key, value)?;
            } else if check {
                config::read_jobs(&jobs_path)?;
                println!("{} is valid", jobs_path.display());
            } else {
                print!(
                    "{}",
                    CONFIG_GUIDE.replace("__JOBS__", &jobs_path.display().to_string())
                );
                print!("{}", include_str!("../docs/jobs.md"));
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
                        adapter.resume(&s.session_id, &s.cwd, &config::defaults(&jobs_path))?
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
        Action::Comms { dir, task } => {
            comms(&folder(&state, &claude, &jobs_path, dir, &cwd)?, task)
        }
        Action::Coordinator { dir, task } => {
            let folder = folder(&state, &claude, &jobs_path, dir, &cwd)?;
            match task {
                CoordinatorTask::Start => {
                    if let Some(status) = harness::coordinator_status(&state, &folder.path) {
                        bail!(
                            "coordinator already running in {} (pid {}, session {})",
                            folder.path.display(),
                            status["pid"],
                            status["session"].as_str().unwrap_or("-")
                        );
                    }
                    let started =
                        harness::coordinator(&folder.path, &state, &config::defaults(&jobs_path))?
                            .status()
                            .context("start claude")?;
                    Ok(started.code().unwrap_or(1))
                }
                CoordinatorTask::Claim { release } => {
                    println!("{}", cones::coordinator::claim(&folder, release)?);
                    Ok(0)
                }
                CoordinatorTask::Tick => {
                    print!("{}", cones::coordinator::tick(&folder)?);
                    Ok(0)
                }
                CoordinatorTask::Comms(task) => comms(&folder, task),
            }
        }
        Action::Show {
            id,
            tail,
            all,
            at,
            context,
            json,
            harness,
            home,
        } => {
            cones::history_api::validate_harness(harness.as_deref())?;
            let home = home.map(|p| cones::expand_path(&p, &cwd)).transpose()?;
            let located =
                match cones::show::locate_scoped(&claude, &id, harness.as_deref(), home.as_deref())
                {
                    Ok(located) => located,
                    // Not a conversation's own id: a live row is listed by another one.
                    Err(missing) => {
                        ensure!(tail != Some(0), "--tail takes a positive number of lines");
                        match cones::show::live(
                            &state,
                            &claude,
                            &id,
                            harness.as_deref(),
                            home.as_deref(),
                        )? {
                            Some(cones::show::Live::Conversation(located)) => located,
                            Some(cones::show::Live::Screen(screen)) => {
                                ensure!(at.is_none(), "{id} has a screen, not a conversation");
                                if json {
                                    println!("{}", screen.json(tail, all));
                                } else {
                                    print!("{}", screen.render(tail, all));
                                }
                                return Ok(0);
                            }
                            None => return Err(missing),
                        }
                    }
                };
            let export = match at {
                Some(at) => cones::transcript::export_around(
                    &located.source,
                    &located.key.harness,
                    at,
                    context.unwrap_or(cones::show::DEFAULT_CONTEXT),
                )?,
                None => {
                    let tail = (!all).then(|| tail.unwrap_or(cones::show::DEFAULT_TAIL));
                    cones::transcript::export(&located.source, &located.key.harness, tail)?
                }
            };
            if json {
                println!("{}", cones::show::json(&located, &export));
            } else {
                print!("{}", cones::show::render(&export));
            }
            // The export is text on stdout; what is wrong with it belongs on stderr.
            if export.incomplete {
                eprintln!(
                    "cones: {} is being written; its last record is not in this export",
                    located.key.session_id
                );
            }
            Ok(0)
        }
        Action::Search {
            query,
            mode,
            dir,
            harness,
            home,
            since,
            when,
            limit,
            offset,
            wait_seconds,
            json,
        } => {
            let mut service = cones::history_api::Service::discover(&claude, state, cwd);
            let results = service.search(cones::history_api::Search {
                query,
                mode,
                dir,
                harness,
                home,
                since,
                when,
                limit,
                offset,
                wait_seconds,
            })?;
            if json {
                println!("{}", serde_json::to_string(&results)?);
            } else {
                for entry in &results.entries {
                    println!(
                        "{}\t{}\t{}\t{}",
                        entry.key.session_id,
                        entry.key.harness,
                        entry.cwd.display(),
                        cones::transcript::plain(entry.title.as_deref().unwrap_or(""))
                    );
                    if let Some(hit) = &entry.hit
                        && !hit.snippet.is_empty()
                    {
                        println!("  {}", cones::transcript::plain(&hit.snippet));
                    }
                }
                eprintln!(
                    "{} matches; {} returned",
                    results.total,
                    results.entries.len()
                );
                if let Some(next) = results.next_offset {
                    eprintln!("next page: --offset {next}");
                }
                if results.pending {
                    eprintln!("cones: semantic search is incomplete; rerun to continue indexing");
                }
                if let Some(error) = &results.error {
                    eprintln!("cones: semantic search unavailable: {error}");
                }
            }
            Ok(if results.error.is_some() {
                1
            } else if results.pending {
                2
            } else {
                0
            })
        }
        Action::Index {
            timeout,
            json,
            task,
        } => {
            let json = json || matches!(task, Some(IndexTask::Status { json: true }));
            let mut service = cones::history_api::Service::discover(&claude, state, cwd);
            let status = match task {
                Some(IndexTask::Status { .. }) => {
                    let status = service.index_status()?;
                    if json {
                        println!("{}", serde_json::to_string(&status)?);
                    }
                    status
                }
                None => {
                    let bar = !json && std::io::stderr().is_terminal();
                    let status =
                        service.index(timeout.map(std::time::Duration::from_secs), |s| {
                            if json {
                                println!("{}", serde_json::to_string(s).unwrap());
                            } else if bar {
                                eprint!("\r\x1b[2K{}", index_progress(s));
                            } else {
                                eprintln!("{}", index_progress(s));
                            }
                        })?;
                    if bar {
                        eprintln!();
                    }
                    status
                }
            };
            if !json {
                println!("{}", index_summary(&status));
            }
            if let Some(error) = &status.error {
                eprintln!("cones: indexing failed: {error}");
            }
            Ok(match status.phase {
                cones::history_api::Phase::Done => 0,
                cones::history_api::Phase::Failed => 1,
                _ => 2,
            })
        }
        Action::Mcp => {
            // The control tools run this binary against the same configuration and state.
            let control = cones::history_mcp::Control {
                exe: std::env::current_exe().context("locating the cones executable")?,
                global: vec![
                    "--jobs".into(),
                    jobs_path.clone().into(),
                    "--state-dir".into(),
                    state.clone().into(),
                ],
            };
            let mut service = cones::history_api::Service::discover(&claude, state, cwd);
            cones::history_mcp::serve_with(
                &mut service,
                Some(&control),
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )?;
            Ok(0)
        }
        Action::Worker { .. } => unreachable!(),
    }
}

/// The folder a coordination command acts on. It must already exist: these commands act on a
/// folder agents are working in, never on one they would create.
fn folder(
    state: &std::path::Path,
    claude: &std::path::Path,
    jobs: &std::path::Path,
    dir: Option<PathBuf>,
    cwd: &std::path::Path,
) -> Result<cones::coordinator::Folder> {
    cones::cost::init(state, false);
    Ok(cones::coordinator::Folder {
        state: state.to_path_buf(),
        claude: claude.to_path_buf(),
        jobs: jobs.to_path_buf(),
        path: cones::expand_path(&dir.unwrap_or_else(|| PathBuf::from(".")), cwd)?
            .canonicalize()
            .context("coordinator directory")?,
    })
}

/// The commands `cones comms` and `cones coordinator` both answer to, on one folder's state.
fn comms(folder: &cones::coordinator::Folder, task: CommsTask) -> Result<i32> {
    match task {
        CommsTask::Wait { ids, timeout } => {
            match cones::coordinator::wait(folder, &ids, timeout) {
                Ok(Some(woken)) => print!("{woken}"),
                Ok(None) => {
                    println!("timeout");
                    return Ok(2);
                }
                // A watch that lost the lease to another watch gets its own code, because that
                // is the refusal a caller may retry: re-arming the moment a wait returns can
                // race the predecessor out of the folder. A peer's claim never clears that way.
                Err(e) if e.is::<cones::coordinator::Armed>() => {
                    eprintln!("cones: {e:#}");
                    return Ok(3);
                }
                Err(e) => return Err(e),
            }
        }
        CommsTask::Mail { ack } => print!("{}", cones::coordinator::mail(folder, ack)?),
        CommsTask::Send { id, text, greet } => {
            print!("{}", cones::coordinator::send(folder, &id, &text, greet)?)
        }
    }
    Ok(0)
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

/// An unambiguous prefix of an id `cones ls` prints names that row, as it does for `cones show`.
/// Anything else must match exactly. Rows sharing one id are the same name.
fn by_prefix(ids: impl Iterator<Item = String>, id: &str) -> Result<String> {
    let mut matches: Vec<_> = ids.filter(|s| s.starts_with(id)).collect();
    matches.sort();
    matches.dedup();
    ensure!(
        matches.len() < 2,
        "{id} matches {} sessions: {}",
        matches.len(),
        matches.join(", ")
    );
    Ok(matches.pop().unwrap_or_else(|| id.to_owned()))
}
