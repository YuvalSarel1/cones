//! One folder's coordination plumbing: the claim on it, the wake gate, the mail and the notes.
//!
//! Coordination itself is a skill the model reads; what lives here is the plumbing that skill
//! would otherwise re-implement in shell. Every piece of it is harness-neutral: the state sits
//! under cones' own directory rather than one harness's home, the sender's identity comes from
//! the roster rather than a single harness's registry, and an outgoing note goes through
//! whatever delivery command that worker's harness declares.
//!
//! The commands split by who may run them. Anyone may `send`. Consuming a folder — `mail` and
//! `wait` — belongs to one agent at a time, because acknowledgement is a single cursor and the
//! watcher keeps a single position: a second consumer either acts on a reply the first one owns
//! or moves the cursor past one it never saw.

use crate::fleet::{self, Session};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long the watcher sleeps between roster reads. Arrivals, mail and a watched worker's
/// condition are the only things it looks for, so a slower beat costs nothing but the delay
/// before a greeting goes out.
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

    /// Every session cones can see, wherever it is working. Discovery is not re-derived here:
    /// cones already decides who is a worker, resolves a Codex thread to its client and drops
    /// viewers and daemons.
    fn fleet(&self) -> Result<Vec<Session>> {
        let runs = crate::ledger::Ledger::new(&self.state)?.runs()?;
        crate::tui::fleet_rows(
            &self.claude,
            &self.state,
            &runs,
            &crate::config::defaults(&self.jobs),
        )
    }

    /// This folder and the worktrees under it, which is the same read `cones ls --dir` prints.
    pub fn roster(&self) -> Result<Vec<Session>> {
        Ok(self
            .fleet()?
            .into_iter()
            .filter(|s| fleet::contains(&self.path, &s.cwd))
            .collect())
    }

    fn inbox(&self) -> PathBuf {
        self.dir().join("inbox.jsonl")
    }

    /// Lines the folder's consumer has durably handled. Reading mail never moves this; only an
    /// explicit acknowledgement does, which is what lets a replacement see a reply the agent
    /// before it read but never acted on.
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

