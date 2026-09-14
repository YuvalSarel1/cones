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
}
