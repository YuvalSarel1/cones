//! The folder's coordinator: its claim on the folder, its wake gate, its mail and its notes.
//!
//! Coordination itself is a skill the model reads; what lives here is the plumbing that skill
//! would otherwise re-implement in shell. Every piece of it is harness-neutral: the state sits
//! under cones' own directory rather than one harness's home, the coordinator's identity comes
//! from the roster rather than a single harness's registry, and an outgoing note goes through
//! whatever delivery command that worker's harness declares.

use crate::fleet::{self, Session};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long the watcher sleeps between roster reads. Arrivals and mail are the only two things
/// it looks for, so a slower beat costs nothing but the delay before a greeting goes out.
const BEAT: Duration = Duration::from_secs(10);

/// One coordinated folder: where its state lives and how its roster is read.
pub struct Folder {
    pub state: PathBuf,
    pub claude: PathBuf,
    pub jobs: PathBuf,
    /// The launch folder, canonicalized. Worktrees under it belong to it.
    pub path: PathBuf,
}

impl Folder {
    /// The folder's own directory: the record, the inbox and the watcher's position. It hangs
    /// off the state directory because any harness can hold the role, so it cannot live in the
    /// home of the one harness that happens to be coordinating today.
    pub fn dir(&self) -> PathBuf {
        directory(&self.state, &self.path)
    }

    /// Everything cones sees in this folder and the worktrees under it, which is the same read
    /// `cones ls --dir` prints. Discovery is not re-derived here: cones already decides who is
    /// a worker, resolves a Codex thread to its client and drops viewers and daemons.
    pub fn roster(&self) -> Result<Vec<Session>> {
        let runs = crate::ledger::Ledger::new(&self.state)?.runs()?;
        let rows = crate::tui::fleet_rows(
            &self.claude,
            &self.state,
            &runs,
            &crate::config::defaults(&self.jobs),
        )?;
        Ok(rows
            .into_iter()
            .filter(|s| fleet::contains(&self.path, &s.cwd))
            .collect())
    }

    fn inbox(&self) -> PathBuf {
        self.dir().join("inbox.jsonl")
    }

    /// Lines the coordinator has durably handled. Reading mail never moves this; only an
    /// explicit acknowledgement does, which is what lets a replaced coordinator see a reply
    /// the one before it read but never acted on.
    fn acknowledged(&self, total: usize) -> usize {
        read_number(&self.dir().join("inbox.ack")).min(total)
    }
}

pub fn directory(state: &Path, folder: &Path) -> PathBuf {
    let digest = Sha256::digest(folder.to_string_lossy().as_bytes());
    state
        .join("coordinator/folders")
        .join(format!("{digest:x}"))
}

/// The live record for one folder, or none. A record whose process has gone is not live: the
/// coordinator that wrote it cannot release a folder it crashed out of.
pub fn status(state: &Path, folder: &Path) -> Option<Value> {
    let record: Value =
        serde_json::from_slice(&fs::read(directory(state, folder).join("status.json")).ok()?)
            .ok()?;
    fleet::alive(record["pid"].as_u64()? as u32).then_some(record)
}

/// Every live coordinator, by the process and folder it claimed. Rows are marked from this and
/// never from a title, so a session that merely mentions the word is not mistaken for the role.
pub fn claims(state: &Path) -> HashSet<(u32, PathBuf)> {
    fs::read_dir(state.join("coordinator/folders"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let record: Value =
                serde_json::from_slice(&fs::read(entry.path().join("status.json")).ok()?).ok()?;
            let pid = record["pid"].as_u64()? as u32;
            let cwd = PathBuf::from(record["cwd"].as_str()?);
            fleet::alive(pid).then_some((pid, cwd))
        })
        .collect()
}

/// Which roster row is running this command, found by walking the process chain until one of
/// the pids is a session cones can see. That works whatever harness the coordinator runs in,
/// where reading one harness's registry would only ever find that harness's sessions.
fn own_session(rows: &[Session]) -> Option<&Session> {
    let mut pid = Some(std::process::id());
    let mut seen = HashSet::new();
    while let Some(current) = pid.filter(|p| *p > 1 && seen.insert(*p)) {
        if let Some(row) = rows.iter().find(|s| s.pid == Some(current)) {
            return Some(row);
        }
        pid = crate::process_info::parent(current);
    }
    None
}

