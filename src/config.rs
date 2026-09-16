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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub timeout_min: Option<f64>,
    pub budget_usd: Option<f64>,
    pub daily_budget_usd: Option<f64>,
    pub write: Option<bool>,
    pub max_turns: Option<u32>,
    pub codex_full_access: Option<bool>,
    pub overlap: Option<Overlap>,
    pub notify: Option<bool>,
    /// Per-harness model defaults; each job has a single `model` override.
    pub model: Option<String>,
    pub codex_model: Option<String>,
    /// `true` selects Bedrock, `false` the native provider, `None` the harness configuration.
    pub bedrock: Option<bool>,
    /// Both must be configured when `bedrock` is true; shell values do not satisfy validation.
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
    /// Default job harness; the composer starts on `Start::harness`.
    pub harness: Option<HarnessKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    /// Unset takes `defaults.harness`, else Claude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessKind>,
    pub cwd: PathBuf,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archive_transcript: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    // Keep these explicit: serde flatten cannot enforce unknown-field rejection reliably.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_budget_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_full_access: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap: Option<Overlap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bedrock: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
}

impl Job {
    pub fn new(name: &str, schedule: &str, cwd: &Path, prompt: &str) -> Self {
        Self {
            name: name.to_owned(),
            schedule: schedule.to_owned(),
            harness: None,
            cwd: cwd.to_owned(),
            prompt: prompt.to_owned(),
            model: None,
            enabled: true,
            archive_transcript: false,
            env: vec![],
            timeout_min: None,
            budget_usd: None,
            daily_budget_usd: None,
            write: None,
            max_turns: None,
            codex_full_access: None,
            overlap: None,
            notify: None,
            bedrock: None,
            aws_profile: None,
            aws_region: None,
        }
    }
}

fn is_true(b: &bool) -> bool {
    *b
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
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub pane: Option<Pane>,
    #[serde(default)]
    pub start: Option<Start>,
    /// Confirmation timeout in seconds; zero waits until the next key.
    #[serde(default)]
    pub confirm_secs: Option<f64>,
}

pub const CONFIRM_SECS: f64 = 2.0;

pub fn check_confirm_secs(secs: f64) -> Result<()> {
    ensure!(
        secs.is_finite() && (0.0..=600.0).contains(&secs),
        "confirm_secs {secs}: seconds from 0 to 600, 0 keeps the mark until the next key"
    );
    Ok(())
}

pub const COLUMNS: [&str; 8] = [
    "harness", "state", "model", "age", "context", "tokens", "last", "activity",
];
pub const DEFAULT_COLUMNS: [&str; 7] = [
    "harness", "state", "context", "activity", "model", "age", "last",
];

/// Activity settings; omitted fields use built-ins. See docs/dashboard.md.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    #[serde(default = "sixteen")]
    pub bars: usize,
    /// `15s`, `1m`, `5m`, `1h`: a count of seconds, minutes or hours.
    #[serde(default = "one_minute")]
    pub bucket: String,
    /// `lines`, every transcript line; `messages`, assistant replies; `tools`, tool calls;
    /// `tokens`, output tokens.
    #[serde(default = "lines")]
    pub metric: String,
    /// `fleet`, `row`, `log`, or a positive numeric bound.
    #[serde(default = "fleet")]
    pub bound: String,
}

fn sixteen() -> usize {
    16
}
fn one_minute() -> String {
    "1m".into()
}
fn lines() -> String {
    "lines".into()
}
fn fleet() -> String {
    "fleet".into()
}

pub const METRICS: [&str; 4] = ["lines", "messages", "tools", "tokens"];
pub const BOUNDS: [&str; 3] = ["fleet", "row", "log"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pane {
    #[serde(default = "right")]
    pub at: String,
}

fn right() -> String {
    "right".into()
}

pub const SIDES: [&str; 2] = ["right", "bottom"];

/// Initial dashboard state; runtime toggles do not write it back.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Start {
    #[serde(default = "claude")]
    pub harness: HarnessKind,
    #[serde(default = "yes")]
    pub pane: bool,
}

fn claude() -> HarnessKind {
    HarnessKind::Claude
}

impl Default for Start {
    fn default() -> Self {
        Self {
            harness: claude(),
            pane: yes(),
        }
    }
}

