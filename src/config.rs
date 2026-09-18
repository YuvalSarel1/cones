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
    Opencode,
    Gemini,
    #[serde(rename = "cursor-agent")]
    Cursor,
    Copilot,
    Amp,
    Droid,
    Kimi,
}

impl HarnessKind {
    pub fn terminal_only(self) -> bool {
        matches!(
            self,
            Self::Gemini | Self::Cursor | Self::Copilot | Self::Amp | Self::Droid | Self::Kimi
        )
    }
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor-agent",
            Self::Copilot => "copilot",
            Self::Amp => "amp",
            Self::Droid => "droid",
            Self::Kimi => "kimi",
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

/// What to do about ticks that passed while the Mac was off or logged out. launchd loses them:
/// it coalesces a slept-through tick into one launch on wake, but never replays a tick missed
/// while powered down, and never wakes the Mac.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CatchUp {
    #[default]
    Skip,
    Once,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub timeout_min: Option<f64>,
    pub write: Option<bool>,
    pub codex_full_access: Option<bool>,
    pub overlap: Option<Overlap>,
    pub catch_up: Option<CatchUp>,
    pub notify: Option<bool>,
    pub archive_transcript: Option<bool>,
    /// Variable names every run imports; a job's own list replaces it.
    pub env: Option<Vec<String>>,
    /// Per-harness model defaults; each job has a single `model` override.
    pub model: Option<String>,
    pub codex_model: Option<String>,
    pub pi_model: Option<String>,
    pub pi_provider: Option<String>,
    pub opencode_model: Option<String>,
    pub gemini_model: Option<String>,
    pub cursor_model: Option<String>,
    pub copilot_model: Option<String>,
    pub kimi_model: Option<String>,

    /// Per-harness reasoning effort, named after the flag each harness takes.
    /// Codex and OpenCode have no such flag, so neither has a key here.
    pub effort: Option<String>,
    pub pi_thinking: Option<String>,
    /// Harnesses the composer offers. Unset is enabled; `false` takes the harness out of
    /// the composer's cycle, and cones neither starts it nor probes it for a session.
    pub claude_enabled: Option<bool>,
    pub codex_enabled: Option<bool>,
    pub pi_enabled: Option<bool>,
    pub opencode_enabled: Option<bool>,
    pub gemini_enabled: Option<bool>,
    pub cursor_enabled: Option<bool>,
    pub copilot_enabled: Option<bool>,
    pub amp_enabled: Option<bool>,
    pub droid_enabled: Option<bool>,
    pub kimi_enabled: Option<bool>,

    /// `true` selects Bedrock, `false` the native provider, `None` the harness configuration.
    pub bedrock: Option<bool>,
    /// Passed to whichever harness a run names. Both must be configured when `bedrock`
    /// is true; shell values do not satisfy validation.
    pub aws_profile: Option<String>,
    pub aws_region: Option<String>,
    /// Default job harness; the composer starts on `Start::harness`.
    pub harness: Option<HarnessKind>,
}

impl Policy {
    pub fn model_for(&self, kind: HarnessKind) -> Option<&str> {
        match kind {
            HarnessKind::Claude => self.model.as_deref(),
            HarnessKind::Codex => self.codex_model.as_deref(),
            HarnessKind::Pi => self.pi_model.as_deref(),
            HarnessKind::Opencode => self.opencode_model.as_deref(),
            HarnessKind::Gemini => self.gemini_model.as_deref(),
            HarnessKind::Cursor => self.cursor_model.as_deref(),
            HarnessKind::Copilot => self.copilot_model.as_deref(),
            HarnessKind::Amp => None,
            HarnessKind::Droid => None,
            HarnessKind::Kimi => self.kimi_model.as_deref(),
        }
    }

    /// A harness no setting mentions is enabled, so an older file offers what it always did.
    pub fn effort_for(&self, kind: HarnessKind) -> Option<&str> {
        match kind {
            HarnessKind::Claude => self.effort.as_deref(),
            HarnessKind::Pi => self.pi_thinking.as_deref(),
            HarnessKind::Codex
            | HarnessKind::Opencode
            | HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => None,
        }
    }

    pub fn enabled_for(&self, kind: HarnessKind) -> bool {
        match kind {
            HarnessKind::Claude => self.claude_enabled,
            HarnessKind::Codex => self.codex_enabled,
            HarnessKind::Pi => self.pi_enabled,
            HarnessKind::Opencode => self.opencode_enabled,
            HarnessKind::Gemini => self.gemini_enabled,
            HarnessKind::Cursor => self.cursor_enabled,
            HarnessKind::Copilot => self.copilot_enabled,
            HarnessKind::Amp => self.amp_enabled,
            HarnessKind::Droid => self.droid_enabled,
            HarnessKind::Kimi => self.kimi_enabled,
        }
        .unwrap_or(true)
    }