pub fn claim(folder: &Folder, release: bool) -> Result<String> {
    let dir = folder.dir();
    let record = dir.join("status.json");
    if release {
        // Only the holder hands the folder back. A coordinator that finds a foreign record has
        // already been told to stand down, and clearing it would take the folder from a peer.
        let live = status(&folder.state, &folder.path);
        if let Some(live) = &live
            && live["pid"].as_u64() != Some(u64::from(std::process::id()))
            && !own_process(live)
        {
            bail!(
                "{} is held by pid {}, not by you",
                folder.path.display(),
                live["pid"]
            );
        }
        fs::remove_file(&record).ok();
        return Ok(format!("released {}", folder.path.display()));
    }
    let rows = folder.roster()?;
    let me = own_session(&rows).context(
        "no agent session in this process's parent chain; the coordinator runs inside one",
    )?;
    if let Some(live) = status(&folder.state, &folder.path)
        && !own_process(&live)
    {
        bail!(
            "another coordinator owns {} (pid {}); report that and stop",
            folder.path.display(),
            live["pid"]
        );
    }
    fs::create_dir_all(&dir)?;
    write_atomically(
        &record,
        &json!({
            "cwd": folder.path, "pid": me.pid, "session": me.session_id, "harness": me.harness,
        })
        .to_string(),
    )?;
    // Mail already in the folder when this coordinator arrives is history, not a backlog it was
    // asked to answer. Record it as handled once, here, rather than replaying it as new.
    let ack = dir.join("inbox.ack");
    let mut note = String::new();
    if !ack.exists() {
        let waiting = lines(&folder.inbox()).len();
        write_atomically(&ack, &waiting.to_string())?;
        if waiting > 0 {
            note = format!("\n{waiting} inbox entries predate this claim and are not replayed");
        }
    }
    Ok(format!(
        "session={} harness={} dir={}{note}",
        me.session_id,
        me.harness,
        dir.display()
    ))
}

/// A record this very session wrote, including one written by an earlier run of it. Matching on
/// the pid alone is enough: the pid in the record is the agent session's, not this command's.
fn own_process(record: &Value) -> bool {
    let Some(pid) = record["pid"].as_u64().map(|p| p as u32) else {
        return false;
    };
    let mut current = Some(std::process::id());
    let mut seen = HashSet::new();
    while let Some(p) = current.filter(|p| *p > 1 && seen.insert(*p)) {
        if p == pid {
            return true;
        }
        current = crate::process_info::parent(p);
    }
    false
}

/// What the watcher has already put in front of the model. Kept beside the inbox rather than in
/// the job that armed it, so a restarted coordinator does not re-announce a worker it knows.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Seen {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    inbox: usize,
}

/// Block until something happens that is worth a model call, and print what it was.
///
/// Exactly two things qualify: a worker arrived and has not been shown, and a worker wrote. A
/// departure, a state moving between active, idle and blocked, and an edit to the tree are facts
/// to read from a tick once the coordinator is already awake. Waking for them spends a call to
/// learn that somebody else is still working, and a session going idle and active again is not
/// news. This is the only place that rule is enforced, so there is no loop condition to widen.
pub fn wait(folder: &Folder, timeout: Option<Duration>) -> Result<Option<String>> {
    let dir = folder.dir();
    fs::create_dir_all(&dir)?;
    let seen_path = dir.join("wait.json");
    let mut seen: Seen = fs::read(&seen_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let deadline = timeout.map(|t| Instant::now() + t);
    // A roster read can fail transiently while a harness rewrites a registry. Retrying keeps the
    // watcher armed; an hour of failures is a broken install and belongs in front of the model.
    let mut failures = 0;
    let mut first = !seen_path.exists();
    loop {
        match folder.roster() {
            Ok(rows) => {
                failures = 0;
                let known: HashSet<&str> = seen.ids.iter().map(String::as_str).collect();
                let arrivals: Vec<&Session> = rows
                    .iter()
                    .filter(|s| !known.contains(s.session_id.as_str()))
                    .collect();
                let waiting = lines(&folder.inbox());
                let mail = waiting.len() > seen.inbox.max(folder.acknowledged(waiting.len()));
                // The first arm has nothing recorded, so everything present looks new. It is
                // not: the coordinator read the folder before arming. Snapshot and sleep.
                if !first && (!arrivals.is_empty() || mail) {
                    let mut out = String::new();
                    for row in &arrivals {
                        out.push_str(&format!("new: {}\n", summary(row)));
                    }
                    if mail {
                        out.push_str(&pending(folder, &waiting));
                    }
                    seen.ids = rows.iter().map(|s| s.session_id.clone()).collect();
                    seen.inbox = waiting.len();
                    write_atomically(&seen_path, &serde_json::to_string(&seen)?)?;
                    return Ok(Some(out));
                }
                seen.ids = rows.iter().map(|s| s.session_id.clone()).collect();
                seen.inbox = waiting.len();
                write_atomically(&seen_path, &serde_json::to_string(&seen)?)?;
                first = false;
            }
            Err(e) => {
                failures += 1;
                if failures > 360 {
                    return Err(e.context("the roster has been unreadable for an hour"));
                }
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(None);
        }
        let nap = match deadline {
            Some(d) => BEAT.min(d.saturating_duration_since(Instant::now())),
            None => BEAT,
        };
        std::thread::sleep(nap);
    }
}

fn summary(row: &Session) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}",
        row.session_id,
        row.harness,
        row.state,
        fleet::tilde(&row.cwd),
        row.title.as_deref().unwrap_or("-")
    )
}