impl Start {
    pub fn lines(&self) -> Vec<String> {
        vec![
            "start:".to_owned(),
            format!("  harness: {}", self.harness),
            format!("  pane: {}", self.pane),
        ]
    }
}

impl Default for Pane {
    fn default() -> Self {
        Self { at: right() }
    }
}

impl Pane {
    pub fn check(&self) -> Result<()> {
        ensure!(
            SIDES.contains(&self.at.as_str()),
            "pane at {:?}: any of {}",
            self.at,
            SIDES.join(", ")
        );
        Ok(())
    }

    pub fn lines(&self) -> Vec<String> {
        vec!["pane:".to_owned(), format!("  at: {}", self.at)]
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            bars: sixteen(),
            bucket: one_minute(),
            metric: lines(),
            bound: fleet(),
        }
    }
}

impl Activity {
    pub fn bucket_seconds(&self) -> Result<u64> {
        let t = self.bucket.trim();
        let what = || format!("activity bucket {t:?}: a count of s, m or h, as in 1m");
        let (n, unit) = t.split_at(t.len() - t.chars().last().map_or(0, char::len_utf8));
        let n: u64 = n.parse().ok().filter(|n| *n > 0).with_context(what)?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            _ => bail!(what()),
        };
        ensure!(secs <= 86400, "activity bucket {t:?}: at most 24h");
        Ok(secs)
    }

    /// A fixed bound as a number, None for `fleet`, `row` or `log`.
    pub fn fixed_bound(&self) -> Option<f64> {
        self.bound.trim().parse::<f64>().ok().filter(|b| *b > 0.0)
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.bars),
            "activity bars {}: 1 to 64",
            self.bars
        );
        self.bucket_seconds()?;
        ensure!(
            METRICS.contains(&self.metric.as_str()),
            "activity metric {:?}: any of {}",
            self.metric,
            METRICS.join(", ")
        );
        ensure!(
            BOUNDS.contains(&self.bound.as_str()) || self.fixed_bound().is_some(),
            "activity bound {:?}: any of {}, or a positive number",
            self.bound,
            BOUNDS.join(", ")
        );
        Ok(())
    }

    pub fn lines(&self) -> Vec<String> {
        vec![
            "activity:".to_owned(),
            format!("  bars: {}", self.bars),
            format!("  bucket: {}", self.bucket),
            format!("  metric: {}", self.metric),
            format!("  bound: {}", self.bound),
        ]
    }
}

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
    if let Some(sp) = &doc.activity {
        sp.check()?;
    }
    if let Some(p) = &doc.pane {
        p.check()?;
    }
    if let Some(secs) = doc.confirm_secs {
        check_confirm_secs(secs)?;
    }
    Ok(doc)
}

