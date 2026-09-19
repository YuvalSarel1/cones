//! Read-only context inspector: what instructions, skills and MCP servers a
//! session's own records show, and where they came from.
//!
//! Nothing here starts a harness client, sends a prompt or writes configuration.
//! [`Reader`] owns a worker thread and answers one request at a time; every
//! [`Response`] carries the [`Target`] it was read for, so a late result for an
//! earlier session can be dropped instead of attributed to the current one.
//!
//! Three origins are kept apart and never merged:
//! [`Origin::Captured`] is text the session recorded, [`Origin::Reconstructed`]
//! is configuration the records name without quoting, and [`Origin::OnDisk`] is
//! a file that exists next to the session but does not appear in its records.
//! Installed is not loaded. Unreadable or unsupported data stays explicit, and
//! an unknown count is reported as unknown rather than estimated.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

/// Stop before a pathological transcript; the tail of a large file is enough
/// because every record the inspector reads is written near session start.
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TEXT: usize = 256 * 1024;
const MAX_ITEMS: usize = 200;
/// Directories walked upward from the session folder while looking for
/// instruction files that are on disk but absent from the records.
const MAX_PARENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Row key; a response is only applied to the row that asked for it.
    pub key: String,
    pub harness: String,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Text the session recorded for itself.
    Captured,
    /// Named by the records without captured text.
    Reconstructed,
    /// Present in the folder, absent from the records.
    OnDisk,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Captured => "in session records",
            Origin::Reconstructed => "named, text not recorded",
            Origin::OnDisk => "on disk, not in records",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    /// Source path, only where the records attribute one.
    pub source: Option<String>,
    pub origin: Origin,
    /// Captured text. `None` means the records carry no text for this item.
    pub text: Option<String>,
    /// Set when a captured file no longer matches the copy on disk.
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    pub name: &'static str,
    pub items: Vec<Item>,
    /// Why a category is empty or partial, in the session's own terms.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub harness: String,
    pub categories: Vec<Category>,
    /// Records the session never wrote, so the inspector cannot report them.
    pub unsupported: Vec<String>,
}

impl Report {
    pub fn items(&self) -> usize {
        self.categories.iter().map(|c| c.items.len()).sum()
    }
}

pub struct Response {
    pub target: Target,
    pub result: Result<Arc<Report>>,
}

/// One outstanding request; the UI may change its desired target meanwhile.
pub struct Reader {
    requests: mpsc::Sender<Target>,
    results: mpsc::Receiver<Response>,
    busy: bool,
}

