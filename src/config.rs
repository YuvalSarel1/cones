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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// The model a Claude job runs on unless it names its own; `codex_model` the same for a
    /// Codex job. A job's `model:` is one field, so the default is per harness.
    pub model: Option<String>,
    pub codex_model: Option<String>,
    /// Where the harness sends its requests: `true` Amazon Bedrock, `false` the harness's own
    /// endpoint, unset whatever the harness's own configuration says.
    pub bedrock: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub harness: HarnessKind,
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
    pub tools: Option<Vec<String>>,
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
}

impl Job {
    /// A Claude job with only the four fields the dashboard's wizard asks for; everything else
    /// is the file's defaults.
    pub fn new(name: &str, schedule: &str, cwd: &Path, prompt: &str) -> Self {
        Self {
            name: name.to_owned(),
            schedule: schedule.to_owned(),
            harness: HarnessKind::Claude,
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
            tools: None,
            max_turns: None,
            codex_full_access: None,
            overlap: None,
            notify: None,
            bedrock: None,
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
    /// Session columns the dashboard shows after the harness and title, from `COLUMNS`.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// The `sparkline` column's window, metric and scale.
    #[serde(default)]
    pub sparkline: Option<Sparkline>,
    /// Seconds the dashboard's red `ctrl+x` mark stays with no key pressed; 0 keeps it until
    /// the next key. The built-in is `MARK_SECS`.
    #[serde(default)]
    pub mark_secs: Option<f64>,
}

pub const MARK_SECS: f64 = 2.0;

/// `mark_secs` as jobs.yaml accepts it: a finite count of seconds from 0 to 600.
pub fn check_mark_secs(secs: f64) -> Result<()> {
    ensure!(
        secs.is_finite() && (0.0..=600.0).contains(&secs),
        "mark_secs {secs}: seconds from 0 to 600, 0 keeps the mark until the next key"
    );
    Ok(())
}

pub const COLUMNS: [&str; 8] = [
    "state",
    "model",
    "age",
    "activity",
    "context",
    "tokens",
    "last",
    "sparkline",
];
pub const DEFAULT_COLUMNS: [&str; 6] =
    ["state", "context", "sparkline", "model", "activity", "last"];

/// The `sparkline` column: `bars` buckets of `bucket` each, newest on the right, one bar per
/// bucket for the `metric` counted from the transcript lines the harness wrote in it, scaled so
/// a full bar is `bound`. Every field has a built-in, so `sparkline:` may name only what changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sparkline {
    #[serde(default = "sixteen")]
    pub bars: usize,
    /// `15s`, `1m`, `5m`, `1h`: a count of seconds, minutes or hours.
    #[serde(default = "one_minute")]
    pub bucket: String,
    /// `lines`, every transcript line; `messages`, assistant replies; `tools`, tool calls;
    /// `tokens`, output tokens.
    #[serde(default = "lines")]
    pub metric: String,
    /// `fleet`, the busiest bucket on screen; `row`, the row's own busiest bucket; `log`, the
    /// fleet's on a log scale; or a number, the count that fills a bar, the same tomorrow.
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

impl Default for Sparkline {
    fn default() -> Self {
        Self {
            bars: sixteen(),
            bucket: one_minute(),
            metric: lines(),
            bound: fleet(),
        }
    }
}

impl Sparkline {
    /// The bucket in seconds, from `30s`, `5m` or `1h`.
    pub fn bucket_seconds(&self) -> Result<u64> {
        let t = self.bucket.trim();
        let what = || format!("sparkline bucket {t:?}: a count of s, m or h, as in 1m");
        let (n, unit) = t.split_at(t.len() - t.chars().last().map_or(0, char::len_utf8));
        let n: u64 = n.parse().ok().filter(|n| *n > 0).with_context(what)?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            _ => bail!(what()),
        };
        ensure!(secs <= 86400, "sparkline bucket {t:?}: at most 24h");
        Ok(secs)
    }

    /// A fixed bound as a number, None for `fleet`, `row` or `log`.
    pub fn fixed_bound(&self) -> Option<f64> {
        self.bound.trim().parse::<f64>().ok().filter(|b| *b > 0.0)
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.bars),
            "sparkline bars {}: 1 to 64",
            self.bars
        );
        self.bucket_seconds()?;
        ensure!(
            METRICS.contains(&self.metric.as_str()),
            "sparkline metric {:?}: any of {}",
            self.metric,
            METRICS.join(", ")
        );
        ensure!(
            BOUNDS.contains(&self.bound.as_str()) || self.fixed_bound().is_some(),
            "sparkline bound {:?}: any of {}, or a positive number",
            self.bound,
            BOUNDS.join(", ")
        );
        Ok(())
    }

    /// The column's name, what it covers: `last 16m`.
    pub fn title(&self) -> String {
        let secs = self.bucket_seconds().unwrap_or(60) * self.bars as u64;
        let span = match secs {
            0..60 => format!("{secs}s"),
            60..3600 => format!("{}m", secs / 60),
            _ if secs.is_multiple_of(3600) => format!("{}h", secs / 3600),
            _ => format!("{}m", secs / 60),
        };
        format!("last {span}")
    }

    /// The block as jobs.yaml lines.
    pub fn lines(&self) -> Vec<String> {
        vec![
            "sparkline:".to_owned(),
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
    if let Some(sp) = &doc.sparkline {
        sp.check()?;
    }
    if let Some(secs) = doc.mark_secs {
        check_mark_secs(secs)?;
    }
    Ok(doc)
}

/// The `sparkline` column's settings: `sparkline:` from jobs.yaml, or the built-in when the
/// file is missing, invalid or silent on it.
pub fn sparkline(path: &Path) -> Sparkline {
    parse(path)
        .ok()
        .and_then(|d| d.sparkline)
        .unwrap_or_default()
}

/// `sparkline:` as written, `None` when the file has none: what the config editor edits.
pub fn file_sparkline(path: &Path) -> Option<Sparkline> {
    parse(path).ok().and_then(|d| d.sparkline)
}

/// How long the dashboard's `ctrl+x` mark stays: `mark_secs:` from jobs.yaml, or the built-in.
pub fn mark_secs(path: &Path) -> f64 {
    file_mark_secs(path).unwrap_or(MARK_SECS)
}

/// `mark_secs:` as written, `None` when the file has none: what the config editor edits.
pub fn file_mark_secs(path: &Path) -> Option<f64> {
    parse(path).ok().and_then(|d| d.mark_secs)
}

/// The dashboard's session columns: `columns:` from jobs.yaml, or the default when the file is
/// missing or invalid, so the fleet view works without any jobs.
pub fn columns(path: &Path) -> Vec<String> {
    parse(path)
        .ok()
        .and_then(|d| d.columns)
        .unwrap_or_else(|| DEFAULT_COLUMNS.iter().map(|c| (*c).to_owned()).collect())
}

/// `columns:` as written, `None` when the file has none or cannot be read: what the config
/// editor edits.
pub fn file_columns(path: &Path) -> Option<Vec<String>> {
    parse(path).ok().and_then(|d| d.columns)
}

/// The jobs as written, before defaults and path expansion: what the wizard edits.
pub fn raw_jobs(path: &Path) -> Result<Vec<Job>> {
    Ok(parse(path)?.jobs)
}

/// One job's lines in jobs.yaml: from its `- ` item line to the next item or top-level key.
/// Found by text, so the rest of the file, comments and quoting included, is never rewritten.
/// Returns the item indent, the blocks as `(name, start, end)`, and where the list ends.
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

/// Rewrite jobs.yaml with `job` in place of the job named `old`, appended to the list when
/// there is no such job, or with that job removed when `job` is `None`. Only the one block
/// changes; the file is validated as a whole before it replaces the old one, so a bad answer
/// comes back as the error and the file is untouched. Then `cones install` is the caller's.
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
            // `jobs: []` becomes a list with an item.
            out[jobs_at] = "jobs:".into();
        }
        None => {
            ensure!(removed, "no job named {}", old.unwrap_or(""));
            if blocks.len() == 1 {
                out[jobs_at] = "jobs: []".into();
            }
        }
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, out.join("\n") + "\n")?;
    let checked = read_jobs(&tmp).map(drop);
    if checked.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    checked?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The file's `defaults` block as written, or nothing set when the file is missing or does