/// Read activity settings, falling back to built-ins if missing or invalid.
pub fn activity(path: &Path) -> Activity {
    parse(path)
        .ok()
        .and_then(|d| d.activity)
        .unwrap_or_default()
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_activity(path: &Path) -> Option<Activity> {
    parse(path).ok().and_then(|d| d.activity)
}

/// Read pane settings, falling back to built-ins if missing or invalid.
pub fn pane(path: &Path) -> Pane {
    file_pane(path).unwrap_or_default()
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_pane(path: &Path) -> Option<Pane> {
    parse(path).ok().and_then(|d| d.pane)
}

pub fn confirm_secs(path: &Path) -> f64 {
    file_confirm_secs(path).unwrap_or(CONFIRM_SECS)
}

/// Read startup settings, falling back to built-ins if missing or invalid.
pub fn start(path: &Path) -> Start {
    file_start(path).unwrap_or_default()
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_start(path: &Path) -> Option<Start> {
    parse(path).ok().and_then(|d| d.start)
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_confirm_secs(path: &Path) -> Option<f64> {
    parse(path).ok().and_then(|d| d.confirm_secs)
}

/// Read columns, falling back to built-ins if missing or invalid.
pub fn columns(path: &Path) -> Vec<String> {
    parse(path)
        .ok()
        .and_then(|d| d.columns)
        .unwrap_or_else(|| DEFAULT_COLUMNS.iter().map(|c| (*c).to_owned()).collect())
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_columns(path: &Path) -> Option<Vec<String>> {
    parse(path).ok().and_then(|d| d.columns)
}

/// The jobs as written, before defaults and path expansion: what the wizard edits.
pub fn raw_jobs(path: &Path) -> Result<Vec<Job>> {
    Ok(parse(path)?.jobs)
}

/// Locate job blocks by text to preserve surrounding comments and formatting.
/// Return item indent, `(name, start, end)` blocks, and list end.
fn job_blocks(lines: &[&str], jobs_at: usize) -> (usize, Vec<(String, usize, usize)>, usize) {
    let mut starts: Vec<usize> = vec![];
    let mut indent = 2;
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(jobs_at + 1) {
        let t = l.trim_start();
        let ind = l.len() - t.len();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if ind == 0 && !t.starts_with("- ") {
            end = i;
            break;
        }
        if t.starts_with("- ") && (starts.is_empty() || ind == indent) {
            indent = ind;
            starts.push(i);
        }
    }
    let blocks = starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let e = starts.get(n + 1).copied().unwrap_or(end);
            let name = lines[s..e]
                .iter()
                .map(|l| l.trim_start().trim_start_matches("- ").trim_start())
                .find_map(|l| l.strip_prefix("name:"))
                .map(|v| v.trim().trim_matches(['"', '\'']).to_owned())
                .unwrap_or_default();
            (name, s, e)
        })
        .collect();
    (indent, blocks, end)
}

/// Replace or append `job`, or delete `old` when `job` is `None`. Validate the whole
/// file before replacing it; the caller must reinstall launchd jobs.
pub fn write_job(path: &Path, old: Option<&str>, job: Option<&Job>) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let jobs_at = lines
        .iter()
        .position(|l| l.starts_with("jobs:"))
        .ok_or_else(|| anyhow::anyhow!("{} has no jobs: list", path.display()))?;
    let (indent, blocks, end) = job_blocks(&lines, jobs_at);
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let (at, removed) = match blocks.iter().find(|(n, _, _)| Some(n.as_str()) == old) {
        Some(&(_, s, e)) => {
            out.drain(s..e);
            (s, true)
        }
        None => (end, false),
    };
    match job {
        Some(job) => {
            let pad = " ".repeat(indent);
            let block = serde_yaml::to_string(job)?;
            let block = block.lines().enumerate().map(|(i, l)| {
                if i == 0 {
                    format!("{pad}- {l}")
                } else {
                    format!("{pad}  {l}")
                }
            });
            out.splice(at..at, block);
            out[jobs_at] = "jobs:".into();
        }
        None => {
            ensure!(removed, "no job named {}", old.unwrap_or(""));
            if blocks.len() == 1 {
                out[jobs_at] = "jobs: []".into();
            }
        }
    }
    save(path, out.join("\n") + "\n")?;
    Ok(())
}

/// Read defaults, returning an empty policy if the file is missing or invalid.
pub fn defaults(path: &Path) -> Policy {
    parse(path).map(|d| d.defaults).unwrap_or_default()
}

fn defaults_lines(d: &Policy) -> Vec<String> {
    let mut out = vec!["defaults:".to_owned()];
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("  {k}: {v}"));
        }
    };
    put("timeout_min", d.timeout_min.map(|v| v.to_string()));
    put("budget_usd", d.budget_usd.map(|v| v.to_string()));
    put(
        "daily_budget_usd",
        d.daily_budget_usd.map(|v| v.to_string()),
    );
    put("write", d.write.map(|v| v.to_string()));
    put("max_turns", d.max_turns.map(|v| v.to_string()));
    put("model", d.model.clone());
    put("codex_model", d.codex_model.clone());
    put(
        "codex_full_access",
        d.codex_full_access.map(|v| v.to_string()),
    );
    put(
        "overlap",
        d.overlap.map(|v| {
            serde_yaml::to_string(&v)
                .unwrap_or_default()
                .trim()
                .to_owned()
        }),
    );
    put("notify", d.notify.map(|v| v.to_string()));
    put("bedrock", d.bedrock.map(|v| v.to_string()));
    put("aws_profile", d.aws_profile.clone());
    put("aws_region", d.aws_region.clone());
    put("harness", d.harness.map(|v| v.to_string()));
    out
}

/// Find a top-level block, excluding trailing blank lines.
fn top_level(lines: &[&str], key: &str) -> Option<(usize, usize)> {
    let s = lines.iter().position(|l| l.starts_with(key))?;
    let mut e = lines
        .iter()
        .enumerate()
        .skip(s + 1)
        .find(|(_, l)| !l.starts_with([' ', '\t', '#']) && !l.trim().is_empty())
        .map_or(lines.len(), |(i, _)| i);
    while e > s + 1 && lines[e - 1].trim().is_empty() {
        e -= 1;
    }
    Some((s, e))
}

