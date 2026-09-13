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
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
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
    /// Session columns the dashboard shows after the harness and title, from `COLUMNS`.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

pub const COLUMNS: [&str; 7] = [
    "state", "model", "age", "activity", "context", "tokens", "last",
];
pub const DEFAULT_COLUMNS: [&str; 5] = ["state", "model", "activity", "context", "last"];

fn parse(path: &Path) -> Result<JobsFile> {
    let doc: JobsFile =
        serde_yaml::from_str(&fs::read_to_string(path)?).context("invalid jobs.yaml")?;
    ensure!(
        doc.version == 1,
        "unsupported jobs version {}; expected 1",
        doc.version
    );
    if let Some(bad) = doc
        .columns
        .iter()
        .flatten()
        .find(|c| !COLUMNS.contains(&c.as_str()))
    {
        bail!("unknown column {bad:?}; columns are {}", COLUMNS.join(", "));
    }
    Ok(doc)
}

/// The dashboard's session columns: `columns:` from jobs.yaml, or the default when the file is
/// missing or invalid, so the fleet view works without any jobs.
pub fn columns(path: &Path) -> Vec<String> {
    parse(path)
        .ok()
        .and_then(|d| d.columns)
        .unwrap_or_else(|| DEFAULT_COLUMNS.iter().map(|c| (*c).to_owned()).collect())
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

/// A one-off job for `cones run --prompt`: the template's policy (or the read-only defaults)
/// with a fresh name, the given prompt and `cwd`. Unique names keep ad-hoc runs out of each
/// other's overlap rules.
pub fn adhoc(template: Option<&ResolvedJob>, prompt: &str, cwd: &Path) -> Result<ResolvedJob> {
    ensure!(
        !prompt.trim().is_empty() && !prompt.contains('\0'),
        "prompt must be nonempty and contain no NUL"
    );
    let name = format!("adhoc-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let cwd = fs::canonicalize(cwd)?;
    Ok(match template {
        Some(t) => ResolvedJob {
            name,
            schedule: "-".into(),
            prompt: prompt.to_owned(),
            cwd,
            enabled: true,
            ..t.clone()
        },
        None => ResolvedJob {
            name,
            schedule: "-".into(),
            harness: HarnessKind::Claude,
            cwd,
            prompt: prompt.to_owned(),
            model: None,
            enabled: true,
            archive_transcript: false,
            env: vec![],
            timeout_min: 30.0,
            budget_usd: 2.0,
            daily_budget_usd: None,
            write: false,
            tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            max_turns: None,
            codex_full_access: false,
            overlap: Overlap::Skip,
            notify: false,
        },
    })
}

pub fn read_jobs(path: &Path) -> Result<Vec<ResolvedJob>> {
    let path =
        fs::canonicalize(path).with_context(|| format!("read jobs file {}", path.display()))?;
    let doc = parse(&path)?;
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
    let tools = tools.unwrap_or_else(|| match j.harness {
        HarnessKind::Claude => vec!["Read".into(), "Grep".into(), "Glob".into()],
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