impl Reader {
    pub fn new() -> std::io::Result<Self> {
        let (requests, input) = mpsc::channel::<Target>();
        let (output, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("cones-context".into())
            .spawn(move || {
                while let Ok(target) = input.recv() {
                    let result = read(&target).map(Arc::new);
                    if output.send(Response { target, result }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            results,
            busy: false,
        })
    }

    /// False when a read is already in flight; the caller retries after polling.
    pub fn request(&mut self, target: Target) -> Result<bool> {
        if self.busy {
            return Ok(false);
        }
        self.requests
            .send(target)
            .context("context worker exited")?;
        self.busy = true;
        Ok(true)
    }

    pub fn poll(&mut self) -> Option<Response> {
        if !self.busy {
            return None;
        }
        match self.results.try_recv() {
            Ok(response) => {
                self.busy = false;
                Some(response)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                None
            }
        }
    }
}

pub fn supported(harness: &str) -> bool {
    matches!(harness, "claude" | "codex")
}

pub fn read(target: &Target) -> Result<Report> {
    let bytes = read_bounded(&target.transcript)?;
    let mut report = match target.harness.as_str() {
        "claude" => claude(&bytes),
        "codex" => codex(&bytes),
        other => bail!("{other} records no session context"),
    };
    report.harness = target.harness.clone();
    mark_changed(&mut report);
    inventory(&mut report, &target.cwd);
    Ok(report)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    if meta.len() > MAX_BYTES {
        bail!(
            "{} is {} MB; too large to inspect",
            path.display(),
            meta.len() / (1024 * 1024)
        );
    }
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_owned();
    }
    let mut end = MAX_TEXT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[truncated]", &text[..end])
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A Claude session writes its startup records again when it is resumed or
/// forked, so the last occurrence of each record is the one in force.
fn claude(bytes: &[u8]) -> Report {
    let (mut instructions, mut skills, mut mcp, mut prompt) = (None, None, None, None);
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(attachment) = record.get("attachment") else {
            continue;
        };
        match attachment.get("type").and_then(Value::as_str) {
            Some("instructions") => instructions = Some(attachment.clone()),
            Some("skill_listing") => skills = Some(attachment.clone()),
            Some("mcp_instructions_delta") => mcp = Some(attachment.clone()),
            Some("prompt_snapshot") => prompt = Some(attachment.clone()),
            _ => {}
        }
    }

    let mut files = vec![];
    let mut note = None;
    match instructions.as_ref().and_then(|a| a.get("files")) {
        Some(Value::Array(entries)) => {
            for entry in entries.iter().take(MAX_ITEMS) {
                let path = entry.get("path").and_then(Value::as_str);
                let text = entry.get("content").and_then(Value::as_str);
                files.push(Item {
                    title: path.map_or_else(
                        || {
                            entry
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("instructions")
                                .to_owned()
                        },
                        |p| name_of(p).to_owned(),
                    ),
                    source: path.map(str::to_owned),
                    origin: if text.is_some() {
                        Origin::Captured
                    } else {
                        Origin::Reconstructed
                    },
                    text: text.map(clip),
                    changed: false,
                });
            }
        }
        _ => note = Some("This session recorded no instruction files.".into()),
    }

    let mut skill_items = vec![];
    if let Some(listing) = &skills {
        let names = strings(listing.get("names"));
        let described = listing.get("content").and_then(Value::as_str).unwrap_or("");
        for name in names.iter().take(MAX_ITEMS) {
            let line = described
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("- {name}:")))
                .map(str::trim)
                .map(str::to_owned);
            skill_items.push(Item {
                title: name.clone(),
                source: None,
                origin: if line.is_some() {
                    Origin::Captured
                } else {
                    Origin::Reconstructed
                },
                text: line,
                changed: false,
            });
        }
        if let Some(count) = listing.get("skillCount").and_then(as_count)
            && count != names.len() as u64
        {
            note = note.or(Some(format!(
                "The session reports {count} skills and names {}.",
                names.len()
            )));
        }
    }
    let skill_note = if skills.is_none() {
        Some("This session recorded no skill catalogue.".into())
    } else {
        None
    };

    let mut mcp_items = vec![];
    if let Some(delta) = &mcp {
        let names = strings(delta.get("addedNames"));
        let blocks = strings(delta.get("addedBlocks"));
        for (i, name) in names.iter().enumerate().take(MAX_ITEMS) {
            let text = blocks.get(i).cloned();
            mcp_items.push(Item {
                title: name.clone(),
                source: None,
                origin: if text.is_some() {
                    Origin::Captured
                } else {
                    Origin::Reconstructed
                },
                text: text.map(|t| clip(&t)),
                changed: false,
            });
        }
    }

    let mut system = vec![];
    if let Some(snapshot) = prompt.as_ref().and_then(|p| p.get("systemPrompt")) {
        let text = match snapshot {
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            Value::String(text) => text.clone(),
            _ => String::new(),
        };
        if !text.is_empty() {
            system.push(Item {
                title: "system prompt".into(),
                source: None,
                origin: Origin::Captured,
                text: Some(clip(&text)),
                changed: false,
            });
        }
    }

    let mut unsupported = vec![];
    if mcp.is_none() {
        unsupported.push("MCP instructions were not recorded by this session.".into());
    }
    if system.is_empty() {
        unsupported.push("No system prompt snapshot was recorded.".into());
    }

    Report {
        harness: String::new(),
        categories: vec![
            Category {
                name: "Instructions",
                items: files,
                note,
            },
            Category {
                name: "Skills",
                items: skill_items,
                note: skill_note,
            },
            Category {
                name: "MCP",
                items: mcp_items,
                note: None,
            },
            Category {
                name: "System prompt",
                items: system,
                note: None,
            },
        ],
        unsupported,
    }
}