/// Validate and replace dashboard settings and defaults while preserving job blocks.
/// Create a missing file with `jobs: []`; validate defaults even when no jobs exist.
pub fn write_config(
    path: &Path,
    d: &Policy,
    columns: Option<&[String]>,
    activity: Option<&Activity>,
    pane: Option<&Pane>,
    start: Option<&Start>,
    confirm_secs: Option<f64>,
) -> Result<()> {
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_owned);
    resolve(Job::new("defaults", "0 9 * * *", &base, "check"), d, &base)?;
    let text = fs::read_to_string(path).unwrap_or_else(|_| {
        "version: 1
jobs: []
"
        .to_owned()
    });
    let mut out: Vec<String> = text.lines().map(str::to_owned).collect();
    let block = defaults_lines(d);
    let block = if block.len() == 1 { vec![] } else { block };
    let cols = columns
        .filter(|c| !c.is_empty())
        .map(|c| vec![format!("columns: [{}]", c.join(", "))])
        .unwrap_or_default();
    let spark = activity.map(Activity::lines).unwrap_or_default();
    let pane = pane.map(Pane::lines).unwrap_or_default();
    let start = start.map(Start::lines).unwrap_or_default();
    let mark = confirm_secs
        .map(|s| vec![format!("confirm_secs: {s}")])
        .unwrap_or_default();
    // Replace blocks in reverse order to keep offsets valid; insert missing ones after their predecessor.
    let order = [
        "defaults:",
        "columns:",
        "activity:",
        "pane:",
        "start:",
        "confirm_secs:",
    ];
    for (key, block) in [
        ("confirm_secs:", mark),
        ("start:", start),
        ("pane:", pane),
        ("activity:", spark),
        ("columns:", cols),
        ("defaults:", block),
    ] {
        let lines: Vec<&str> = out.iter().map(String::as_str).collect();
        let at = match top_level(&lines, key) {
            Some((s, e)) => s..e,
            None => {
                let before = order.iter().position(|k| *k == key).unwrap_or(0);
                let at = order[..before]
                    .iter()
                    .rev()
                    .find_map(|k| top_level(&lines, k))
                    .map(|(_, e)| e)
                    .or_else(|| {
                        lines
                            .iter()
                            .position(|l| l.starts_with("version:"))
                            .map(|i| i + 1)
                    })
                    .unwrap_or(0);
                at..at
            }
        };
        out.splice(at, block);
    }
    save(path, out.join("\n") + "\n")?;
    Ok(())
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
    pub max_turns: Option<u32>,
    pub codex_full_access: bool,
    pub overlap: Overlap,
    pub notify: bool,
    pub bedrock: Option<bool>,
    /// Validated Bedrock profile and region; both absent unless `bedrock` is true.
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
}

/// Use the template policy or read-only defaults, with a unique name to isolate overlap checks.
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
            max_turns: None,
            codex_full_access: false,
            overlap: Overlap::Skip,
            notify: false,
            bedrock: None,
            aws_profile: None,
            aws_region: None,
        },
    })
}

/// Validate the new text in a sibling file before it replaces the old one. The sibling is
/// created fresh so a planted symlink there cannot redirect the write.
fn save(path: &Path, text: String) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let checked = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
        .map_err(Into::into)
        .and_then(|()| read_jobs(&tmp).map(drop));
    if checked.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    checked?;
    fs::rename(&tmp, path)?;
    Ok(())
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

/// Require explicit profile and region for Bedrock. Validation must not depend on
/// the caller's environment; credentials are inherited separately at launch.
pub fn bedrock_aws(
    bedrock: Option<bool>,
    profile: Option<&str>,
    region: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    if bedrock != Some(true) {
        return Ok((None, None));
    }
    let set = |v: Option<&str>| v.map(str::to_owned).filter(|v| !v.trim().is_empty());
    let (profile, region) = (set(profile), set(region));
    let missing: Vec<&str> = [("aws_profile", &profile), ("aws_region", &region)]
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| *n)
        .collect();
    // Name missing fields so the config editor can focus them.
    ensure!(
        missing.is_empty(),
        "{}: needed by bedrock: true, since Bedrock is reached with a profile and a region \
         and the switch on its own is a session that dies on its first call",
        missing.first().unwrap_or(&"aws_profile")
    );
    Ok((profile, region))
}