/// Hold this folder's records across a read-modify-write. Reading a record and then replacing
/// it is not mutual exclusion: two agents both read the folder as free, both write, and both
/// believe they hold it. Every writer of `status.json` and `watcher.json` takes this first.
fn hold(dir: &Path) -> Result<File> {
    crate::private_dir(dir)?;
    let f = crate::private_file(&dir.join("folder.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}

/// The live record for one folder, or none. A record whose process has gone is not live: the
/// coordinator that wrote it cannot release a folder it crashed out of.
pub fn status(state: &Path, folder: &Path) -> Option<Value> {
    let record: Value =
        serde_json::from_slice(&fs::read(directory(state, folder).join("status.json")).ok()?)
            .ok()?;
    fleet::alive(record["pid"].as_u64()? as u32).then_some(record)
}

/// Every coordinator claim, by the session and folder it named. Rows are marked from this and
/// never from a title, so a session that merely mentions the word is not mistaken for the role.
/// The session is the identity, not the process: `claude --bg` re-hosts a conversation under a
/// new pid while it works, so a pid read minutes ago names nothing. A claim whose session the
/// harness no longer reports marks no row, which is the only liveness test the mark needs.
pub fn claims(state: &Path) -> HashSet<(String, PathBuf)> {
    fs::read_dir(state.join("coordinator/folders"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let record: Value =
                serde_json::from_slice(&fs::read(entry.path().join("status.json")).ok()?).ok()?;
            let session = record["session"].as_str()?.to_owned();
            let cwd = PathBuf::from(record["cwd"].as_str()?);
            Some((session, cwd))
        })
        .collect()
}

/// Which roster row is running this command, found by walking the process chain until one of
/// the pids is a session cones can see. That works whatever harness the caller runs in,
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
    let _hold = hold(&dir)?;
    let record = dir.join("status.json");
    if release {
        // Only the holder hands the folder back. A coordinator that finds a foreign record has
        // already been told to stand down, and clearing it would take the folder from a peer.
        // The holder is the session named in the record, not the process that wrote it: a
        // re-hosted `claude --bg` claim would otherwise read as nobody's and be cleared by the
        // first peer to release a folder it never held.
        let rows = folder.roster()?;
        if let Some(held) = foreign_holder(&record, &rows) {
            bail!(
                "{} is held by session {held}, not by you",
                folder.path.display()
            );
        }
        fs::remove_file(&record).ok();
        return Ok(format!("released {}", folder.path.display()));
    }
    let rows = folder.roster()?;
    let me = own_session(&rows).context(
        "no agent session in this process's parent chain; the coordinator runs inside one",
    )?;
    if let Some(held) = foreign_holder(&record, &rows) {
        bail!(
            "another coordinator owns {} (session {held}); report that and stop",
            folder.path.display()
        );
    }
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
    // A replacement does not inherit the position its predecessor announced from. A reply that
    // one read and never acknowledged is still unhandled, and leaving the watcher above it
    // would step straight over the reply this claim exists to pick up.
    let seen_path = dir.join("wait.json");
    if let Ok(mut seen) = read_json::<Seen>(&seen_path) {
        let acked = folder.acknowledged(lines(&folder.inbox()).len());
        if seen.inbox > acked {
            seen.inbox = acked;
            write_atomically(&seen_path, &serde_json::to_string(&seen)?)?;
            note.push_str(&format!(
                "\nunhandled mail from before this claim is pending again from line {}",
                acked + 1
            ));
        }
    }
    Ok(format!(
        "session={} harness={} dir={}{note}",
        me.session_id,
        me.harness,
        dir.display()
    ))
}

/// Point the holder's record at the process running it now. A `claude --bg` conversation is
/// re-hosted under a new pid while it works, so a record written at claim time names a process
/// that has gone: the folder would read as free, and the row would lose its mark, while its
/// coordinator is mid-beat. The holder rewrites it as it ticks; nobody else may.
fn refresh(folder: &Folder, rows: &[Session]) {
    let record = folder.dir().join("status.json");
    let Some(me) = own_session(rows) else {
        return;
    };
    let Ok(mut live) = read_json::<Value>(&record) else {
        return;
    };
    if live["session"].as_str() != Some(me.session_id.as_str())
        || live["pid"].as_u64() == me.pid.map(u64::from)
    {
        return;
    }
    let Ok(_hold) = hold(&folder.dir()) else {
        return;
    };
    live["pid"] = json!(me.pid);
    let _ = write_atomically(&record, &live.to_string());
}

/// The session holding this folder, when it is not the caller's own and the harness still
/// reports it. A record naming a session no agent is running is a leftover: the coordinator
/// that wrote it cannot hand back a folder it crashed out of, so the next caller may clear it.
fn foreign_holder(record: &Path, rows: &[Session]) -> Option<String> {
    let live = read_json::<Value>(record).ok()?;
    let held = live["session"].as_str()?;
    if own_session(rows).is_some_and(|me| me.session_id == held) || own_process(&live) {
        return None;
    }
    // Either reading proves the holder is still there: the harness reports its session, or the
    // process it recorded is running. A background session re-hosted since it claimed fails the
    // second and passes the first, which is the whole point of naming the session.
    let running = rows.iter().any(|s| s.session_id == held)
        || live["pid"]
            .as_u64()
            .is_some_and(|pid| fleet::alive(pid as u32));
    running.then(|| held.to_owned())
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

/// A folder has one consumer of its inbox and its watcher, and that is whoever holds its claim.
/// Anyone may send into a folder; two agents reading out of one split a single acknowledgement
/// cursor between them, so the one that loses a line never learns the line existed.
fn consumer(folder: &Folder) -> Result<()> {
    let Some(live) = status(&folder.state, &folder.path) else {
        return Ok(());
    };
    ensure!(
        own_process(&live),
        "{} is coordinated by pid {} (session {}); one agent consumes a folder's inbox and \
         watcher, so report through that agent or use a separate task folder",
        folder.path.display(),
        live["pid"],
        live["session"].as_str().unwrap_or("-")
    );
    Ok(())
}

/// A watch refused because one is already armed. This is the one refusal worth retrying: a
/// caller that re-arms the instant its own wait returns can race its predecessor out of the
/// folder. A refusal from the folder's claim is not retryable, which is why they are separate
/// errors and separate exit codes.
#[derive(Debug)]
pub struct Armed(pub u32);

impl std::fmt::Display for Armed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pid {} is already waiting here; a folder has one watcher, so retry once it \
             returns, wait through that agent, or use a separate task folder",
            self.0
        )
    }
}

impl std::error::Error for Armed {}