fn as_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// Codex records its base instructions in `session_meta` and the project file
/// it loaded in `world_state`. It records no skill catalogue and no MCP text.
fn codex(bytes: &[u8]) -> Report {
    let (mut base, mut project) = (None, None);
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let payload = record.get("payload").unwrap_or(&Value::Null);
        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                base = payload
                    .get("base_instructions")
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("world_state") => {
                if let Some(agents) = payload.get("state").and_then(|s| s.get("agents_md")) {
                    project = Some((
                        agents
                            .get("directory")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        agents
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    ));
                }
            }
            _ => {}
        }
    }

    let mut files = vec![];
    if let Some((directory, text)) = project {
        let source = directory.map(|d| format!("{d}/AGENTS.md"));
        files.push(Item {
            title: "AGENTS.md".into(),
            source,
            origin: if text.is_some() {
                Origin::Captured
            } else {
                Origin::Reconstructed
            },
            text: text.as_deref().map(clip),
            changed: false,
        });
    }
    let note = if files.is_empty() {
        Some("This thread recorded no project instruction file.".into())
    } else {
        None
    };

    let system = base
        .map(|text| Item {
            title: "base instructions".into(),
            source: None,
            origin: Origin::Captured,
            text: Some(clip(&text)),
            changed: false,
        })
        .into_iter()
        .collect::<Vec<_>>();

    Report {
        harness: String::new(),
        categories: vec![
            Category {
                name: "Instructions",
                items: files,
                note,
            },
            Category {
                name: "Skills",
                items: vec![],
                note: Some("Codex records no skill catalogue.".into()),
            },
            Category {
                name: "MCP",
                items: vec![],
                note: Some("Codex records no MCP instruction text.".into()),
            },
            Category {
                name: "System prompt",
                items: system,
                note: None,
            },
        ],
        unsupported: vec!["Codex threads record no skill catalogue or MCP instructions.".into()],
    }
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A captured file can have been edited since; say so instead of implying the
/// session sees what the file says now. An unreadable file stays unmarked.
fn mark_changed(report: &mut Report) {
    for category in &mut report.categories {
        for item in &mut category.items {
            let (Some(source), Some(text)) = (&item.source, &item.text) else {
                continue;
            };
            if text.ends_with("[truncated]") {
                continue;
            }
            if let Ok(disk) = fs::read_to_string(source) {
                item.changed = disk != *text;
            }
        }
    }
}