fn resolve(j: Job, d: &Policy, base: &Path) -> Result<ResolvedJob> {
    // Apply harness-specific defaults only to matching jobs; explicit job values are always validated.
    let kind = j.harness.or(d.harness).unwrap_or(HarnessKind::Claude);
    let claude = kind == HarnessKind::Claude;
    let full = j
        .codex_full_access
        .or(d.codex_full_access.filter(|_| kind == HarnessKind::Codex))
        .unwrap_or(false);
    ensure!(
        !full || !claude,
        "job {}: codex_full_access applies only to Codex",
        j.name
    );
    let max_turns = j.max_turns.or(d.max_turns.filter(|_| claude));
    ensure!(
        max_turns != Some(0),
        "job {}: max_turns must be positive",
        j.name
    );
    ensure!(
        max_turns.is_none() || claude,
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
    let model = j.model.or_else(|| match kind {
        HarnessKind::Claude => d.model.clone(),
        HarnessKind::Codex => d.codex_model.clone(),
        HarnessKind::Pi => None,
    });
    ensure!(
        model
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
    let bedrock = j.bedrock.or(d.bedrock);
    // Codex reaches its model through the app-server daemon, which takes its provider
    // from the Codex configuration it started with and ignores what a thread asks for.
    // cones cannot hold a Codex session to this switch, so it refuses to imply that it can.
    ensure!(
        kind != HarnessKind::Codex || bedrock.is_none(),
        "job {}: bedrock cannot be set on a Codex job, here or in defaults. Choose the \
         provider in the Codex configuration instead, in a Codex home of its own when \
         the model needs a region the daemon was not started with",
        j.name
    );
    let (aws_profile, aws_region) = bedrock_aws(
        bedrock,
        j.aws_profile.as_deref().or(d.aws_profile.as_deref()),
        j.aws_region.as_deref().or(d.aws_region.as_deref()),
    )
    .with_context(|| format!("job {}", j.name))?;
    Ok(ResolvedJob {
        name: j.name,
        schedule: j.schedule,
        harness: kind,
        cwd: fs::canonicalize(cwd)?,
        prompt: j.prompt,
        model,
        enabled: j.enabled,
        archive_transcript: j.archive_transcript,
        env: j.env,
        timeout_min: timeout,
        budget_usd: budget,
        daily_budget_usd: daily,
        write,
        max_turns,
        codex_full_access: full,
        overlap,
        notify: j.notify.or(d.notify).unwrap_or(false),
        bedrock,
        aws_profile,
        aws_region,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "version: 1\ndefaults:\n  budget_usd: 1.0   # cheap\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: first\n\n  # two runs at night\n  - name: two\n    schedule: \"0 2 * * *\"\n    harness: claude\n    cwd: .\n    prompt: second\n    model: sonnet\ncolumns: [state]\n";

    fn file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("jobs.yaml");
        fs::write(&p, text).unwrap();
        (d, p)
    }

    #[test]
    fn write_job_adds_edits_and_removes_one_block_and_leaves_the_rest_alone() {
        let (_d, p) = file(FILE);
        let three = Job::new("three", "*/5 * * * *", Path::new("."), "third");
        write_job(&p, None, Some(&three)).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  budget_usd: 1.0   # cheap\n"),
            "comments survive"
        );
        assert!(text.contains("  # two runs at night\n"));
        assert!(
            text.ends_with("    prompt: third\ncolumns: [state]\n"),
            "{text}"
        );
        assert!(!text.contains("model: null"), "unset fields are left out");
        let names: Vec<String> = raw_jobs(&p).unwrap().into_iter().map(|j| j.name).collect();
        assert_eq!(names, ["one", "two", "three"]);

        let mut two = raw_jobs(&p).unwrap().remove(1);
        two.prompt = "second, revised".into();
        write_job(&p, Some("two"), Some(&two)).unwrap();
        let jobs = raw_jobs(&p).unwrap();
        assert_eq!(jobs[1].prompt, "second, revised");
        assert_eq!(jobs[1].model.as_deref(), Some("sonnet"));
        assert_eq!(jobs[2].name, "three");

        write_job(&p, Some("one"), None).unwrap();
        let names: Vec<String> = raw_jobs(&p).unwrap().into_iter().map(|j| j.name).collect();
        assert_eq!(names, ["two", "three"]);
        assert!(write_job(&p, Some("nine"), None).is_err());
    }

    #[test]
    fn write_job_validates_before_replacing_the_file() {
        let (_d, p) = file(FILE);
        let bad = Job::new("bad", "0 9 * * *", Path::new("/nonexistent/dir"), "x");
        let err = write_job(&p, None, Some(&bad)).unwrap_err().to_string();
        assert!(err.contains("cwd is not a directory"), "{err}");
        assert_eq!(fs::read_to_string(&p).unwrap(), FILE, "untouched");
        assert!(!p.with_extension("tmp").exists());
    }

    #[test]
    fn write_job_handles_an_empty_list_both_ways() {
        let (_d, p) = file("version: 1\njobs: []\n");
        let one = Job::new("one", "0 9 * * *", Path::new("."), "first");
        write_job(&p, None, Some(&one)).unwrap();
        assert_eq!(raw_jobs(&p).unwrap().len(), 1);
        write_job(&p, Some("one"), None).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "version: 1\njobs: []\n");
    }

    #[test]
    fn bedrock_is_refused_on_a_codex_job_rather_than_passed_and_ignored() {
        let job = |harness: &str| {
            format!(
                "version: 1\ndefaults:\n  bedrock: true\n  aws_profile: claude\n  aws_region: us-east-1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    harness: {harness}\n"
            )
        };
        let (_d, claude) = file(&job("claude"));
        assert_eq!(read_jobs(&claude).unwrap()[0].bedrock, Some(true));
        let (_d, codex) = file(&job("codex"));
        let e = format!("{:#}", read_jobs(&codex).unwrap_err());
        assert!(
            e.contains("bedrock cannot be set on a Codex job"),
            "the daemon keeps its own provider, so the switch is a validation error: {e}"
        );
    }

    #[test]
    fn bedrock_is_refused_without_the_profile_and_region_it_runs_on() {
        let bedrock = |p: &str, r: &str| {
            format!(
                "version: 1\ndefaults:\n  bedrock: true\n{p}{r}jobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n"
            )
        };
        let profile = "  aws_profile: claude\n";
        let region = "  aws_region: us-east-1\n";
        for (text, want) in [
            (bedrock("", ""), Some("aws_profile")),
            (bedrock(profile, ""), Some("aws_region")),
            (bedrock("", region), Some("aws_profile")),
            (bedrock(profile, region), None),
        ] {
            let (_d, p) = file(&text);
            match (read_jobs(&p), want) {
                (Ok(jobs), None) => assert_eq!(
                    (
                        jobs[0].aws_profile.as_deref(),
                        jobs[0].aws_region.as_deref()
                    ),
                    (Some("claude"), Some("us-east-1"))
                ),
                (Err(e), Some(field)) => {
                    let e = format!("{e:#}");
                    assert!(
                        e.contains(&format!("{field}: needed by bedrock: true")),
                        "{e}"
                    );
                }
                (got, want) => panic!("{text}\nwanted {want:?}, got {got:?}"),
            }
        }
        let (_d, p) = file(
            "version: 1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    bedrock: true\n    aws_profile: claude\n    aws_region: us-east-1\n  - name: two\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    aws_profile: unused\n",
        );
        let jobs = read_jobs(&p).unwrap();
        assert_eq!(jobs[0].aws_profile.as_deref(), Some("claude"));
        assert_eq!(jobs[1].aws_profile, None, "carried but not resolved");
    }

    #[test]
    fn write_config_replaces_the_blocks_creates_them_and_checks_them() {
        let (_d, p) = file(FILE);
        let d = Policy {
            timeout_min: Some(5.0),
            budget_usd: Some(0.25),
            daily_budget_usd: Some(2.0),
            harness: None,
            write: Some(true),
            max_turns: Some(3),
            overlap: Some(Overlap::Replace),
            notify: Some(true),
            codex_full_access: None,
            model: None,
            codex_model: None,
            bedrock: None,
            aws_profile: None,
            aws_region: None,
        };
        let cols = ["state".to_owned(), "age".to_owned()];
        write_config(&p, &d, Some(&cols), None, None, None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ndefaults:\n  timeout_min: 5\n  budget_usd: 0.25\n  daily_budget_usd: 2\n  write: true\n  max_turns: 3\n  overlap: replace\n  notify: true\njobs:\n"),
            "{text}"
        );
        assert!(
            text.ends_with("    model: sonnet\ncolumns: [state, age]\n"),
            "the columns line is replaced where it is: {text}"
        );
        assert_eq!(columns(&p), cols);
        assert!(
            text.contains("  # two runs at night\n"),
            "the rest is untouched"
        );
        assert_eq!(defaults(&p).overlap, Some(Overlap::Replace));
        assert!(read_jobs(&p).unwrap()[0].write);

        write_config(&p, &Policy::default(), None, None, None, None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("version: 1\njobs:\n"), "{text}");
        assert!(!text.contains("columns"), "{text}");
        let d = Policy {
            notify: Some(true),
            ..Default::default()
        };
        write_config(&p, &d, Some(&[]), None, None, None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ndefaults:\n  notify: true\njobs:\n"),
            "{text}"
        );
        assert_eq!(file_columns(&p), None);
        write_config(&p, &Policy::default(), Some(&cols), None, None, None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ncolumns: [state, age]\njobs:\n"),
            "{text}"
        );
        let err = write_config(&p, &d, Some(&["speed".to_owned()]), None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown column"), "{err}");
        assert_eq!(file_columns(&p).as_deref(), Some(&cols[..]), "untouched");

        let missing = p.with_file_name("new.yaml");
        write_config(&missing, &d, None, None, None, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(&missing).unwrap(),
            "version: 1\ndefaults:\n  notify: true\njobs: []\n"
        );
        let bad = Policy {
            budget_usd: Some(3.0),
            daily_budget_usd: Some(1.0),
            ..Default::default()
        };
        let err = write_config(&missing, &bad, None, None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("daily_budget_usd must cover"), "{err}");
        assert!(
            fs::read_to_string(&missing)
                .unwrap()
                .contains("notify: true"),
            "untouched"
        );
        assert!(!missing.with_extension("tmp").exists());
    }

    #[test]
    fn the_activity_block_is_read_checked_and_written() {
        let (_d, p) = file(FILE);
        assert_eq!(
            activity(&p),
            Activity::default(),
            "built-in without a block"
        );
        assert_eq!(file_activity(&p), None);
        let sp = Activity {
            bars: 12,
            bucket: "5m".into(),
            metric: "tools".into(),
            bound: "20".into(),
        };
        write_config(
            &p,
            &Policy::default(),
            Some(&["state".to_owned()]),
            Some(&sp),
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.ends_with(
                "columns: [state]\nactivity:\n  bars: 12\n  bucket: 5m\n  metric: tools\n  bound: 20\n"
            ),
            "the block follows the columns line: {text}"
        );
        assert_eq!(activity(&p), sp);
        assert_eq!(sp.bucket_seconds().unwrap(), 300);
        assert_eq!(sp.fixed_bound(), Some(20.0));
        for (bad, msg) in [
            (
                Activity {
                    bars: 0,
                    ..sp.clone()
                },
                "activity bars 0",
            ),
            (
                Activity {
                    bucket: "5x".into(),
                    ..sp.clone()
                },
                "activity bucket",
            ),
            (
                Activity {
                    metric: "cost".into(),
                    ..sp.clone()
                },
                "activity metric",
            ),
            (
                Activity {
                    bound: "-3".into(),
                    ..sp.clone()
                },
                "activity bound",
            ),
        ] {
            let err = write_config(&p, &Policy::default(), None, Some(&bad), None, None, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains(msg), "{err}");
            assert_eq!(activity(&p), sp, "untouched after {msg}");
        }
        write_config(&p, &Policy::default(), None, None, None, None, None).unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("activity"));
        assert_eq!(confirm_secs(&p), CONFIRM_SECS, "built-in without a line");
        write_config(
            &p,
            &Policy::default(),
            None,
            Some(&sp),
            None,
            None,
            Some(3.5),
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  bound: 20\nconfirm_secs: 3.5\njobs:"),
            "the line follows the block: {text}"
        );
        assert_eq!((confirm_secs(&p), file_confirm_secs(&p)), (3.5, Some(3.5)));
        let err = write_config(&p, &Policy::default(), None, None, None, None, Some(-1.0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("confirm_secs -1"), "{err}");
        assert_eq!(confirm_secs(&p), 3.5, "untouched after a refused value");
        write_config(&p, &Policy::default(), None, None, None, None, None).unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("confirm_secs"));
        let (_d, p) = file("version: 1\nactivity:\n  metric: tokens\njobs: []\n");
        assert_eq!(
            activity(&p),
            Activity {
                metric: "tokens".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_job_without_a_harness_takes_the_default_harness() {
        let (_d, p) = file(
            "version: 1\ndefaults:\n  harness: codex\njobs:\n  - name: d\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: p\n  - name: own\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n",
        );
        let jobs = read_jobs(&p).unwrap();
        assert_eq!(jobs[0].harness, HarnessKind::Codex);
        assert_eq!(jobs[1].harness, HarnessKind::Claude);
        let (_d, p) = file(
            "version: 1\njobs:\n  - name: d\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: p\n",
        );
        assert_eq!(read_jobs(&p).unwrap()[0].harness, HarnessKind::Claude);
        assert_eq!(defaults(&p).harness, None);
    }

    #[test]
    fn the_start_block_is_read_and_written() {
        let (_d, p) = file(FILE);
        assert_eq!(start(&p), Start::default(), "built-in without a block");
        assert_eq!(file_start(&p), None);
        let st = Start {
            harness: HarnessKind::Codex,
            pane: false,
        };
        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            Some(&st),
            Some(2.0),
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("start:\n  harness: codex\n  pane: false\nconfirm_secs: 2\n"),
            "the block sits between pane and confirm_secs: {text}"
        );
        assert_eq!((start(&p), file_start(&p)), (st, Some(st)));
        let (_d, p) = file("version: 1\nstart:\n  pane: false\njobs: []\n");
        assert_eq!(
            start(&p),
            Start {
                harness: HarnessKind::Claude,
                pane: false,
            },
        );
    }

    #[test]
    fn the_pane_block_is_read_checked_and_written() {
        let (_d, p) = file(FILE);
        assert_eq!(pane(&p), Pane::default(), "built-in without a block");
        assert_eq!(file_pane(&p), None);
        let pn = Pane {
            at: "bottom".into(),
        };
        let sp = Activity::default();
        write_config(
            &p,
            &Policy::default(),
            None,
            Some(&sp),
            Some(&pn),
            None,
            Some(2.0),
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  bound: fleet\npane:\n  at: bottom\nconfirm_secs: 2\n"),
            "the block sits between activity and confirm_secs: {text}"
        );
        assert_eq!((pane(&p), file_pane(&p)), (pn.clone(), Some(pn.clone())));
        let bad = Pane { at: "left".into() };
        let err = write_config(&p, &Policy::default(), None, None, Some(&bad), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pane at \"left\""), "{err}");
        assert_eq!(pane(&p), pn, "untouched after a side that is not a side");
        write_config(&p, &Policy::default(), None, None, None, None, None).unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("pane"));
        let (_d, p) = file("version: 1\npane:\n  at: bottom\njobs: []\n");
        assert_eq!(
            pane(&p),
            Pane {
                at: "bottom".into()
            }
        );
    }

    #[test]
    fn a_harness_default_applies_only_to_that_harness_and_a_job_keeps_its_own_model() {
        let (_d, p) = file(
            "version: 1\ndefaults:\n  max_turns: 3\n  model: sonnet\n  codex_model: o3\n  codex_full_access: true\njobs:\n  - name: c\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n  - name: x\n    schedule: \"0 9 * * *\"\n    harness: codex\n    cwd: .\n    prompt: p\n  - name: own\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n    model: opus\n",
        );
        let jobs = read_jobs(&p).unwrap();
        let (c, x, own) = (&jobs[0], &jobs[1], &jobs[2]);
        assert_eq!((c.max_turns, c.codex_full_access), (Some(3), false));
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert_eq!(
            (x.max_turns, x.codex_full_access),
            (None, true),
            "Claude's max_turns does not reach a Codex job"
        );
        assert_eq!(x.model.as_deref(), Some("o3"));
        assert_eq!(own.model.as_deref(), Some("opus"));
        let text = fs::read_to_string(&p).unwrap();
        let d = defaults(&p);
        write_config(&p, &d, None, None, None, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text,
            "the block round-trips"
        );
    }
}