    pub fn provider_for(&self, kind: HarnessKind) -> Option<&str> {
        (kind == HarnessKind::Pi)
            .then_some(self.pi_provider.as_deref())
            .flatten()
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_transcript: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    // Keep these explicit: serde flatten cannot enforce unknown-field rejection reliably.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_full_access: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap: Option<Overlap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catch_up: Option<CatchUp>,
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
            archive_transcript: None,
            env: vec![],
            timeout_min: None,
            write: None,
            codex_full_access: None,
            overlap: None,
            catch_up: None,
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
    pub run_columns: Option<Vec<String>>,
    #[serde(default)]
    pub job_columns: Option<Vec<String>>,
    #[serde(default)]
    pub history_columns: Option<Vec<String>>,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub pane: Option<Pane>,
    #[serde(default)]
    pub start: Option<Start>,
    /// Confirmation timeout in seconds; zero waits until the next key.
    #[serde(default)]
    pub confirm_secs: Option<f64>,
    /// Leave out a table column the list's right edge would cut through.
    #[serde(default)]
    pub whole_columns: Option<bool>,
}

pub const WHOLE_COLUMNS: bool = true;

pub const CONFIRM_SECS: f64 = 2.0;

pub fn check_confirm_secs(secs: f64) -> Result<()> {
    ensure!(
        secs.is_finite() && (0.0..=600.0).contains(&secs),
        "confirm_secs {secs}: seconds from 0 to 600, 0 keeps the mark until the next key"
    );
    Ok(())
}

pub const COLUMNS: [&str; 15] = [
    "harness",
    "state",
    "model",
    "effort",
    "age",
    "context",
    "tokens",
    "last_reply",
    "activity",
    "folder",
    "branch",
    "last_active",
    "cost",
    "cpu",
    "memory",
];
pub const DEFAULT_COLUMNS: [&str; 8] = [
    "state",
    "context",
    "activity",
    "model",
    "age",
    "last_active",
    "folder",
    "last_reply",
];
pub const RUN_COLUMNS: [&str; 13] = [
    "harness",
    "status",
    "started",
    "ended",
    "duration",
    "context",
    "model",
    "tokens",
    "cost",
    "reason",
    "folder",
    "trigger",
    "last_reply",
];
pub const DEFAULT_RUN_COLUMNS: [&str; 7] = [
    "status", "started", "duration", "model", "cost", "folder", "reason",
];
pub const JOB_COLUMNS: [&str; 7] = [
    "harness", "status", "schedule", "next_run", "model", "last_run", "folder",
];
pub const DEFAULT_JOB_COLUMNS: [&str; 6] = [
    "status", "schedule", "next_run", "model", "last_run", "folder",
];
pub const HISTORY_COLUMNS: [&str; 9] = [
    "harness",
    "model",
    "age",
    "context",
    "tokens",
    "last_reply",
    "folder",
    "last_active",
    "cost",
];
pub const DEFAULT_HISTORY_COLUMNS: [&str; 5] =
    ["last_active", "folder", "model", "context", "last_reply"];

/// Old column ids remain readable; saves use the explicit names.
pub fn column_name(name: &str) -> &str {
    match name {
        "dir" => "folder",
        "took" => "duration",
        "last" => "last_reply",
        name => name,
    }
}

pub fn column_set(key: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match key {
        "run_columns" => (&RUN_COLUMNS, &DEFAULT_RUN_COLUMNS),
        "job_columns" => (&JOB_COLUMNS, &DEFAULT_JOB_COLUMNS),
        "history_columns" => (&HISTORY_COLUMNS, &DEFAULT_HISTORY_COLUMNS),
        _ => (&COLUMNS, &DEFAULT_COLUMNS),
    }
}

/// Activity settings; omitted fields use built-ins. See docs/jobs.md.
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
    /// Percent of the frame the viewer pane takes; the list keeps the rest.
    #[serde(default = "fifty")]
    pub ratio: u16,
}

fn right() -> String {
    "right".into()
}

fn fifty() -> u16 {
    50
}

pub const SIDES: [&str; 2] = ["right", "bottom"];
/// The pane's share of the frame, as offered by the config editor.
pub const RATIOS: [&str; 5] = ["30", "40", "50", "60", "70"];

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
        Self {
            at: right(),
            ratio: fifty(),
        }
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
        ensure!(
            (30..=70).contains(&self.ratio),
            "pane ratio {}: 30 to 70 percent of the frame",
            self.ratio
        );
        Ok(())
    }

    pub fn lines(&self) -> Vec<String> {
        vec![
            "pane:".to_owned(),
            format!("  at: {}", self.at),
            format!("  ratio: {}", self.ratio),
        ]
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

/// The schema `cones` writes and reads. Older files are migrated on the first read.
pub const VERSION: u32 = 3;

/// What each earlier version called a setting the current one renamed: `(version, was, is)`,
/// oldest first. A rename is the only migration a text rewrite can do, which is all the
/// versions so far have needed; a change of meaning would need its own step here.
const RENAMES: [(u32, &str, &str); 1] = [(1, "sparkline", "activity")];

/// Settings a later version stopped having, `(version, key)`: their lines are deleted so a
/// file written for the older schema still loads, since every field here is denied as unknown.
const DROPS: [(u32, &str); 3] = [(2, "budget_usd"), (2, "daily_budget_usd"), (2, "max_turns")];

/// Read the version alone. The whole file cannot be deserialized before migrating it,
/// since a renamed key is an unknown field.
#[derive(Deserialize)]
struct Version {
    version: u32,
}

/// Rename the keys an older file uses, in its own text, so comments and layout survive.
/// Only a top-level key and the `columns` line are touched, so the same word in a prompt is
/// the user's and stays. A trailing comment on the `columns` line is renamed with it, which
/// is what a comment about that line should say anyway.
fn migrated(text: &str, from: u32) -> String {
    let renames: Vec<_> = RENAMES.iter().filter(|(v, ..)| *v >= from).collect();
    let drops: Vec<_> = DROPS.iter().filter(|(v, _)| *v >= from).collect();
    let out: Vec<String> = text
        .lines()
        // A dropped setting is a `defaults` or job key, two or four spaces in, so a prompt
        // written as a block scalar keeps every line of its own that says the same word.
        .filter(|l| {
            !drops.iter().any(|(_, key)| {
                ["  ", "    "]
                    .iter()
                    .any(|pad| l.starts_with(&format!("{pad}{key}:")))
            })
        })
        .map(|l| {
            if l.starts_with("version:") {
                return format!("version: {VERSION}");
            }
            let mut line = l.to_owned();
            for (_, was, is) in &renames {
                if line.trim_end() == format!("{was}:") {
                    line = format!("{is}:");
                } else if line.starts_with("columns:") {
                    line = line.replace(was, is);
                }
            }
            line
        })
        .collect();
    // A `defaults` block whose every setting was dropped would read back as null, not a policy.
    let out: Vec<String> = out
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            l.trim_end() != "defaults:" || out.get(i + 1).is_some_and(|next| next.starts_with("  "))
        })
        .map(|(_, l)| l.to_owned())
        .collect();
    out.join("\n") + "\n"
}