/// One armed watcher per folder, released when it returns. Two `wait` calls on one folder split
/// its wakeups: each records what it showed, so whichever loses the race for a line never learns
/// that line existed. A lease whose process has gone is free to take.
struct Watcher(PathBuf);

impl Watcher {
    fn arm(dir: &Path) -> Result<Self> {
        let _hold = hold(dir)?;
        let path = dir.join("watcher.json");
        if let Some(pid) = read_json::<Value>(&path)
            .ok()
            .and_then(|v| v["pid"].as_u64())
            .map(|p| p as u32)
            .filter(|p| *p != std::process::id() && fleet::alive(*p))
        {
            return Err(Armed(pid).into());
        }
        write_atomically(&path, &json!({"pid": std::process::id()}).to_string())?;
        Ok(Self(path))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Clear the lease only while it is still this process's: a watcher that was killed can
        // be replaced by the next one before this value is dropped.
        if read_json::<Value>(&self.0)
            .ok()
            .and_then(|v| v["pid"].as_u64())
            == Some(u64::from(std::process::id()))
        {
            fs::remove_file(&self.0).ok();
        }
    }
}

/// What the watcher has already put in front of the model. Kept beside the inbox rather than in
/// the job that armed it, so a restarted coordinator does not re-announce a worker it knows.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Seen {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    inbox: usize,
    /// The last condition reported for each watched worker, so a worker that is still waiting
    /// for input is not announced every pass, and one that blocks again after recovering is.
    #[serde(default)]
    workers: BTreeMap<String, String>,
}

