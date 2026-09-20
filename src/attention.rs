//! Read markers over native reports. These never replace the harness's state.
use crate::{fleet::Session, ledger::Run};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub key: String,
    pub state: String,
    revision: String,
    #[serde(skip)]
    title: String,
}

impl Observation {
    pub(crate) fn session(session: &Session) -> Option<Self> {
        if session.harness == "terminal" || session.state == "started" {
            return None;
        }
        let source = session
            .transcript_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(Self {
            key: format!("{}:{}:{source}", session.harness, session.session_id),
            state: session.state.clone(),
            revision: session
                .last
                .as_ref()
                .map(|last| format!("{:x}", Sha256::digest(last.as_bytes())))
                .unwrap_or_default(),
            title: session
                .title
                .clone()
                .unwrap_or_else(|| session.harness.clone()),
        })
    }

    pub(crate) fn run(run: &Run) -> Self {
        Self {
            key: format!("run:{}", run.started.run_id),
            state: run.status(),
            revision: run
                .terminal
                .as_ref()
                .and_then(|t| t.ended_at)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            title: run.started.job.clone().unwrap_or_else(|| "run".into()),
        }
    }

    fn complete(&self) -> bool {
        matches!(
            self.state.as_str(),
            "idle" | "done" | "ok" | "failed" | "timeout" | "crashed"
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    observation: Observation,
    unread: bool,
    touched: i64,
}

#[derive(Default)]
pub(crate) struct Tracker {
    entries: HashMap<String, Entry>,
}

pub(crate) struct Notice {
    pub title: String,
    pub input: bool,
}

impl Tracker {
    /// Shared read markers survive restart. Merge while locked so two dashboards
    /// cannot erase each other's acknowledgements with whole-file snapshots.
    pub(crate) fn update(
        &mut self,
        state: &Path,
        observations: &[Observation],
        reviewed: &HashSet<String>,
    ) -> io::Result<Vec<Notice>> {
        crate::private_dir(state).map_err(io::Error::other)?;
        let lock = crate::private_file(&state.join("attention.lock")).map_err(io::Error::other)?;
        lock.lock_exclusive()?;
        let path = state.join("attention.json");
        self.entries = match fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err(io::Error::other("attention records exceed 2 MiB"));
                }
                serde_json::from_slice(&bytes).map_err(io::Error::other)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        let mut dirty = false;
        let mut notices = Vec::new();
        let now = chrono::Utc::now().timestamp();
        for observation in observations {
            if observation.state == "-" {
                continue;
            }
            match self.entries.get_mut(&observation.key) {
                Some(entry) => {
                    let changed = entry.observation.state != observation.state
                        || entry.observation.revision != observation.revision;
                    let completed = observation.complete()
                        && changed
                        && (matches!(
                            entry.observation.state.as_str(),
                            "active" | "started" | "blocked"
                        ) || (entry.observation.complete()
                            && !observation.revision.is_empty()
                            && entry.observation.revision != observation.revision));
                    let input =
                        observation.state == "blocked" && entry.observation.state != "blocked";
                    if completed {
                        entry.unread = true;
                    }
                    if matches!(
                        observation.state.as_str(),
                        "active" | "started" | "stopped" | "exited"
                    ) {
                        entry.unread = false;
                    }
                    if reviewed.contains(&observation.key) && entry.unread {
                        entry.unread = false;
                        dirty = true;
                    }
                    if (completed || input) && !reviewed.contains(&observation.key) {
                        notices.push(Notice {
                            title: observation.title.clone(),
                            input,
                        });
                    }
                    if changed {
                        entry.observation = observation.clone();
                        entry.touched = now;
                        dirty = true;
                    }
                }
                None => {
                    // An initial snapshot is a baseline, not hundreds of unseen
                    // historical completions. New input requests still filter in.
                    self.entries.insert(
                        observation.key.clone(),
                        Entry {
                            observation: observation.clone(),
                            unread: false,
                            touched: now,
                        },
                    );
                    dirty = true;
                }
            }
        }
        if self.entries.len() > 2048 {
            let current: HashSet<_> = observations.iter().map(|o| &o.key).collect();
            let mut older: Vec<_> = self
                .entries
                .iter()
                .filter(|(k, _)| !current.contains(k))
                .map(|(k, v)| (k.clone(), v.touched))
                .collect();
            older.sort_by_key(|(_, at)| *at);
            for (key, _) in older.into_iter().take(self.entries.len() - 2048) {
                self.entries.remove(&key);
            }
            dirty = true;
        }
        if dirty {
            let mut temporary = tempfile::NamedTempFile::new_in(state)?;
            serde_json::to_writer(&mut temporary, &self.entries).map_err(io::Error::other)?;
            temporary.flush()?;
            temporary.persist(path).map_err(io::Error::other)?;
        }
        Ok(notices)
    }

    pub(crate) fn unread(&self, observation: &Observation) -> bool {
        self.entries.get(&observation.key).is_some_and(|e| e.unread)
    }
}

/// Notification delivery cannot block drawing or change a read marker.
pub(crate) fn notify(notices: Vec<Notice>) {
    if notices.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for notice in notices.into_iter().take(8) {
            let title: String = notice
                .title
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect();
            let message = format!(
                "{title}: {}",
                if notice.input {
                    "needs input"
                } else {
                    "ready to review"
                }
            );
            let mut command = match std::env::var_os("CONES_NOTIFIER") {
                Some(program) => {
                    let mut c = Command::new(program);
                    c.args(["cones", &message]);
                    c
                }
                None => {
                    let mut c = Command::new("/usr/bin/osascript");
                    c.args([
                        "-e",
                        &format!(
                            "display notification {} with title \"cones\"",
                            serde_json::json!(message)
                        ),
                    ]);
                    c
                }
            };
            let Ok(mut child) = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(state: &str, revision: &str) -> Observation {
        Observation {
            key: "claude:session:home".into(),
            state: state.into(),
            revision: revision.into(),
            title: "work".into(),
        }
    }

    #[test]
    fn native_completion_is_unread_until_review_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::default();
        let active = observation("active", "");
        assert!(
            tracker
                .update(dir.path(), &[active], &HashSet::new())
                .unwrap()
                .is_empty()
        );
        let done = observation("done", "reply");
        let notices = tracker
            .update(dir.path(), std::slice::from_ref(&done), &HashSet::new())
            .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(!notices[0].input);
        assert!(tracker.unread(&done));
        let mut other = Tracker::default();
        assert!(
            other
                .update(dir.path(), std::slice::from_ref(&done), &HashSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(other.unread(&done));
        other
            .update(
                dir.path(),
                std::slice::from_ref(&done),
                &HashSet::from([done.key.clone()]),
            )
            .unwrap();
        tracker
            .update(dir.path(), std::slice::from_ref(&done), &HashSet::new())
            .unwrap();
        assert!(
            !tracker.unread(&done),
            "an older dashboard must not resurrect a reviewed result"
        );
    }

    #[test]
    fn baseline_unknown_reports_and_focus_do_not_invent_completions() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::default();
        let idle = observation("idle", "first");
        assert!(
            tracker
                .update(dir.path(), std::slice::from_ref(&idle), &HashSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(!tracker.unread(&idle));
        tracker.update(dir.path(), &[], &HashSet::new()).unwrap();
        assert!(
            tracker
                .update(dir.path(), std::slice::from_ref(&idle), &HashSet::new())
                .unwrap()
                .is_empty()
        );
        let done = observation("idle", "next");
        assert!(
            tracker
                .update(
                    dir.path(),
                    std::slice::from_ref(&done),
                    &HashSet::from([done.key.clone()])
                )
                .unwrap()
                .is_empty()
        );
        assert!(!tracker.unread(&done));
        let blocked = observation("blocked", "next");
        assert_eq!(
            tracker
                .update(dir.path(), std::slice::from_ref(&blocked), &HashSet::new())
                .unwrap()
                .len(),
            1
        );
        tracker
            .update(
                dir.path(),
                std::slice::from_ref(&blocked),
                &HashSet::from([blocked.key.clone()]),
            )
            .unwrap();
        assert_eq!(
            tracker.entries[&blocked.key].observation.state, "blocked",
            "review never answers a native question"
        );
    }

    #[test]
    fn identity_and_corrupt_records_are_not_silently_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::default();
        tracker
            .update(dir.path(), &[observation("active", "")], &HashSet::new())
            .unwrap();
        let mut foreign = observation("done", "reply");
        foreign.key = "claude:session:another-home".into();
        assert!(
            tracker
                .update(dir.path(), std::slice::from_ref(&foreign), &HashSet::new())
                .unwrap()
                .is_empty()
        );
        fs::write(dir.path().join("attention.json"), b"not json").unwrap();
        assert!(
            tracker
                .update(dir.path(), &[foreign], &HashSet::new())
                .is_err()
        );
        assert_eq!(
            fs::read(dir.path().join("attention.json")).unwrap(),
            b"not json"
        );
    }
}