/// not parse, so the dashboard's config editor opens on what is there.
pub fn defaults(path: &Path) -> Policy {
    parse(path).map(|d| d.defaults).unwrap_or_default()
}

/// The `defaults:` block as jobs.yaml lines, one per set field, in the order jobs.md lists them.
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
    put(
        "tools",
        d.tools.as_ref().map(|t| format!("[{}]", t.join(", "))),
    );
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
    out
}

/// Where `key:` sits in `lines`, to the next top-level key, leaving the blank lines before it
/// where they are.
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

/// Rewrite the `defaults:` block, the `columns:` line, the `sparkline:` block and the
/// `mark_secs:` line of jobs.yaml with `d`, `columns`, `sparkline` and `mark_secs`: in place
/// when the file has them, after `version:` when it does not, and a missing file is created
/// around them with `jobs: []`. Only those change.
/// The policy is checked as a Claude job would resolve it, so a default no job could run under
/// is refused with the file untouched, whether or not the file has jobs; an unknown column or a
/// sparkline value cones cannot draw is refused the same way.
pub fn write_config(
    path: &Path,
    d: &Policy,
    columns: Option<&[String]>,
    sparkline: Option<&Sparkline>,
    mark_secs: Option<f64>,
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
    let spark = sparkline.map(Sparkline::lines).unwrap_or_default();
    let mark = mark_secs
        .map(|s| vec![format!("mark_secs: {s}")])
        .unwrap_or_default();
    // Last first, so each block's place is still where it was read: the mark line follows
    // the sparkline block, which follows the columns line, which follows the defaults block.
    for (key, block) in [
        ("mark_secs:", mark),
        ("sparkline:", spark),
        ("columns:", cols),
        ("defaults:", block),
    ] {
        let lines: Vec<&str> = out.iter().map(String::as_str).collect();
        let at = match top_level(&lines, key) {
            Some((s, e)) => s..e,
            None => {
                let at = match key {
                    "mark_secs:" => top_level(&lines, "sparkline:")
                        .or_else(|| top_level(&lines, "columns:"))
                        .or_else(|| top_level(&lines, "defaults:"))
                        .map(|(_, e)| e),
                    "sparkline:" => top_level(&lines, "columns:")
                        .or_else(|| top_level(&lines, "defaults:"))
                        .map(|(_, e)| e),
                    "columns:" => top_level(&lines, "defaults:").map(|(_, e)| e),
                    _ => None,
                }
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
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, out.join("\n") + "\n")?;
    let checked = read_jobs(&tmp).map(drop);
    if checked.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    checked?;
    fs::rename(&tmp, path)?;
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
    pub tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub codex_full_access: bool,
    pub overlap: Overlap,
    pub notify: bool,
    pub bedrock: Option<bool>,
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
            bedrock: None,
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
    // A default that belongs to one harness (tools, max_turns and model to Claude,
    // codex_full_access and codex_model to Codex) applies only to that harness's jobs; on a
    // job it is checked as written.
    let claude = j.harness == HarnessKind::Claude;
    let tools = j.tools.or_else(|| d.tools.clone().filter(|_| claude));
    if !claude && tools.is_some() {
        bail!(
            "job {}: Codex has no per-tool allowlist; remove tools and choose write: false (read-only) or write: true (workspace-write)",
            j.name
        );
    }
    let full = j
        .codex_full_access
        .or(d.codex_full_access.filter(|_| !claude))
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
    let model = j.model.or_else(|| {
        if claude {
            d.model.clone()
        } else {
            d.codex_model.clone()
        }
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
        model,
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
        bedrock: j.bedrock.or(d.bedrock),
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

        // Editing keeps the fields the wizard does not ask about and the block's place.
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
    fn write_config_replaces_the_blocks_creates_them_and_checks_them() {
        let (_d, p) = file(FILE);
        let d = Policy {
            timeout_min: Some(5.0),
            budget_usd: Some(0.25),
            daily_budget_usd: Some(2.0),
            write: Some(true),
            tools: Some(vec!["Read".into(), "Edit".into()]),
            max_turns: Some(3),
            overlap: Some(Overlap::Replace),
            notify: Some(true),
            codex_full_access: None,
            model: None,
            codex_model: None,
            bedrock: None,
        };
        let cols = ["state".to_owned(), "age".to_owned()];
        write_config(&p, &d, Some(&cols), None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ndefaults:\n  timeout_min: 5\n  budget_usd: 0.25\n  daily_budget_usd: 2\n  write: true\n  tools: [Read, Edit]\n  max_turns: 3\n  overlap: replace\n  notify: true\njobs:\n"),
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
        assert_eq!(read_jobs(&p).unwrap()[0].tools, ["Read", "Edit"]);

        // Nothing set removes the block and the line; a file without them gets them after version.
        write_config(&p, &Policy::default(), None, None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("version: 1\njobs:\n"), "{text}");
        assert!(!text.contains("columns"), "{text}");
        let d = Policy {
            notify: Some(true),
            ..Default::default()
        };
        write_config(&p, &d, Some(&[]), None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ndefaults:\n  notify: true\njobs:\n"),
            "{text}"
        );
        assert_eq!(file_columns(&p), None);
        write_config(&p, &Policy::default(), Some(&cols), None, None).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 1\ncolumns: [state, age]\njobs:\n"),
            "{text}"
        );
        let err = write_config(&p, &d, Some(&["speed".to_owned()]), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown column"), "{err}");
        assert_eq!(file_columns(&p).as_deref(), Some(&cols[..]), "untouched");

        // A missing file is created; a default no job could run under is refused, jobs or not.
        let missing = p.with_file_name("new.yaml");
        write_config(&missing, &d, None, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(&missing).unwrap(),
            "version: 1\ndefaults:\n  notify: true\njobs: []\n"
        );
        let bad = Policy {
            budget_usd: Some(3.0),
            daily_budget_usd: Some(1.0),
            ..Default::default()
        };
        let err = write_config(&missing, &bad, None, None, None)
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
    fn the_sparkline_block_is_read_checked_and_written() {
        let (_d, p) = file(FILE);
        assert_eq!(
            sparkline(&p),
            Sparkline::default(),
            "built-in without a block"
        );
        assert_eq!(file_sparkline(&p), None);
        let sp = Sparkline {
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
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.ends_with(
                "columns: [state]\nsparkline:\n  bars: 12\n  bucket: 5m\n  metric: tools\n  bound: 20\n"
            ),
            "the block follows the columns line: {text}"
        );
        assert_eq!(sparkline(&p), sp);
        assert_eq!(sp.bucket_seconds().unwrap(), 300);
        assert_eq!(sp.fixed_bound(), Some(20.0));
        assert_eq!(sp.title(), "last 1h");
        assert_eq!(Sparkline::default().title(), "last 16m");
        // A value cones cannot draw is refused with the file untouched.
        for (bad, msg) in [
            (
                Sparkline {
                    bars: 0,
                    ..sp.clone()
                },
                "sparkline bars 0",
            ),
            (
                Sparkline {
                    bucket: "5x".into(),
                    ..sp.clone()
                },
                "sparkline bucket",
            ),
            (
                Sparkline {
                    metric: "cost".into(),
                    ..sp.clone()
                },
                "sparkline metric",
            ),
            (
                Sparkline {
                    bound: "-3".into(),
                    ..sp.clone()
                },
                "sparkline bound",
            ),
        ] {
            let err = write_config(&p, &Policy::default(), None, Some(&bad), None)
                .unwrap_err()
                .to_string();
            assert!(err.contains(msg), "{err}");
            assert_eq!(sparkline(&p), sp, "untouched after {msg}");
        }
        // Nothing set removes the block.
        write_config(&p, &Policy::default(), None, None, None).unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("sparkline"));
        // The mark line follows the sparkline block, is read back, checked, and removed the
        // same way.
        assert_eq!(mark_secs(&p), MARK_SECS, "built-in without a line");
        write_config(&p, &Policy::default(), None, Some(&sp), Some(3.5)).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  bound: 20\nmark_secs: 3.5\njobs:"),
            "the line follows the block: {text}"
        );
        assert_eq!((mark_secs(&p), file_mark_secs(&p)), (3.5, Some(3.5)));
        let err = write_config(&p, &Policy::default(), None, None, Some(-1.0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mark_secs -1"), "{err}");
        assert_eq!(mark_secs(&p), 3.5, "untouched after a refused value");
        write_config(&p, &Policy::default(), None, None, None).unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("mark_secs"));
        // A block may name only what changes.
        let (_d, p) = file("version: 1\nsparkline:\n  metric: tokens\njobs: []\n");
        assert_eq!(
            sparkline(&p),
            Sparkline {
                metric: "tokens".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_harness_default_applies_only_to_that_harness_and_a_job_keeps_its_own_model() {
        let (_d, p) = file(
            "version: 1\ndefaults:\n  tools: [Read, Edit]\n  max_turns: 3\n  model: sonnet\n  codex_model: o3\n  codex_full_access: true\njobs:\n  - name: c\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n  - name: x\n    schedule: \"0 9 * * *\"\n    harness: codex\n    cwd: .\n    prompt: p\n  - name: own\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n    model: opus\n",
        );
        let jobs = read_jobs(&p).unwrap();
        let (c, x, own) = (&jobs[0], &jobs[1], &jobs[2]);
        assert_eq!(c.tools, ["Read", "Edit"]);
        assert_eq!((c.max_turns, c.codex_full_access), (Some(3), false));
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert!(
            x.tools.is_empty(),
            "Claude's tools do not reach a Codex job"
        );
        assert_eq!((x.max_turns, x.codex_full_access), (None, true));
        assert_eq!(x.model.as_deref(), Some("o3"));
        assert_eq!(own.model.as_deref(), Some("opus"));
        let text = fs::read_to_string(&p).unwrap();
        let d = defaults(&p);
        write_config(&p, &d, None, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text,
            "the block round-trips"
        );
    }
}