/// Block until something happens that is worth a model call, and print what it was.
///
/// With no `ids`, exactly two things qualify: a worker arrived and has not been shown, and a
/// worker wrote. A departure, a state moving between active, idle and blocked, and an edit to
/// the tree are facts to read from a tick once the coordinator is already awake. Waking for them
/// spends a call to learn that somebody else is still working, and a session going idle and
/// active again is not news.
///
/// With `ids`, the watch narrows to those workers, which is what a dispatcher that launched a
/// known set wants: arrivals are somebody else's business, and what matters is a worker that
/// stopped making progress on its own. Mail still wakes it, because a reply is how a worker
/// reports. This is the only place either rule is enforced, so there is no loop condition in a
/// prompt to widen.
pub fn wait(folder: &Folder, ids: &[String], timeout: Option<Duration>) -> Result<Option<String>> {
    consumer(folder)?;
    let dir = folder.dir();
    // The lease is this value's, so it is released as this function returns, before the caller
    // can print or observe anything. A caller that re-arms on the same line as its wake finds
    // the folder free; one that races its own predecessor gets `Armed`, which it may retry.
    let _watcher = Watcher::arm(&dir)?;
    let seen_path = dir.join("wait.json");
    let mut seen: Seen = read_json(&seen_path).unwrap_or_default();
    // A watch names workers the caller launched, so an id cones has never seen is a typo. One
    // it has watched before is not: a worker leaving is the disappearance this mode reports.
    if !ids.is_empty() {
        let roster = folder.roster()?;
        for id in ids {
            ensure!(
                roster.iter().any(|s| s.session_id == *id) || seen.workers.contains_key(id),
                "{id} is not on this folder's roster"
            );
        }
    }
    let deadline = timeout.map(|t| Instant::now() + t);
    // A roster read can fail transiently while a harness rewrites a registry. Retrying keeps the
    // watcher armed; an hour of failures is a broken install and belongs in front of the model.
    let mut failures = 0;
    let mut first = !seen_path.exists();
    loop {
        match folder.roster() {
            Ok(rows) => {
                failures = 0;
                let waiting = lines(&folder.inbox());
                // Mail is not suppressed on the first arm. A reply that landed between the
                // coordinator's read and this call is unacknowledged, which is the record of
                // nobody having acted on it, and a watcher that swallowed it would never wake.
                let mail = waiting.len() > seen.inbox.max(folder.acknowledged(waiting.len()));
                let mut out = String::new();
                if ids.is_empty() {
                    let known: HashSet<&str> = seen.ids.iter().map(String::as_str).collect();
                    // The first arm has nothing recorded, so every session present looks new. It
                    // is not: the coordinator read the folder before arming.
                    if !first {
                        for row in rows
                            .iter()
                            .filter(|s| !known.contains(s.session_id.as_str()))
                        {
                            out.push_str(&format!("new: {}\n", summary(row)));
                        }
                    }
                    seen.ids = rows.iter().map(|s| s.session_id.clone()).collect();
                } else {
                    out.push_str(&watched(ids, &rows, &mut seen.workers));
                }
                if mail {
                    out.push_str(&unread(folder, &waiting));
                }
                seen.inbox = waiting.len();
                write_atomically(&seen_path, &serde_json::to_string(&seen)?)?;
                first = false;
                if !out.is_empty() {
                    return Ok(Some(out));
                }
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

/// Why a watched worker is worth looking at. None of these is a finished task: a worker that
/// asks for input, reports a native failure or leaves the roster has stopped moving on its own,
/// and what the assignment came to is the worker's own report. Each condition is announced once
/// and again only after the worker has been out of it.
fn watched(ids: &[String], rows: &[Session], reported: &mut BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for id in ids {
        let (condition, why) = match rows.iter().find(|s| s.session_id == *id) {
            None => (
                "gone",
                Some("left the roster, so cones can no longer observe it"),
            ),
            Some(row) => match row.state.as_str() {
                "blocked" => (
                    "blocked",
                    Some("is asking for input natively; answer it in its own session"),
                ),
                "failed" | "crashed" | "error" => (
                    "failed",
                    Some("reported a native failure; read its session"),
                ),
                other => (other, None),
            },
        };
        if reported.get(id).map(String::as_str) == Some(condition) {
            continue;
        }
        reported.insert(id.clone(), condition.to_owned());
        if let Some(why) = why {
            out.push_str(&format!(
                "worker: {id}\t{condition}\t{why}. This is not a completed task.\n"
            ));
        }
    }
    out
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
         Acknowledge with `cones comms mail --ack N` once you have acted on it.\n",
    );
    for (n, line) in waiting.iter().enumerate().skip(acked) {
        out.push_str(&format!("{}\t{line}\n", n + 1));
    }
    out
}

/// Pending mail, or the line that says there is none. Separate from `mail` so a `tick` can print
/// it without taking the consumer check twice.
fn unread(folder: &Folder, waiting: &[String]) -> String {
    match folder.acknowledged(waiting.len()) < waiting.len() {
        true => pending(folder, waiting),
        false => "mail: none pending\n".into(),
    }
}

pub fn mail(folder: &Folder, ack: Option<usize>) -> Result<String> {
    consumer(folder)?;
    let waiting = lines(&folder.inbox());
    let acked = folder.acknowledged(waiting.len());
    let Some(through) = ack else {
        return Ok(unread(folder, &waiting));
    };
    ensure!(
        acked < through && through <= waiting.len(),
        "acknowledge a line between {} and {}",
        acked + 1,
        waiting.len()
    );
    let dir = folder.dir();
    crate::private_dir(&dir)?;
    write_atomically(&dir.join("inbox.ack"), &through.to_string())?;
    Ok(format!("acknowledged through line {through}\n"))
}

/// One note to a live worker in this folder, delivered by that worker's own harness.
///
/// The sender has no authority the owner did not give it, so the note says who it is from and
/// carries the folder's inbox as the way back. A harness with no delivery command of its own is
/// refused rather than approximated: typing into somebody's terminal is not a message.
pub fn send(folder: &Folder, id: &str, text: &str, greet: bool) -> Result<String> {
    let fleet_rows = folder.fleet()?;
    let row = fleet_rows
        .iter()
        .find(|s| s.session_id == id && fleet::contains(&folder.path, &s.cwd))
        .with_context(|| format!("{id} is not on this folder's roster"))?;
    let dir = folder.dir();
    crate::private_dir(&dir)?;
    let greeted_path = dir.join("greeted.json");
    let mut greeted: Vec<String> = read_json(&greeted_path).unwrap_or_default();
    // A greeting registers the session and outlives every task in it, so repeating one is a
    // no-op rather than a second interruption.
    if greet && greeted.iter().any(|g| g == id) {
        return Ok(format!("{id} was already greeted\n"));
    }
    let note = format!(
        "[{}] {text}\n\
         Reply by appending one JSON line to {}: {}",
        sender(folder, &fleet_rows, greet),
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

/// How the note introduces its sender. Only the folder's claim holder may call itself the
/// coordinator; any other agent names the session it is, and one cones cannot place says only
/// that it is not the owner. A recipient that cannot tell a peer from the coordinator cannot
/// weigh what it just read, and every one of these is peer input either way.
fn sender(folder: &Folder, rows: &[Session], greet: bool) -> String {
    let who = match status(&folder.state, &folder.path) {
        Some(live) if own_process(&live) => "coordinator, not the owner".to_owned(),
        _ => match own_session(rows) {
            Some(me) => format!(
                "{} session {}, a peer agent, not the owner",
                me.harness, me.session_id
            ),
            None => "another agent, not the owner".to_owned(),
        },
    };
    match greet {
        true => format!("{who}; session introduction"),
        false => who,
    }
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
    refresh(folder, &rows);
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
    out.push_str(&unread(folder, &waiting));
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: &str, pid: Option<u32>) -> Session {
        serde_json::from_value(json!({
            "session_id": id, "harness": "claude", "cwd": "/project",
            "state": state, "pid": pid
        }))
        .unwrap()
    }

    fn folder(state: &Path) -> Folder {
        Folder {
            state: state.to_path_buf(),
            claude: state.join("claude"),
            jobs: state.join("jobs.yaml"),
            path: PathBuf::from("/project"),
        }
    }

    /// A note says who it is really from. Only the folder's claim holder may call itself the
    /// coordinator, because a worker weighs a note by who sent it, and every other agent's note
    /// would otherwise arrive carrying a role the owner never gave it.
    #[test]
    fn only_the_claim_holder_signs_a_note_as_the_coordinator() {
        let state = tempfile::tempdir().unwrap();
        let f = folder(state.path());
        let me = [row("mine", "idle", Some(std::process::id()))];
        assert_eq!(
            sender(&f, &me, false),
            "claude session mine, a peer agent, not the owner"
        );
        assert_eq!(sender(&f, &[], false), "another agent, not the owner");
        assert_eq!(
            sender(&f, &[], true),
            "another agent, not the owner; session introduction"
        );

        let record = f.dir().join("status.json");
        // launchd is pid 1 on macOS: alive, and certainly not in this process's parent chain.
        write_atomically(&record, &json!({"pid": 1, "session": "peer"}).to_string()).unwrap();
        assert_eq!(
            sender(&f, &me, false),
            "claude session mine, a peer agent, not the owner",
            "a live foreign claim does not make this agent the coordinator"
        );

        write_atomically(
            &record,
            &json!({"pid": std::process::id(), "session": "mine"}).to_string(),
        )
        .unwrap();
        assert_eq!(sender(&f, &me, false), "coordinator, not the owner");
        assert_eq!(
            sender(&f, &me, true),
            "coordinator, not the owner; session introduction"
        );
    }

    /// The watched-worker rule. A worker that asks for input, fails natively or leaves is worth
    /// one look, not one every ten seconds; and it is worth another look only after it has been
    /// out of that condition. None of it means the assignment finished.
    #[test]
    fn a_watched_worker_reports_each_condition_once_and_again_after_it_clears() {
        let ids = ["a".to_owned(), "b".to_owned()];
        let mut reported = BTreeMap::new();
        let working = [row("a", "active", None), row("b", "active", None)];
        assert_eq!(watched(&ids, &working, &mut reported), "");

        let blocked = [row("a", "blocked", None), row("b", "active", None)];
        let out = watched(&ids, &blocked, &mut reported);
        assert!(out.starts_with("worker: a\tblocked\t"), "{out}");
        assert!(out.contains("not a completed task"), "{out}");
        assert_eq!(out.lines().count(), 1, "only the worker that moved: {out}");
        assert_eq!(watched(&ids, &blocked, &mut reported), "", "reported once");

        // Idle, done and stopped are states to read from a tick, not reasons to wake.
        for quiet in ["idle", "done", "stopped"] {
            assert_eq!(
                watched(
                    &ids,
                    &[row("a", quiet, None), row("b", quiet, None)],
                    &mut reported
                ),
                "",
                "{quiet} is not a reason to inspect a worker"
            );
        }
        let out = watched(&ids, &blocked, &mut reported);
        assert!(
            out.starts_with("worker: a\tblocked\t"),
            "blocking again after recovering is news: {out}"
        );

        let out = watched(&ids, &[row("b", "failed", None)], &mut reported);
        assert!(out.contains("worker: a\tgone\t"), "{out}");
        assert!(out.contains("worker: b\tfailed\t"), "{out}");
        assert_eq!(
            watched(&ids, &[row("b", "failed", None)], &mut reported),
            ""
        );
    }
}
