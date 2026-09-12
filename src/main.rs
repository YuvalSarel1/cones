use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use cones::{
    config::{self, Backend, Config},
    control::{self, ControlPlane, Headless},
    harness, launchd,
    ledger::{Ledger, Status},
    output, runner,
};
use std::{
    fs,
    io::Write,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(
    version,
    about = "Scheduled coding-agent runs with explicit policy and a durable ledger"
)]
struct Cli {
    #[arg(long, global = true, default_value = "jobs.yaml")]
    jobs: PathBuf,
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}
#[derive(Clone, Copy, ValueEnum)]
enum Trigger {
    Manual,
    Schedule,
}
#[derive(Subcommand)]
enum Action {
    /// Validate all jobs and their compiled execution policy.
    Validate,
    /// Install enabled jobs as launchd LaunchAgents.
    Install {
        /// Print plists without installing anything. Contains values of named env variables.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove cones LaunchAgents, retaining run history and transcripts.
    Uninstall,
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
    /// List runs as tab-separated rows.
    Ls {
        #[arg(long)]
        job: Option<String>,
        #[arg(long,value_parser=["started","ok","failed","timeout","skipped","crashed","active","idle","blocked","exited"])]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show captured output. Ctrl+C detaches a follower without stopping the run.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        raw: bool,
    },
    /// Stop a running job after verifying its supervisor identity.
    Stop { id: String },
    /// Resume a completed run in its harness UI.
    Attach {
        id: String,
        /// Show the native resume command without opening a TUI.
        #[arg(long)]
        print_command: bool,
    },
    /// Hold the writer lock on a directory while running a command: `cones lock . -- git commit`.
    Lock {
        dir: PathBuf,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Check execution prerequisites and policy hazards.
    Doctor,
    /// Dashboard: jobs, live sessions and runs, with a details pane and a dispatch prompt.
    Tui,
    /// Record a Claude Code hook event from stdin as a fleet state file.
    Hook {
        /// Write the fleet hook into ~/.claude/settings.json instead of recording an event.
        #[arg(long)]
        install: bool,
        /// PID of the harness process; the installed command passes $PPID.
        pid: Option<u32>,
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
    if let Action::Worker { run_id } = &cli.command {
        return runner::worker(run_id);
    }
    let cwd = std::env::current_dir()?;
    let state = cones::expand_path(
        &cli.state_dir.unwrap_or_else(|| PathBuf::from("~/.cones")),
        &cwd,
    )?;
    let jobs_path = cones::expand_path(&cli.jobs, &cwd)?;
    match cli.command {
        Action::Validate => {
            let jobs = config::read_jobs(&jobs_path)?;
            for job in &jobs {
                harness::adapter(job.harness)?.compile(job, &uuid::Uuid::new_v4().to_string())?;
                println!("{}\tvalid\t{}", job.name, job.harness);
            }
            Ok(0)
        }
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
        Action::Uninstall => {
            launchd::uninstall_all()?;
            Ok(0)
        }
        Action::Run {
            job,
            prompt,
            trigger,
        } => {
            let config = Config::load(&state.join("config.toml"))?;
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
                &config,
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
        Action::Ls { job, status, json } => {
            let ledger = Ledger::new(&state)?;
            for run in ledger.runs()?.into_iter().rev().filter(|r| {
                job.as_ref()
                    .is_none_or(|j| r.started.job.as_ref() == Some(j))
                    && status.as_ref().is_none_or(|s| r.status() == *s)
            }) {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"status":run.status(),"started":run.started,"terminal":run.terminal})
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
                            .map(|t| t.to_rfc3339())
                            .unwrap_or_else(|| "-".into()),
                        run.started
                            .harness
                            .map(|h| h.to_string())
                            .unwrap_or_else(|| "-".into()),
                        last.cost_usd
                            .map(|c| format!("{c:.4}"))
                            .unwrap_or_else(|| "-".into()),
                        last.reason.as_deref().unwrap_or("-")
                    );
                }
            }
            // Sessions the fleet hook saw that no cones run owns; same columns, cwd where the job name goes.
            for s in cones::tui::fleet_rows(&state, &ledger.runs()?)?
                .into_iter()
                .filter(|s| job.is_none() && status.as_ref().is_none_or(|st| s.state == *st))
            {
                if json {
                    println!("{}", serde_json::json!({"status":s.state,"session":s}));
                } else {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        s.session_id,
                        cones::fleet::tilde(&s.cwd),
                        s.state,
                        s.updated.to_rfc3339(),
                        s.harness,
                        s.cost_usd
                            .map(|c| format!("{c:.4}"))
                            .unwrap_or_else(|| "-".into()),
                        cones::fleet::tokens(&s)
                    );
                }
            }
            Ok(0)
        }
        Action::Lock { dir, command } => {
            let _held =
                Ledger::new(&state)?.workspace_lock_wait(&cones::expand_path(&dir, &cwd)?)?;
            let status = Command::new(&command[0]).args(&command[1..]).status()?;
            Ok(status.code().unwrap_or(1))
        }
        Action::Tui => cones::tui::run(&std::env::current_exe()?, &jobs_path, &state),
        Action::Hook { install, pid } => {
            if install {
                let settings = claude_settings()?;
                cones::fleet::install(
                    &settings,
                    &cones::fleet::hook_command(&std::env::current_exe()?, &state),
                )?;
                println!("fleet hook installed in {}", settings.display());
                return Ok(0);
            }
            let payload = serde_json::from_reader(std::io::stdin().lock())
                .context("hook payload is not JSON")?;
            let pid = pid.unwrap_or_else(std::os::unix::process::parent_id);
            cones::fleet::record(&state, pid, &payload)?;
            Ok(0)
        }
        Action::List => {
            print!("{}", cones::tui::list(&jobs_path, &state)?);
            Ok(0)
        }
        Action::Logs { id, follow, raw } => {
            output::logs(&Ledger::new(&state)?, &id, follow, raw)?;
            Ok(0)
        }
        Action::Stop { id } => {
            let stopped = runner::stop(&Ledger::new(&state)?, &id)?;
            println!(
                "{}\t{}",
                id,
                if stopped {
                    "stop requested"
                } else {
                    "already finished"
                }
            );
            Ok(0)
        }
        Action::Attach { id, print_command } => {
            let ledger = Ledger::new(&state)?;
            let run = match ledger.resolve(&id) {
                Ok(run) => run,
                // Not a cones run: a session the fleet hook saw. Resume it in place once it is idle.
                Err(e) => {
                    let s = cones::fleet::find(&state, &id)?.ok_or(e)?;
                    ensure!(
                        s.state != "active",
                        "session is still active in its own terminal"
                    );
                    let kind = serde_json::from_value(serde_json::Value::String(s.harness.clone()))
                        .context("unknown harness in fleet state")?;
                    let mut command = harness::adapter(kind)?.resume(&s.session_id, &s.cwd)?;
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
            let mut command = if run.started.live == Some(true) {
                let config = Config::load(&state.join("config.toml"))?;
                control::backend(&config)?.attach(&run, adapter.as_ref())?
            } else {
                Headless.attach(&run, adapter.as_ref())?
            };
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
        Action::Doctor => doctor(
            &jobs_path,
            &state,
            &Config::load(&state.join("config.toml"))?,
        ),
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
    if let (Ok(i), Ok(o), Ok(e)) = (open(), open(), open()) {
        command.stdin(i).stdout(o).stderr(e);
    }
}

fn claude_settings() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("missing home directory")?
        .join(".claude/settings.json"))
}
fn quote(s: &std::ffi::OsStr) -> String {
    format!("'{}'", s.to_string_lossy().replace('\'', "'\\''"))
}
fn doctor(jobs_path: &std::path::Path, state: &std::path::Path, config: &Config) -> Result<i32> {
    let mut failed = false;
    let mut report = |level: &str, message: String| {
        if level == "FAIL" {
            failed = true;
        }
        println!("{level}\t{message}");
    };
    report(
        if cfg!(target_os = "macos") {
            "OK"
        } else {
            "FAIL"
        },
        "launchd requires a logged-in macOS user; wake coalescing does not wake a sleeping Mac"
            .into(),
    );
    let expected = harness::launch_path();
    let shell_path = std::env::var("PATH").unwrap_or_default();
    for part in std::env::split_paths(&expected).take(3) {
        report(
            if std::env::split_paths(&shell_path).any(|p| p == part) {
                "OK"
            } else {
                "WARN"
            },
            format!(
                "shell PATH entry {} (generated launchd PATH includes it)",
                part.display()
            ),
        );
    }
    let jobs = match config::read_jobs(jobs_path) {
        Ok(jobs) => jobs,
        Err(e) => {
            report("FAIL", format!("jobs: {e:#}"));
            vec![]
        }
    };
    for job in &jobs {
        let compiled = harness::adapter(job.harness)
            .and_then(|a| a.compile(job, &uuid::Uuid::new_v4().to_string()));
        report(
            if compiled.is_ok() { "OK" } else { "FAIL" },
            match compiled {
                Ok(_) => format!("job {} policy compiles", job.name),
                Err(e) => format!("job {}: {e:#}", job.name),
            },
        );
        if job.codex_full_access {
            report(
                "WARN",
                format!("job {} enables Codex full access", job.name),
            );
        }
        if job.tools.iter().any(|t| t == "Bash" || t == "Bash(*)") {
            report(
                "WARN",
                format!(
                    "job {} permits broad Bash{}; native sandbox and ordinary permission checks remain enabled",
                    job.name,
                    if job.archive_transcript {
                        " and archives plaintext transcripts"
                    } else {
                        ""
                    }
                ),
            );
        }
        if !job.write
            && job
                .tools
                .iter()
                .any(|t| matches!(t.split('(').next().unwrap_or(""), "Edit" | "Write" | "Bash"))
        {
            report(
                "WARN",
                format!(
                    "job {}: write: false removes Edit, Write and Bash from the compiled allowlist",
                    job.name
                ),
            );
        }
        let path = launchd::exported_plist_path(&job.name)?;
        if path.exists() {
            match plist::Value::from_file(&path)
                .ok()
                .and_then(|p| p.as_dictionary().cloned())
                .and_then(|d| d.get("EnvironmentVariables").cloned())
                .and_then(|e| {
                    e.as_dictionary()
                        .and_then(|d| d.get("PATH"))
                        .and_then(plist::Value::as_string)
                        .map(str::to_owned)
                }) {
                Some(path)
                    if std::env::split_paths(&expected)
                        .all(|p| std::env::split_paths(&path).any(|q| p == q)) =>
                {
                    report("OK", format!("job {} installed launchd PATH", job.name))
                }
                _ => report(
                    "FAIL",
                    format!(
                        "job {} installed plist has missing PATH entries; reinstall",
                        job.name
                    ),
                ),
            }
        } else {
            report("WARN", format!("job {} is not installed", job.name));
        }
    }
    if let Some(claude) = harness::executable("claude", &expected) {
        let version = Command::new(&claude).arg("--version").output()?;
        report(
            if version.status.success() {
                "OK"
            } else {
                "FAIL"
            },
            String::from_utf8_lossy(&version.stdout).trim().into(),
        );
        let help = Command::new(&claude).arg("--help").output()?;
        let help = String::from_utf8_lossy(&help.stdout);
        for flag in [
            "--permission-prompts",
            "--permission-mode",
            "--max-budget-usd",
            "--session-id",
            "--tools",
            "--allowedTools",
            "--safe-mode",
            "--restricted",
            "--strict-mcp-config",
            "--setting-sources",
        ] {
            report(
                if help.contains(flag) { "OK" } else { "FAIL" },
                format!("Claude capability {flag}"),
            );
        }
        // Invalid values exercise the parser without starting an API call; --version bypasses parsing.
        let probe = Command::new(&claude)
            .args(["-p", "--max-turns", "cones-invalid"])
            .stdin(std::process::Stdio::null())
            .output()?;
        let error = String::from_utf8_lossy(&probe.stderr);
        report(
            if error.contains("must be a number") {
                "OK"
            } else {
                "WARN"
            },
            "Claude hidden --max-turns parser probe".into(),
        );
        let auth = Command::new(&claude)
            .args(["auth", "status", "--json"])
            .stdin(std::process::Stdio::null())
            .output()?;
        let logged_in = serde_json::from_slice::<serde_json::Value>(&auth.stdout)
            .ok()
            .and_then(|v| v["loggedIn"].as_bool())
            .unwrap_or(false);
        report(if logged_in{"OK"}else{"WARN"},"Claude authentication status (no credentials printed; named job env still needs to match the auth provider)".into());
    } else {
        report("FAIL", "claude not found in generated launchd PATH".into());
    }
    match config.control_plane {
        Backend::None => report(
            "OK",
            "control_plane = none; headless execution selected".into(),
        ),
        Backend::AgentConsole => match control::backend(config)?.status() {
            Ok(s) => report(
                "WARN",
                format!("{s}; inspected API cannot enforce job policy, runs use headless fallback"),
            ),
            Err(e) => report("WARN", format!("{e}; runs use headless fallback")),
        },
    }
    let settings = claude_settings()?;
    report(
        if cones::fleet::installed(&settings) {
            "OK"
        } else {
            "WARN"
        },
        format!(
            "fleet hook in {} (cones hook --install)",
            settings.display()
        ),
    );
    let projects = dirs::home_dir()
        .context("missing home directory")?
        .join(".claude/projects");
    report(
        if projects.is_dir() { "OK" } else { "WARN" },
        format!("Claude session store {}", projects.display()),
    );
    match Ledger::new(state).and_then(|ledger| ledger.runs()) {
        Ok(_) => report(
            "OK",
            format!(
                "run ledger {} is readable and writable",
                state.join("runs.jsonl").display()
            ),
        ),
        Err(e) => report("FAIL", format!("run ledger: {e:#}")),
    }
    Ok(if failed { 1 } else { 0 })
}