fn pending(folder: &Folder, waiting: &[String]) -> String {
    let acked = folder.acknowledged(waiting.len());
    let mut out = String::from(
        "mail: unacknowledged, still pending after you read it. \
         Acknowledge with `cones coordinator mail --ack N` once you have acted on it.\n",
    );
    for (n, line) in waiting.iter().enumerate().skip(acked) {
        out.push_str(&format!("{}\t{line}\n", n + 1));
    }
    out
}

pub fn mail(folder: &Folder, ack: Option<usize>) -> Result<String> {
    let waiting = lines(&folder.inbox());
    let acked = folder.acknowledged(waiting.len());
    let Some(through) = ack else {
        return Ok(match acked < waiting.len() {
            true => pending(folder, &waiting),
            false => "mail: none pending\n".into(),
        });
    };
    ensure!(
        acked < through && through <= waiting.len(),
        "acknowledge a line between {} and {}",
        acked + 1,
        waiting.len()
    );
    let dir = folder.dir();
    fs::create_dir_all(&dir)?;
    write_atomically(&dir.join("inbox.ack"), &through.to_string())?;
    Ok(format!("acknowledged through line {through}\n"))
}

/// One note to a live worker, delivered by that worker's own harness.
///
/// The coordinator has no authority the owner did not give it, so the note says who it is from
/// and carries the folder's inbox as the way back. A harness with no delivery command of its own
/// is refused rather than approximated: typing into somebody's terminal is not a message.
pub fn send(folder: &Folder, id: &str, text: &str, greet: bool) -> Result<String> {
    let rows = folder.roster()?;
    let row = rows
        .iter()
        .find(|s| s.session_id == id)
        .with_context(|| format!("{id} is not on this folder's roster"))?;
    let dir = folder.dir();
    fs::create_dir_all(&dir)?;
    let greeted_path = dir.join("greeted.json");
    let mut greeted: Vec<String> = fs::read(&greeted_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // A greeting registers the session and outlives every task in it, so repeating one is a
    // no-op rather than a second interruption.
    if greet && greeted.iter().any(|g| g == id) {
        return Ok(format!("{id} was already greeted\n"));
    }
    let note = format!(
        "[orchestrator, not the owner{}] {text}\n\
         Reply by appending one JSON line to {}: {}",
        match greet {
            true => "; session introduction",
            false => "",
        },
        folder.inbox().display(),
        json!({"from": format!("{}:{id}", row.harness), "text": "<reply>"}),
    );
    let home = crate::harness::home_of(row, &folder.claude);
    let mut command = crate::harness::message(row, &home, &note)?;
    let out = command
        .output()
        .with_context(|| format!("delivering to {id}"))?;
    ensure!(
        out.status.success(),
        "{} refused the note: {}",
        row.harness,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    if greet {
        greeted.push(id.to_owned());
        write_atomically(&greeted_path, &serde_json::to_string(&greeted)?)?;
    }
    Ok(format!("sent to {id} ({})\n", row.harness))
}

/// Everything the coordinator reads before it acts, in one tool result: what the tree is doing,
/// who is here, what each worker's window costs, and what is still unanswered.
pub fn tick(folder: &Folder) -> Result<String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&folder.path)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_owned())
    };
    let rows = folder.roster()?;
    let waiting = lines(&folder.inbox());
    let held = own_session(&rows).map(|s| s.session_id.clone());
    let mut out = format!(
        "self={} head={} unacknowledged_mail={}\n",
        held.as_deref().unwrap_or("-"),
        git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "-".into()),
        waiting.len() - folder.acknowledged(waiting.len()),
    );
    out.push_str("--- tree\n");
    let tree = git(&["status", "--short"]).unwrap_or_default();
    out.push_str(match tree.is_empty() {
        true => "clean",
        false => &tree,
    });
    // A window near its end is a handoff rather than another note, and a window the harness never
    // reported is unknown, which is not the same as room to spare.
    out.push_str("\n--- roster (id  harness  state  context  cost  folder  title)\n");
    if rows.is_empty() {
        out.push_str("empty\n");
    }
    for row in &rows {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.session_id,
            row.harness,
            row.state,
            window(row),
            crate::cost::display(row.cost_usd, row.cost_info.as_ref()),
            fleet::tilde(&row.cwd),
            row.title.as_deref().unwrap_or("-")
        ));
    }
    out.push_str("--- mail\n");
    out.push_str(&mail(folder, None)?);
    Ok(out)
}

fn window(row: &Session) -> String {
    match (row.context_tokens, row.context_window) {
        (Some(used), Some(size)) if size > 0 => format!("{}%", used * 100 / size),
        _ => "unknown".into(),
    }
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn read_number(path: &Path) -> usize {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Replace a file through a temporary of this process's own, never a shared name: a replacement
/// coordinator overlapping the one it takes over from writes the same records, and one shared
/// temporary means the second writer's rename destroys the first writer's source.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("state path has no directory")?;
    fs::create_dir_all(dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}