fn parse(path: &Path) -> Result<JobsFile> {
    let mut text = fs::read_to_string(path)?;
    let found: Version = serde_yaml::from_str(&text).context("invalid jobs.yaml")?;
    if found.version < VERSION {
        text = migrated(&text, found.version);
        // Write it back so the file says what it means, but a file cones cannot rewrite
        // still loads: every writer migrates the text it edits, so the old words never
        // end up beside the new ones.
        let _ = save(path, text.clone());
    }
    let mut doc: JobsFile = serde_yaml::from_str(&text).context("invalid jobs.yaml")?;
    ensure!(
        doc.version == VERSION,
        "unsupported jobs version {}; expected {VERSION}",
        doc.version
    );
    for (key, set) in [
        ("columns", &mut doc.columns),
        ("run_columns", &mut doc.run_columns),
        ("job_columns", &mut doc.job_columns),
        ("history_columns", &mut doc.history_columns),
    ] {
        let allowed = column_set(key).0;
        for name in set.iter_mut().flatten() {
            *name = column_name(name).to_owned();
            ensure!(
                allowed.contains(&name.as_str()),
                "unknown column {name:?} in {key}; columns are {}",
                allowed.join(", ")
            );
        }
        if let Some(set) = set {
            let mut seen = BTreeSet::new();
            set.retain(|name| seen.insert(name.clone()));
        }
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

/// Read the whole-columns toggle, falling back to the built-in if missing or invalid.
pub fn whole_columns(path: &Path) -> bool {
    file_whole_columns(path).unwrap_or(WHOLE_COLUMNS)
}

/// Read without applying defaults; missing or invalid files return `None`.
pub fn file_whole_columns(path: &Path) -> Option<bool> {
    parse(path).ok().and_then(|d| d.whole_columns)
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

pub fn run_columns(path: &Path) -> Vec<String> {
    file_run_columns(path).unwrap_or_else(|| {
        DEFAULT_RUN_COLUMNS
            .iter()
            .map(|c| (*c).to_owned())
            .collect()
    })
}

pub fn file_run_columns(path: &Path) -> Option<Vec<String>> {
    parse(path).ok().and_then(|d| d.run_columns)
}

pub fn job_columns(path: &Path) -> Vec<String> {
    file_job_columns(path).unwrap_or_else(|| {
        DEFAULT_JOB_COLUMNS
            .iter()
            .map(|c| (*c).to_owned())
            .collect()
    })
}

pub fn file_job_columns(path: &Path) -> Option<Vec<String>> {
    parse(path).ok().and_then(|d| d.job_columns)
}

pub fn history_columns(path: &Path) -> Vec<String> {
    file_history_columns(path).unwrap_or_else(|| {
        DEFAULT_HISTORY_COLUMNS
            .iter()
            .map(|c| (*c).to_owned())
            .collect()
    })
}

pub fn file_history_columns(path: &Path) -> Option<Vec<String>> {
    parse(path).ok().and_then(|d| d.history_columns)
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
    let text = migrated(&text, 1);
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
    put("write", d.write.map(|v| v.to_string()));
    put("model", d.model.clone());
    put("codex_model", d.codex_model.clone());
    put("pi_model", d.pi_model.clone());
    put("pi_provider", d.pi_provider.clone());
    put("opencode_model", d.opencode_model.clone());
    put("effort", d.effort.clone());
    put("pi_thinking", d.pi_thinking.clone());
    for (key, value) in [
        ("claude_enabled", d.claude_enabled),
        ("codex_enabled", d.codex_enabled),
        ("pi_enabled", d.pi_enabled),
        ("opencode_enabled", d.opencode_enabled),
    ] {
        put(key, value.map(|v| v.to_string()));
    }
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
    put(
        "catch_up",
        d.catch_up.map(|v| {
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
    put(
        "archive_transcript",
        d.archive_transcript.map(|v| v.to_string()),
    );
    put("env", d.env.as_ref().map(|e| format!("[{}]", e.join(", "))));
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

/// A name a run may import. The config editor asks before writing the line, so a name
/// YAML would read as something other than a string never reaches the file.
pub fn env_name(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.bytes().enumerate().all(|(i, b)| b == b'_'
                || b.is_ascii_alphabetic()
                || (i > 0 && b.is_ascii_digit())),
        "invalid environment variable name: {key}"
    );
    ensure!(
        !matches!(
            key,
            "HOME" | "PATH" | "SHELL" | "BASH_ENV" | "ENV" | "NODE_OPTIONS" | "CLAUDE_CONFIG_DIR"
        ) && !key.starts_with("DYLD_")
            && !key.starts_with("LD_")
            && !key.starts_with("CLAUDE_CODE_"),
        "{key} can override execution policy and cannot be imported"
    );
    Ok(())
}

/// Change only one column set, preserving the other settings and job blocks.
/// `None` restores inheritance; an empty slice explicitly hides every optional column.
pub fn write_column_set(path: &Path, key: &str, columns: Option<&[String]>) -> Result<()> {
    ensure!(
        matches!(
            key,
            "columns" | "run_columns" | "job_columns" | "history_columns"
        ),
        "unknown column set: {key}"
    );
    let names = columns.map(|cols| {
        cols.iter()
            .map(|c| column_name(c).to_owned())
            .collect::<Vec<_>>()
    });
    if let Some(names) = &names {
        for name in names {
            ensure!(
                column_set(key).0.contains(&name.as_str()),
                "unknown {key} column: {name}"
            );
        }
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("version: {VERSION}\njobs: []\n")
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let text = migrated(&text, 1);
    let mut out: Vec<String> = text.lines().map(str::to_owned).collect();
    let lines: Vec<&str> = out.iter().map(String::as_str).collect();
    let (start, mut end) = top_level(&lines, &format!("{key}:")).unwrap_or((out.len(), out.len()));
    while end > start + 1
        && (out[end - 1].trim().is_empty() || out[end - 1].trim_start().starts_with('#'))
    {
        end -= 1;
    }
    let replacement = names
        .map(|names| vec![format!("{key}: [{}]", names.join(", "))])
        .unwrap_or_default();
    out.splice(start..end, replacement);
    save(path, out.join("\n") + "\n")
}

/// Validate and replace dashboard settings and defaults while preserving job blocks.
/// Create a missing file with `jobs: []`; validate defaults even when no jobs exist.
/// One argument per top-level setting, since each is written on its own.
#[allow(clippy::too_many_arguments)]
pub fn write_config(
    path: &Path,
    d: &Policy,
    columns: Option<&[String]>,
    activity: Option<&Activity>,
    pane: Option<&Pane>,
    start: Option<&Start>,
    confirm_secs: Option<f64>,
    whole_columns: Option<bool>,
    run_columns: Option<&[String]>,
    job_columns: Option<&[String]>,
    history_columns: Option<&[String]>,
) -> Result<()> {
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_owned);
    resolve(Job::new("defaults", "0 9 * * *", &base, "check"), d, &base)?;
    let text =
        fs::read_to_string(path).unwrap_or_else(|_| format!("version: {VERSION}\njobs: []\n"));
    // Migrating the text every writer edits is what lets a file cones failed to rewrite on
    // read keep working: the old word is renamed here rather than joined by the new one.
    let text = migrated(&text, 1);
    let mut out: Vec<String> = text.lines().map(str::to_owned).collect();
    let block = defaults_lines(d);
    let block = if block.len() == 1 { vec![] } else { block };
    let column_lines = |key: &str, cols: Option<&[String]>| {
        cols.map(|c| {
            let names: Vec<&str> = c.iter().map(|c| column_name(c)).collect();
            vec![format!("{key}: [{}]", names.join(", "))]
        })
        .unwrap_or_default()
    };
    let cols = column_lines("columns", columns);
    let run_cols = column_lines("run_columns", run_columns);
    let job_cols = column_lines("job_columns", job_columns);
    let history_cols = column_lines("history_columns", history_columns);
    let spark = activity.map(Activity::lines).unwrap_or_default();
    let pane = pane.map(Pane::lines).unwrap_or_default();
    let start = start.map(Start::lines).unwrap_or_default();
    let mark = confirm_secs
        .map(|s| vec![format!("confirm_secs: {s}")])
        .unwrap_or_default();
    let whole = whole_columns
        .map(|w| vec![format!("whole_columns: {w}")])
        .unwrap_or_default();
    // Replace blocks in reverse order to keep offsets valid; insert missing ones after their predecessor.
    let order = [
        "defaults:",
        "columns:",
        "run_columns:",
        "job_columns:",
        "history_columns:",
        "whole_columns:",
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
        ("whole_columns:", whole),
        ("history_columns:", history_cols),
        ("job_columns:", job_cols),
        ("run_columns:", run_cols),
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
    pub effort: Option<String>,
    pub enabled: bool,
    pub archive_transcript: bool,
    pub env: Vec<String>,
    pub timeout_min: f64,
    pub write: bool,
    pub codex_full_access: bool,
    pub overlap: Overlap,
    pub catch_up: CatchUp,
    pub notify: bool,
    pub bedrock: Option<bool>,
    /// Validated AWS profile and region, passed to every harness; `bedrock: true`
    /// requires both.
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
            effort: None,
            enabled: true,
            archive_transcript: false,
            env: vec![],
            timeout_min: 30.0,
            write: false,
            codex_full_access: false,
            overlap: Overlap::Skip,
            catch_up: CatchUp::Skip,
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
            // The login agent that catches up missed ticks holds this LaunchAgent label.
            ensure!(
                j.name != crate::launchd::CATCHUP,
                "{} is reserved for the catch-up agent; name the job something else",
                crate::launchd::CATCHUP
            );
            resolve(j, &doc.defaults, path.parent().unwrap())
        })
        .collect()
}

/// Require explicit profile and region for Bedrock. Validation must not depend on
/// the caller's environment; credentials are inherited separately at launch. Both
/// values reach the run whichever harness it names, since every harness resolves AWS
/// the same way; `bedrock: true` is what makes them mandatory.
pub fn bedrock_aws(
    bedrock: Option<bool>,
    profile: Option<&str>,
    region: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let set = |v: Option<&str>| v.map(str::to_owned).filter(|v| !v.trim().is_empty());
    let (profile, region) = (set(profile), set(region));
    if bedrock != Some(true) {
        return Ok((profile, region));
    }
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
    let timeout = j.timeout_min.or(d.timeout_min).unwrap_or(30.0);
    ensure!(
        timeout.is_finite() && timeout > 0.0 && timeout <= 10080.0,
        "job {}: timeout_min must be positive and at most 10080",
        j.name
    );
    launchd::calendar_intervals(&j.schedule).with_context(|| format!("job {} schedule", j.name))?;
    ensure!(
        !j.prompt.trim().is_empty() && !j.prompt.contains('\0'),
        "job {}: prompt must be nonempty and contain no NUL",
        j.name
    );
    let model = j.model.or_else(|| d.model_for(kind).map(str::to_owned));
    ensure!(
        model
            .as_ref()
            .is_none_or(|s| !s.is_empty() && !s.contains('\0')),
        "job {}: model must be nonempty and contain no NUL",
        j.name
    );
    // Effort has no per-job override: the harness default answers every run of it.
    let effort = d.effort_for(kind).map(str::to_owned);
    ensure!(
        effort
            .as_ref()
            .is_none_or(|s| !s.is_empty() && !s.contains('\0')),
        "job {}: effort must be nonempty and contain no NUL",
        j.name
    );
    let cwd = expand_path(&j.cwd, base)?;
    ensure!(
        cwd.is_dir(),
        "job {}: cwd is not a directory: {}",
        j.name,
        cwd.display()
    );
    // A job's own list replaces the default one, so a job can import nothing by writing `env: []`
    // only when the defaults name none; there is no per-name removal.
    let env = if j.env.is_empty() {
        d.env.clone().unwrap_or_default()
    } else {
        j.env
    };
    for key in &env {
        env_name(key).with_context(|| format!("job {}", j.name))?;
    }
    let write = j.write.or(d.write).unwrap_or(false);
    let overlap = j.overlap.or(d.overlap).unwrap_or_default();
    let catch_up = j.catch_up.or(d.catch_up).unwrap_or_default();
    let bedrock = j.bedrock.or(d.bedrock);
    // Only a harness whose definition names a Bedrock switch can be sent to Bedrock by
    // cones. Codex reaches its model through the app-server daemon, which keeps the
    // provider its configuration started with, and pi and OpenCode choose Bedrock by
    // provider instead. cones refuses the switch rather than implying it arrives.
    let takes_the_switch = |kind| crate::harness::spec::spec(kind).bedrock_switch().is_some();
    ensure!(
        bedrock.is_none() || takes_the_switch(kind),
        "job {}: bedrock cannot be set on a {kind} job, here or in defaults. Only {} takes \
         a Bedrock switch from cones; on {kind}, choose the provider in its own \
         configuration, in a native home of its own when the model needs a region it was \
         not started with",
        j.name,
        crate::harness::spec::known()
            .iter()
            .filter(|k| takes_the_switch(**k))
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
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
        effort,
        enabled: j.enabled,
        archive_transcript: j
            .archive_transcript
            .or(d.archive_transcript)
            .unwrap_or(false),
        env,
        timeout_min: timeout,
        write,
        codex_full_access: full,
        overlap,
        catch_up,
        notify: j.notify.or(d.notify).unwrap_or(false),
        bedrock,
        aws_profile,
        aws_region,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "version: 1\ndefaults:\n  timeout_min: 5   # quick\n  budget_usd: 1.0\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: first\n\n  # two runs at night\n  - name: two\n    schedule: \"0 2 * * *\"\n    harness: claude\n    cwd: .\n    prompt: second\n    model: sonnet\ncolumns: [state]\n";

    fn file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("jobs.yaml");
        fs::write(&p, text).unwrap();
        (d, p)
    }

    #[test]
    fn a_column_save_preserves_other_settings_comments_and_explicit_empty_sets() {
        let text = "version: 3\ndefaults:\n  timeout_min: 7 # keep\ncolumns:\n  - state\n  - model\n# run preferences\nrun_columns: []\njob_columns: [schedule]\nhistory_columns: [folder]\nwhole_columns: false\njobs: []\n";
        let (_d, p) = file(text);
        write_column_set(&p, "columns", Some(&["context".into()])).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text.replace("columns:\n  - state\n  - model\n", "columns: [context]\n")
        );
        write_column_set(&p, "columns", Some(&[])).unwrap();
        assert_eq!(file_columns(&p), Some(vec![]));
        assert_eq!(file_run_columns(&p), Some(vec![]));
        write_column_set(&p, "columns", None).unwrap();
        assert_eq!(file_columns(&p), None);
        assert_eq!(columns(&p), DEFAULT_COLUMNS);
        assert!(
            fs::read_to_string(&p)
                .unwrap()
                .contains("# run preferences\nrun_columns: []")
        );
    }

    #[test]
    fn column_saves_validate_and_create_only_the_requested_set() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new.yaml");
        write_column_set(&p, "run_columns", Some(&["dir".into()])).unwrap();
        assert_eq!(run_columns(&p), ["folder"]);
        let before = fs::read_to_string(&p).unwrap();
        assert!(write_column_set(&p, "defaults", Some(&[])).is_err());
        assert!(write_column_set(&p, "columns", Some(&["made_up".into()])).is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), before);
        let directory = d.path().join("unreadable.yaml");
        fs::create_dir(&directory).unwrap();
        assert!(write_column_set(&directory, "columns", Some(&[])).is_err());
        assert!(directory.is_dir());
    }

    #[test]
    fn write_job_adds_edits_and_removes_one_block_and_leaves_the_rest_alone() {
        let (_d, p) = file(FILE);
        let three = Job::new("three", "*/5 * * * *", Path::new("."), "third");
        write_job(&p, None, Some(&three)).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  timeout_min: 5   # quick\n"),
            "comments survive"
        );
        assert!(
            !text.contains("budget_usd"),
            "a setting a later version dropped is deleted on migration: {text}"
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
    fn a_defaults_block_of_dropped_settings_alone_leaves_with_them() {
        let (_d, p) = file(
            "version: 2\ndefaults:\n  budget_usd: 1.0\n  daily_budget_usd: 4.0\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: p\n    max_turns: 5\n",
        );
        assert_eq!(read_jobs(&p).unwrap().len(), 1, "an older file still loads");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "version: 3\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: p\n",
            "the empty block goes with its settings"
        );
    }

    #[test]
    fn write_job_handles_an_empty_list_both_ways() {
        let (_d, p) = file("version: 3\njobs: []\n");
        let one = Job::new("one", "0 9 * * *", Path::new("."), "first");
        write_job(&p, None, Some(&one)).unwrap();
        assert_eq!(raw_jobs(&p).unwrap().len(), 1);
        write_job(&p, Some("one"), None).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "version: 3\njobs: []\n");
    }

    #[test]
    fn bedrock_is_refused_on_a_harness_whose_definition_names_no_switch() {
        let job = |harness: &str| {
            format!(
                "version: 1\ndefaults:\n  bedrock: true\n  aws_profile: claude\n  aws_region: us-east-1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    harness: {harness}\n"
            )
        };
        let (_d, claude) = file(&job("claude"));
        assert_eq!(read_jobs(&claude).unwrap()[0].bedrock, Some(true));
        // Codex keeps the provider its daemon started with, and pi and OpenCode choose
        // Bedrock as a provider, so none of the three takes a switch from cones.
        for harness in ["codex", "pi", "opencode"] {
            let (_d, path) = file(&job(harness));
            let e = format!("{:#}", read_jobs(&path).unwrap_err());
            assert!(
                e.contains(&format!("bedrock cannot be set on a {harness} job")),
                "an ignored switch is a validation error: {e}"
            );
            assert!(
                e.contains("Only claude takes a Bedrock switch"),
                "a file that stopped loading is fixable from the message alone: {e}"
            );
        }
    }

    #[test]
    fn an_aws_profile_and_region_reach_a_job_on_any_harness_without_bedrock() {
        let file_for = |harness: &str| {
            format!(
                "version: 1\ndefaults:\n  aws_profile: claude\n  aws_region: us-east-1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    harness: {harness}\n"
            )
        };
        for harness in ["claude", "codex", "pi", "opencode"] {
            let (_d, path) = file(&file_for(harness));
            let job = &read_jobs(&path).unwrap()[0];
            assert_eq!(
                (
                    job.bedrock,
                    job.aws_profile.as_deref(),
                    job.aws_region.as_deref()
                ),
                (None, Some("claude"), Some("us-east-1")),
                "{harness} resolves AWS itself, and the pair is not a Bedrock-only field"
            );
        }
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
            "version: 1\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    bedrock: true\n    aws_profile: claude\n    aws_region: us-east-1\n  - name: two\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: go\n    aws_profile: other\n",
        );
        let jobs = read_jobs(&p).unwrap();
        assert_eq!(jobs[0].aws_profile.as_deref(), Some("claude"));
        assert_eq!(
            jobs[1].aws_profile.as_deref(),
            Some("other"),
            "a profile without bedrock is this job's AWS, not a Bedrock-only field"
        );
    }

    #[test]
    fn category_columns_preserve_each_other_and_accept_old_column_aliases() {
        let (_d, path) = file(
            "version: 3\ncolumns: [last, folder, last_reply]\nrun_columns: [dir, took, last]\njob_columns: [next_run, schedule]\nhistory_columns: []\njobs: []\n",
        );
        assert_eq!(columns(&path), ["last_reply", "folder"]);
        assert_eq!(run_columns(&path), ["folder", "duration", "last_reply"]);
        assert_eq!(job_columns(&path), ["next_run", "schedule"]);
        assert!(history_columns(&path).is_empty());
        let agents = file_columns(&path).unwrap();
        let runs = file_run_columns(&path).unwrap();
        let jobs = file_job_columns(&path).unwrap();
        write_config(
            &path,
            &Policy::default(),
            Some(&agents),
            None,
            None,
            None,
            None,
            None,
            Some(&runs),
            Some(&jobs),
            Some(&[]),
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("run_columns: [folder, duration, last_reply]"));
        assert!(text.contains("history_columns: []"));
        assert_eq!(job_columns(&path), jobs);
        let error = write_config(
            &path,
            &Policy::default(),
            Some(&agents),
            None,
            None,
            None,
            None,
            None,
            Some(&runs),
            Some(&jobs),
            Some(&["activity".into()]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("in history_columns"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text,
            "invalid history columns do not change another category"
        );
    }

    #[test]
    fn run_columns_save_independently_and_can_hide_every_optional_column() {
        let (_d, p) = file(FILE);
        let session = ["state".into()];
        let runs = ["model".into(), "context".into(), "harness".into()];
        let save = |columns: Option<&[String]>| {
            write_config(
                &p,
                &Policy::default(),
                Some(&session),
                None,
                None,
                None,
                None,
                None,
                columns,
                None,
                None,
            )
        };
        assert_eq!(run_columns(&p), DEFAULT_RUN_COLUMNS);
        save(Some(&runs)).unwrap();
        assert_eq!(run_columns(&p), runs);
        assert_eq!(columns(&p), session);
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("  # two runs at night\n"));
        let err = save(Some(&["activity".into()])).unwrap_err();
        assert!(err.to_string().contains("in run_columns"), "{err}");
        assert_eq!(fs::read_to_string(&p).unwrap(), text);
        save(Some(&[])).unwrap();
        assert!(run_columns(&p).is_empty());
        assert_eq!(file_run_columns(&p), Some(Vec::new()));
        save(None).unwrap();
        assert_eq!(run_columns(&p), DEFAULT_RUN_COLUMNS);
        assert!(!fs::read_to_string(&p).unwrap().contains("run_columns:"));
        assert_eq!(columns(&p), session);
    }

    #[test]
    fn write_config_replaces_the_blocks_creates_them_and_checks_them() {
        let (_d, p) = file(FILE);
        let d = Policy {
            timeout_min: Some(5.0),
            harness: None,
            write: Some(true),
            overlap: Some(Overlap::Replace),
            catch_up: Some(CatchUp::Once),
            notify: Some(true),
            codex_full_access: None,
            model: None,
            codex_model: None,
            pi_model: None,
            pi_provider: None,
            opencode_model: None,
            effort: None,
            pi_thinking: None,
            bedrock: None,
            aws_profile: None,
            aws_region: None,
            archive_transcript: Some(true),
            env: Some(vec!["FOO".to_owned()]),
            claude_enabled: None,
            codex_enabled: None,
            pi_enabled: None,
            opencode_enabled: Some(false),
            ..Policy::default()
        };
        let cols = ["state".to_owned(), "age".to_owned()];
        write_config(
            &p,
            &d,
            Some(&cols),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 3\ndefaults:\n  timeout_min: 5\n  write: true\n  opencode_enabled: false\n  overlap: replace\n  catch_up: once\n  notify: true\n  archive_transcript: true\n  env: [FOO]\njobs:\n"),
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
        let written = defaults(&p);
        assert!(!written.enabled_for(HarnessKind::Opencode));
        assert!(
            written.enabled_for(HarnessKind::Claude),
            "unset stays offered"
        );
        assert!(read_jobs(&p).unwrap()[0].write);

        // The defaults are resolved before the file is touched, so a name the sequence
        // could not be read back from never reaches it.
        let bad = Policy {
            env: Some(vec!["A: B".to_owned()]),
            ..Default::default()
        };
        assert!(
            write_config(
                &p, &bad, None, None, None, None, None, None, None, None, None
            )
            .is_err(),
            "a name YAML would read as a mapping is refused"
        );
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text,
            "and nothing is written"
        );

        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("version: 3\njobs:\n"), "{text}");
        assert!(!text.contains("columns"), "{text}");
        let d = Policy {
            notify: Some(true),
            ..Default::default()
        };
        write_config(
            &p,
            &d,
            Some(&[]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 3\ndefaults:\n  notify: true\ncolumns: []\njobs:\n"),
            "{text}"
        );
        assert_eq!(file_columns(&p), Some(Vec::new()));
        write_config(
            &p,
            &Policy::default(),
            Some(&cols),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 3\ncolumns: [state, age]\njobs:\n"),
            "{text}"
        );
        let err = write_config(
            &p,
            &d,
            Some(&["speed".to_owned()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown column"), "{err}");
        assert_eq!(file_columns(&p).as_deref(), Some(&cols[..]), "untouched");

        let missing = p.with_file_name("new.yaml");
        write_config(
            &missing, &d, None, None, None, None, None, None, None, None, None,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&missing).unwrap(),
            "version: 3\ndefaults:\n  notify: true\njobs: []\n"
        );
        let bad = Policy {
            timeout_min: Some(0.0),
            ..Default::default()
        };
        let err = write_config(
            &missing, &bad, None, None, None, None, None, None, None, None, None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("timeout_min must be positive"), "{err}");
        assert!(
            fs::read_to_string(&missing)
                .unwrap()
                .contains("notify: true"),
            "untouched"
        );
        assert!(!missing.with_extension("tmp").exists());
    }

    #[test]
    fn a_version_1_file_is_migrated_in_place_on_the_first_read() {
        let (_d, p) = file(
            "version: 1\ncolumns: [context, sparkline, model]   # mine\nsparkline:\n  metric: tokens\n  bound: row\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: fix the sparkline\n",
        );
        assert_eq!(activity(&p).metric, "tokens", "the old block is read");
        assert_eq!(
            columns(&p),
            ["context", "activity", "model"],
            "and the old column name with it"
        );
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(
            text,
            "version: 3\ncolumns: [context, activity, model]   # mine\nactivity:\n  metric: tokens\n  bound: row\njobs:\n  - name: one\n    schedule: \"0 9 * * *\"\n    cwd: .\n    prompt: fix the sparkline\n",
            "the file says what it means, keeping its comment and the word in the prompt: {text}"
        );
        let keep = file_activity(&p);
        write_config(
            &p,
            &Policy::default(),
            None,
            keep.as_ref(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            file_activity(&p).map(|a| a.bound),
            Some("row".to_owned()),
            "and a save leaves one block, not the old one beside the new"
        );

        // A file cones cannot rewrite still loads, and a save migrates it instead.
        let (_d, p) = file("version: 1\nsparkline:\n  metric: tools\njobs: []\n");
        let mode = |m| {
            fs::set_permissions(_d.path(), std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap()
        };
        mode(0o500);
        assert_eq!(
            activity(&p).metric,
            "tools",
            "read from the text as written"
        );
        mode(0o700);
        assert!(fs::read_to_string(&p).unwrap().contains("sparkline:"));
        write_job(
            &p,
            None,
            Some(&Job::new("one", "0 9 * * *", Path::new("."), "go")),
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.starts_with("version: 3\nactivity:\n  metric: tools\n"),
            "the writer migrates what the read could not: {text}"
        );
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
            None,
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
            let err = write_config(
                &p,
                &Policy::default(),
                None,
                Some(&bad),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains(msg), "{err}");
            assert_eq!(activity(&p), sp, "untouched after {msg}");
        }
        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
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
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  bound: 20\nconfirm_secs: 3.5\njobs:"),
            "the line follows the block: {text}"
        );
        assert_eq!((confirm_secs(&p), file_confirm_secs(&p)), (3.5, Some(3.5)));
        let err = write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            Some(-1.0),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("confirm_secs -1"), "{err}");
        assert_eq!(confirm_secs(&p), 3.5, "untouched after a refused value");
        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
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
            None,
            None,
            None,
            None,
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
    fn the_whole_columns_toggle_is_read_and_written_beside_the_column_list() {
        let (_d, p) = file(FILE);
        assert!(whole_columns(&p), "the built-in without a line");
        assert_eq!(file_whole_columns(&p), None);
        write_config(
            &p,
            &Policy::default(),
            Some(&["state".to_owned()]),
            None,
            None,
            None,
            None,
            Some(false),
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("columns: [state]\nwhole_columns: false\n"),
            "the line follows the column list: {text}"
        );
        assert_eq!(
            (whole_columns(&p), file_whole_columns(&p)),
            (false, Some(false))
        );
        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("whole_columns"));
        assert!(whole_columns(&p));
    }

    #[test]
    fn the_pane_block_is_read_checked_and_written() {
        let (_d, p) = file(FILE);
        assert_eq!(pane(&p), Pane::default(), "built-in without a block");
        assert_eq!(file_pane(&p), None);
        let pn = Pane {
            at: "bottom".into(),
            ratio: 30,
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
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("  bound: fleet\npane:\n  at: bottom\n  ratio: 30\nconfirm_secs: 2\n"),
            "the block sits between activity and confirm_secs: {text}"
        );
        assert_eq!((pane(&p), file_pane(&p)), (pn.clone(), Some(pn.clone())));
        let bad = Pane {
            at: "left".into(),
            ratio: 50,
        };
        let err = write_config(
            &p,
            &Policy::default(),
            None,
            None,
            Some(&bad),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pane at \"left\""), "{err}");
        assert_eq!(pane(&p), pn, "untouched after a side that is not a side");
        write_config(
            &p,
            &Policy::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!fs::read_to_string(&p).unwrap().contains("pane"));
        let (_d, p) = file("version: 1\npane:\n  at: bottom\njobs: []\n");
        assert_eq!(
            pane(&p),
            Pane {
                at: "bottom".into(),
                ratio: 50
            },
            "an omitted ratio is the built-in half"
        );
        let (_d, p) = file("version: 2\npane:\n  at: right\n  ratio: 80\njobs: []\n");
        assert_eq!(
            pane(&p),
            Pane::default(),
            "a share outside 30 to 70 fails validation and the built-ins stand"
        );
    }

    #[test]
    fn a_harness_default_applies_only_to_that_harness_and_a_job_keeps_its_own_model() {
        let (_d, p) = file(
            "version: 1\ndefaults:\n  model: sonnet\n  codex_model: o3\n  codex_full_access: true\njobs:\n  - name: c\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n  - name: x\n    schedule: \"0 9 * * *\"\n    harness: codex\n    cwd: .\n    prompt: p\n  - name: own\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n    model: opus\n",
        );
        let jobs = read_jobs(&p).unwrap();
        let (c, x, own) = (&jobs[0], &jobs[1], &jobs[2]);
        assert!(!c.codex_full_access);
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert!(
            x.codex_full_access,
            "Codex's own default does not reach a Claude job"
        );
        assert_eq!(x.model.as_deref(), Some("o3"));
        assert_eq!(own.model.as_deref(), Some("opus"));
        let text = fs::read_to_string(&p).unwrap();
        let d = defaults(&p);
        write_config(&p, &d, None, None, None, None, None, None, None, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text,
            "the block round-trips"
        );
    }

    #[test]
    fn native_launch_defaults_survive_config_edits_without_reaching_other_harnesses() {
        let (_dir, path) = file("version: 3\njobs: []\n");
        let policy = Policy {
            pi_model: Some("pi-native-model".into()),
            pi_provider: Some("pi-native-provider".into()),
            opencode_model: Some("opencode-provider/native-model".into()),
            pi_thinking: Some("minimal".into()),
            ..Policy::default()
        };
        write_config(
            &path, &policy, None, None, None, None, None, None, None, None, None,
        )
        .unwrap();
        let saved = defaults(&path);
        assert_eq!(saved, policy);
        assert_eq!(saved.model_for(HarnessKind::Pi), Some("pi-native-model"));
        assert_eq!(
            saved.provider_for(HarnessKind::Pi),
            Some("pi-native-provider")
        );
        assert_eq!(
            saved.model_for(HarnessKind::Opencode),
            Some("opencode-provider/native-model")
        );
        assert_eq!(saved.provider_for(HarnessKind::Opencode), None);
        assert_eq!(saved.effort_for(HarnessKind::Pi), Some("minimal"));
        for kind in [HarnessKind::Claude, HarnessKind::Codex] {
            assert_eq!(saved.model_for(kind), None);
            assert_eq!(saved.provider_for(kind), None);
            assert_eq!(saved.effort_for(kind), None);
        }
    }

    #[test]
    fn a_claude_job_runs_at_the_configured_effort_and_no_other_harness_takes_it() {
        let (_d, p) = file(
            "version: 3\ndefaults:\n  effort: high\n  pi_thinking: minimal\njobs:\n  - name: c\n    schedule: \"0 9 * * *\"\n    harness: claude\n    cwd: .\n    prompt: p\n  - name: x\n    schedule: \"0 9 * * *\"\n    harness: codex\n    cwd: .\n    prompt: p\n",
        );
        let jobs = read_jobs(&p).unwrap();
        assert_eq!(jobs[0].effort.as_deref(), Some("high"));
        assert_eq!(
            jobs[1].effort.as_deref(),
            None,
            "Codex has no effort flag, so its job passes none"
        );
        let saved = defaults(&p);
        assert_eq!(saved.effort_for(HarnessKind::Claude), Some("high"));
        assert_eq!(saved.effort_for(HarnessKind::Pi), Some("minimal"));
        for kind in [HarnessKind::Codex, HarnessKind::Opencode] {
            assert_eq!(saved.effort_for(kind), None);
        }
    }
}
