use crate::{expand_path, launchd};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    Codex,
    Pi,
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Overlap {
    #[default]
    Skip,
    Allow,
    Replace,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub timeout_min: Option<f64>,
    pub budget_usd: Option<f64>,
    pub daily_budget_usd: Option<f64>,
    pub write: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub codex_full_access: Option<bool>,
    pub overlap: Option<Overlap>,
    pub notify: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub harness: HarnessKind,
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub archive_transcript: bool,
    #[serde(default)]
    pub env: Vec<String>,
    // Keep these explicit: serde flatten cannot enforce unknown-field rejection reliably.
    pub timeout_min: Option<f64>,
    pub budget_usd: Option<f64>,
    pub daily_budget_usd: Option<f64>,
    pub write: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub codex_full_access: Option<bool>,
    pub overlap: Option<Overlap>,
    pub notify: Option<bool>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsFile {
    pub version: u32,
    #[serde(default)]
    pub defaults: Policy,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedJob {
    pub name: String,
    pub schedule: String,
    pub harness: HarnessKind,
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub enabled: bool,
    pub archive_transcript: bool,
    pub env: Vec<String>,
    pub timeout_min: f64,
    pub budget_usd: f64,
    pub daily_budget_usd: Option<f64>,
    pub write: bool,
    pub tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub codex_full_access: bool,
    pub overlap: Overlap,
    pub notify: bool,
}

pub fn read_jobs(path: &Path) -> Result<Vec<ResolvedJob>> {
    let path =
        fs::canonicalize(path).with_context(|| format!("read jobs file {}", path.display()))?;
    let doc: JobsFile =
        serde_yaml::from_str(&fs::read_to_string(&path)?).context("invalid jobs.yaml")?;
    ensure!(
        doc.version == 1,
        "unsupported jobs version {}; expected 1",
        doc.version
    );
    let mut names = BTreeSet::new();
    doc.jobs
        .into_iter()
        .map(|j| {
            ensure!(
                !j.name.is_empty()
                    && j.name.len() <= 80
                    && j.name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "job names must be 1..80 ASCII letters, digits, underscores or hyphens"
            );
            ensure!(
                names.insert(j.name.clone()),
                "duplicate job name: {}",
                j.name
            );
            resolve(j, &doc.defaults, path.parent().unwrap())
        })
        .collect()
}

fn resolve(j: Job, d: &Policy, base: &Path) -> Result<ResolvedJob> {
    let tools = j.tools.or_else(|| d.tools.clone());
    if j.harness == HarnessKind::Codex && tools.is_some() {
        bail!(
            "job {}: Codex has no per-tool allowlist; remove tools and choose write: false (read-only) or write: true (workspace-write)",
            j.name
        );
    }
    let full = j.codex_full_access.or(d.codex_full_access).unwrap_or(false);
    ensure!(
        !full || j.harness == HarnessKind::Codex,
        "job {}: codex_full_access applies only to Codex",
        j.name
    );
    let max_turns = j.max_turns.or(d.max_turns);
    ensure!(
        max_turns != Some(0),
        "job {}: max_turns must be positive",
        j.name
    );
    ensure!(
        max_turns.is_none() || j.harness == HarnessKind::Claude,
        "job {}: max_turns is supported only by Claude",
        j.name
    );
    let timeout = j.timeout_min.or(d.timeout_min).unwrap_or(30.0);
    let budget = j.budget_usd.or(d.budget_usd).unwrap_or(2.0);
    let daily = j.daily_budget_usd.or(d.daily_budget_usd);
    ensure!(
        timeout.is_finite() && timeout > 0.0 && timeout <= 10080.0,
        "job {}: timeout_min must be positive and at most 10080",
        j.name
    );
    ensure!(
        budget.is_finite() && budget > 0.0,
        "job {}: budget_usd must be positive and finite",
        j.name
    );
    ensure!(
        daily.is_none_or(|x| x.is_finite() && x > 0.0),
        "job {}: daily_budget_usd must be positive and finite",
        j.name
    );
    ensure!(
        daily.is_none_or(|x| x >= budget),
        "job {}: daily_budget_usd must cover at least one budget_usd reservation",
        j.name
    );
    launchd::calendar_intervals(&j.schedule).with_context(|| format!("job {} schedule", j.name))?;
    ensure!(
        !j.prompt.trim().is_empty() && !j.prompt.contains('\0'),
        "job {}: prompt must be nonempty and contain no NUL",
        j.name
    );
    ensure!(
        j.model
            .as_ref()
            .is_none_or(|s| !s.is_empty() && !s.contains('\0')),
        "job {}: model must be nonempty and contain no NUL",
        j.name
    );
    let cwd = expand_path(&j.cwd, base)?;
    ensure!(
        cwd.is_dir(),
        "job {}: cwd is not a directory: {}",
        j.name,
        cwd.display()
    );
    for key in &j.env {
        ensure!(
            !key.is_empty()
                && key.bytes().enumerate().all(|(i, b)| b == b'_'
                    || b.is_ascii_alphabetic()
                    || (i > 0 && b.is_ascii_digit())),
            "job {}: invalid environment variable name: {key}",
            j.name
        );
        ensure!(
            !matches!(
                key.as_str(),
                "HOME"
                    | "PATH"
                    | "SHELL"
                    | "BASH_ENV"
                    | "ENV"
                    | "NODE_OPTIONS"
                    | "CLAUDE_CONFIG_DIR"
            ) && !key.starts_with("DYLD_")
                && !key.starts_with("LD_")
                && !key.starts_with("CLAUDE_CODE_"),
            "job {}: {key} can override execution policy and cannot be imported",
            j.name
        );
    }
    let write = j.write.or(d.write).unwrap_or(false);
    let overlap = j.overlap.or(d.overlap).unwrap_or_default();
    ensure!(
        !(write && overlap == Overlap::Allow),
        "job {}: overlap: allow with write: true requires worktree-per-run, which is not implemented; use skip or replace",
        j.name
    );
    let tools = tools.unwrap_or_else(|| match j.harness {
        HarnessKind::Claude => vec!["Read".into(), "Grep".into(), "Glob".into()],
        HarnessKind::Pi => vec!["read".into(), "grep".into(), "find".into(), "ls".into()],
        HarnessKind::Codex => vec![],
    });
    Ok(ResolvedJob {
        name: j.name,
        schedule: j.schedule,
        harness: j.harness,
        cwd: fs::canonicalize(cwd)?,
        prompt: j.prompt,
        model: j.model,
        enabled: j.enabled,
        archive_transcript: j.archive_transcript,
        env: j.env,
        timeout_min: timeout,
        budget_usd: budget,
        daily_budget_usd: daily,
        write,
        tools,
        max_turns,
        codex_full_access: full,
        overlap,
        notify: j.notify.or(d.notify).unwrap_or(false),
    })
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    AgentConsole,
    #[default]
    None,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub control_plane: Backend,
    pub agent_console_url: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            control_plane: Backend::None,
            agent_console_url: "http://127.0.0.1:7878".into(),
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s).context("invalid config.toml")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}