/// Files and skills that exist beside the session but do not appear in its
/// records. Bounded: known instruction names up the folder chain, and one
/// directory listing of installed skills.
fn inventory(report: &mut Report, cwd: &Path) {
    fn named<'a>(report: &'a mut Report, name: &str) -> Option<&'a mut Category> {
        report.categories.iter_mut().find(|c| c.name == name)
    }
    const NAMES: [&str; 5] = [
        "CLAUDE.md",
        "CLAUDE.local.md",
        "AGENTS.md",
        ".claude/CLAUDE.md",
        "AGENTS.override.md",
    ];
    let Some(instructions) = named(report, "Instructions") else {
        return;
    };
    let known: Vec<String> = instructions
        .items
        .iter()
        .filter_map(|i| i.source.clone())
        .collect();
    let mut found = vec![];
    for dir in cwd.ancestors().take(MAX_PARENTS) {
        for name in NAMES {
            let path = dir.join(name);
            let text = path.to_string_lossy().into_owned();
            if path.is_file() && !known.contains(&text) && !found.contains(&text) {
                found.push(text);
            }
        }
    }
    instructions
        .items
        .extend(found.into_iter().take(MAX_ITEMS).map(|path| Item {
            title: name_of(&path).to_owned(),
            source: Some(path),
            origin: Origin::OnDisk,
            text: None,
            changed: false,
        }));

    let Some(skills) = named(report, "Skills") else {
        return;
    };
    let listed: Vec<String> = skills.items.iter().map(|i| i.title.clone()).collect();
    let Ok(entries) = fs::read_dir(cwd.join(".claude/skills")) else {
        return;
    };
    let mut installed: Vec<(String, String)> = entries
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                e.path().join("SKILL.md").to_string_lossy().into_owned(),
            )
        })
        .filter(|(name, _)| !listed.contains(name))
        .collect();
    installed.sort();
    skills.items.extend(
        installed
            .into_iter()
            .take(MAX_ITEMS)
            .map(|(name, path)| Item {
                title: name,
                source: Some(path),
                origin: Origin::OnDisk,
                text: None,
                changed: false,
            }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
        let path = dir.join(name);
        let text: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .concat();
        fs::write(&path, text).unwrap();
        path
    }

    fn claude_target(dir: &Path, path: PathBuf) -> Target {
        Target {
            key: "row".into(),
            harness: "claude".into(),
            transcript: path,
            cwd: dir.to_path_buf(),
        }
    }

    fn category<'a>(report: &'a Report, name: &str) -> &'a Category {
        report
            .categories
            .iter()
            .find(|c| c.name == name)
            .expect("category")
    }

    #[test]
    fn claude_records_separate_captured_text_from_disk_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(project.join(".claude/skills/unused")).unwrap();
        fs::write(project.join(".claude/skills/unused/SKILL.md"), "x").unwrap();
        fs::write(project.join("AGENTS.md"), "team rules").unwrap();
        let loaded = project.join("CLAUDE.md");
        fs::write(&loaded, "loaded rules").unwrap();
        let path = write(
            dir.path(),
            "s.jsonl",
            &[
                json!({"attachment": {
                    "type": "instructions",
                    "files": [{"path": loaded.to_string_lossy(), "type": "Project", "content": "loaded rules"}],
                }}),
                json!({"attachment": {"type": "skill_listing", "skillCount": 1, "names": ["voice"],
                "content": "- voice: Write in Yuval's voice."}}),
            ],
        );
        let mut target = claude_target(dir.path(), path);
        target.cwd = project;

        let report = read(&target).unwrap();

        let instructions = category(&report, "Instructions");
        let captured = &instructions.items[0];
        assert_eq!(captured.title, "CLAUDE.md");
        assert_eq!(captured.origin, Origin::Captured);
        assert_eq!(captured.text.as_deref(), Some("loaded rules"));
        assert!(!captured.changed);
        let disk: Vec<_> = instructions
            .items
            .iter()
            .filter(|i| i.origin == Origin::OnDisk)
            .map(|i| i.title.as_str())
            .collect();
        assert_eq!(disk, ["AGENTS.md"], "an unloaded file stays inventory only");
        assert!(
            instructions
                .items
                .iter()
                .filter(|i| i.origin == Origin::OnDisk)
                .all(|i| i.text.is_none()),
            "disk inventory carries no text"
        );

        let skills = category(&report, "Skills");
        assert_eq!(skills.items[0].title, "voice");
        assert_eq!(skills.items[0].origin, Origin::Captured);
        let installed: Vec<_> = skills
            .items
            .iter()
            .filter(|i| i.origin == Origin::OnDisk)
            .map(|i| i.title.as_str())
            .collect();
        assert_eq!(installed, ["unused"], "installed is not loaded");
    }

    #[test]
    fn an_edited_file_is_marked_changed_without_replacing_captured_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("CLAUDE.md");
        fs::write(&file, "new text").unwrap();
        let path = write(
            dir.path(),
            "s.jsonl",
            &[json!({"attachment": {"type": "instructions",
                "files": [{"path": file.to_string_lossy(), "content": "old text"}]}})],
        );

        let report = read(&claude_target(dir.path(), path)).unwrap();

        let item = &category(&report, "Instructions").items[0];
        assert!(item.changed);
        assert_eq!(item.text.as_deref(), Some("old text"));
        assert_eq!(item.origin, Origin::Captured);
    }

    #[test]
    fn a_resumed_session_reports_its_latest_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "s.jsonl",
            &[
                json!({"attachment": {"type": "instructions",
                    "files": [{"path": "/a/CLAUDE.md", "content": "first"}]}}),
                json!({"attachment": {"type": "mcp_instructions_delta",
                    "addedNames": ["figma"], "addedBlocks": ["## figma"]}}),
                json!({"attachment": {"type": "instructions",
                    "files": [{"path": "/b/CLAUDE.md", "content": "second"}]}}),
            ],
        );

        let report = read(&claude_target(dir.path(), path)).unwrap();

        let items = &category(&report, "Instructions").items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.as_deref(), Some("/b/CLAUDE.md"));
        assert_eq!(items[0].text.as_deref(), Some("second"));
        let mcp = &category(&report, "MCP").items;
        assert_eq!(mcp[0].title, "figma");
        assert_eq!(mcp[0].text.as_deref(), Some("## figma"));
    }

    #[test]
    fn an_unrecorded_category_stays_explicit_and_counts_stay_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "s.jsonl", &[json!({"type": "user"})]);

        let report = read(&claude_target(dir.path(), path)).unwrap();

        assert!(category(&report, "Instructions").note.is_some());
        assert!(category(&report, "Skills").note.is_some());
        assert!(category(&report, "MCP").items.is_empty());
        assert!(
            report
                .unsupported
                .iter()
                .any(|u| u.contains("MCP instructions were not recorded")),
            "{:?}",
            report.unsupported
        );
    }

    #[test]
    fn a_partial_skill_listing_reports_the_gap_instead_of_inventing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "s.jsonl",
            &[
                json!({"attachment": {"type": "skill_listing", "skillCount": 63,
                "names": ["voice"], "content": "- voice: Write in Yuval's voice."}}),
            ],
        );

        let report = read(&claude_target(dir.path(), path)).unwrap();

        assert_eq!(category(&report, "Skills").items.len(), 1);
        assert_eq!(
            category(&report, "Instructions").note.as_deref(),
            Some("This session recorded no instruction files.")
        );
    }

    #[test]
    fn codex_reports_its_own_records_and_declares_the_rest_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "r.jsonl",
            &[
                json!({"type": "session_meta", "payload": {
                    "base_instructions": {"text": "You are Codex"}}}),
                json!({"type": "world_state", "payload": {"state": {"agents_md": {
                    "directory": "/repo", "text": "repo rules"}}}}),
            ],
        );
        let target = Target {
            key: "row".into(),
            harness: "codex".into(),
            transcript: path,
            cwd: dir.path().to_path_buf(),
        };

        let report = read(&target).unwrap();

        let item = &category(&report, "Instructions").items[0];
        assert_eq!(item.source.as_deref(), Some("/repo/AGENTS.md"));
        assert_eq!(item.text.as_deref(), Some("repo rules"));
        assert_eq!(
            category(&report, "System prompt").items[0].text.as_deref(),
            Some("You are Codex")
        );
        assert!(category(&report, "Skills").note.is_some());
        assert!(category(&report, "MCP").note.is_some());
    }

    #[test]
    fn an_unsupported_harness_and_a_missing_file_are_errors_not_empty_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "s.jsonl", &[json!({})]);
        let mut target = claude_target(dir.path(), path.clone());
        target.harness = "opencode".into();
        assert!(read(&target).is_err());
        assert!(!supported("opencode"));
        assert!(supported("claude") && supported("codex"));

        let mut missing = claude_target(dir.path(), dir.path().join("gone.jsonl"));
        missing.harness = "claude".into();
        assert!(read(&missing).is_err());
    }

    #[test]
    fn a_response_carries_the_target_it_was_read_for() {
        let dir = tempfile::tempdir().unwrap();
        let first = claude_target(
            dir.path(),
            write(
                dir.path(),
                "a.jsonl",
                &[json!({"attachment": {"type": "instructions",
                    "files": [{"path": "/a/CLAUDE.md", "content": "a"}]}})],
            ),
        );
        let mut second = claude_target(
            dir.path(),
            write(
                dir.path(),
                "b.jsonl",
                &[json!({"attachment": {"type": "instructions",
                    "files": [{"path": "/b/CLAUDE.md", "content": "b"}]}})],
            ),
        );
        second.key = "other".into();

        let mut reader = Reader::new().unwrap();
        assert!(reader.request(first.clone()).unwrap());
        assert!(
            !reader.request(second.clone()).unwrap(),
            "one read at a time"
        );
        let response = loop {
            if let Some(response) = reader.poll() {
                break response;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(response.target, first);
        assert_eq!(
            response.result.unwrap().categories[0].items[0]
                .text
                .as_deref(),
            Some("a")
        );
        assert!(reader.poll().is_none());
        assert!(reader.request(second).unwrap());
    }
}
