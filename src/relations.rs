//! Which agent came from which: a fork, recorded once the native fork reports its new
//! session identity, and a launch, recorded when `cones launch` runs inside a listed session.
//! Both name each end by harness, native home and the harness's own id, the identity that
//! outlives the process. A session still listed by its process carries its pid as well.
use crate::{fleet::Session, harness};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The child continues the parent's conversation, in the parent's folder.
    #[default]
    Fork,
    /// The parent started the child with `cones launch`, in any folder.
    Launch,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Link {
    #[serde(default)]
    pub kind: Kind,
    /// The child's harness and native home.
    pub harness: String,
    pub home: PathBuf,
    /// The child's folder, which a fork shares with its parent.
    pub cwd: PathBuf,
    pub parent: String,
    /// Empty for a launch whose harness had not named it yet; `child_pid` names it then.
    pub child: String,
    /// Set when the parent ran in another harness: a fork seeded from its transcript, or a
    /// launch from an agent of another harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_harness: Option<String>,
    /// A launcher's native home. A fork's parent shares the child's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<u32>,
}

impl Link {
    fn parent_harness(&self) -> &str {
        self.parent_harness.as_deref().unwrap_or(&self.harness)
    }
}

const FILE: &str = "relations.json";
/// Where forks were kept before launches joined them, read until the first write.
const FORKS: &str = "forks.json";
/// The launches a list can still show. Older ones name sessions long gone.
const LAUNCHES: usize = 500;

pub fn read(state: &Path) -> Result<Vec<Link>> {
    load(state).context("reading agent relationships")
}

pub fn record(state: &Path, link: Link) -> Result<()> {
    match link.kind {
        Kind::Fork => ensure!(
            !link.parent.is_empty() && !link.child.is_empty() && link.parent != link.child,
            "a fork must have a distinct native session id"
        ),
        Kind::Launch => ensure!(
            !link.parent.is_empty() && (!link.child.is_empty() || link.child_pid.is_some()),
            "a launch must name both sessions"
        ),
    }
    crate::private_dir(state)?;
    let lock = crate::private_file(&state.join("relations.lock"))?;
    lock.lock_exclusive()?;
    let mut links = load(state)?;
    links.retain(|old| {
        !(old.kind == link.kind
            && old.harness == link.harness
            && old.home == link.home
            && old.child == link.child
            && old.child_pid == link.child_pid)
    });
    links.push(link);
    let launches = links.iter().filter(|l| l.kind == Kind::Launch).count();
    let mut excess = launches.saturating_sub(LAUNCHES);
    links.retain(|l| {
        let drop = excess > 0 && l.kind == Kind::Launch;
        excess -= usize::from(drop);
        !drop
    });
    let bytes = serde_json::to_vec(&links)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "agent relationships are too large"
    );
    let mut file = tempfile::NamedTempFile::new_in(state)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(state.join(FILE))?;
    Ok(())
}

/// The launch of `child`, listed under `id` with `pid`, from `parent`. The child is named by
/// its row when the roster carries it already, else by `id` when `named` says it is the
/// harness's own rather than a process stand-in.
pub fn launch(
    claude: &Path,
    rows: &[Session],
    parent: &Session,
    harness: &str,
    id: &str,
    pid: Option<u32>,
    named: bool,
) -> Link {
    let home = |s: &Session| {
        harness::by_name(&s.harness)
            .map(|spec| spec.session_home(claude, s))
            .unwrap_or_default()
    };
    let row = rows
        .iter()
        .find(|s| s.session_id == id && s.harness == harness);
    Link {
        kind: Kind::Launch,
        harness: harness.to_owned(),
        home: row.map(home).unwrap_or_default(),
        cwd: row.map(|s| s.cwd.clone()).unwrap_or_default(),
        parent: parent.native().to_owned(),
        child: match row {
            Some(s) => s.native().to_owned(),
            None if named => id.to_owned(),
            None => String::new(),
        },
        parent_harness: (parent.harness != harness).then(|| parent.harness.clone()),
        parent_home: Some(home(parent)),
        parent_pid: parent.pid,
        child_pid: row.and_then(|s| s.pid).or(pid),
    }
}

fn load(state: &Path) -> Result<Vec<Link>> {
    let mut bytes = Vec::new();
    let file = match fs::File::open(state.join(FILE)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::File::open(state.join(FORKS)),
        file => file,
    };
    match file {
        Ok(file) => {
            file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 4 * 1024 * 1024,
                "agent relationships are too large"
            );
            Ok(serde_json::from_slice(&bytes)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Mark each listed fork with its parent's id, and, with `launches`, each listed launch with
/// the id its launcher is listed under now. A launcher listed by its process is found by
/// that pid once its harness names it.
pub fn apply(links: &[Link], claude: &Path, sessions: &mut [Session], launches: bool) {
    let homes: Vec<Option<PathBuf>> = sessions
        .iter()
        .map(|s| Some(harness::by_name(&s.harness)?.session_home(claude, s)))
        .collect();
    // ponytail: a pid recycled into another session of the same harness would match; the
    // native id is checked first, so only a launch its harness never named is exposed.
    let is = |i: usize, harness: &str, home: Option<&Path>, id: &str, pid: Option<u32>| {
        let s = &sessions[i];
        s.harness == harness
            && ((!id.is_empty()
                && s.native() == id
                && home.is_none_or(|h| h.as_os_str().is_empty() || homes[i].as_deref() == Some(h)))
                || pid.is_some_and(|p| s.pid == Some(p)))
    };
    let mut forks = Vec::new();
    let mut launchers = Vec::new();
    for i in 0..sessions.len() {
        let home = homes[i].as_deref();
        if let Some(link) = links.iter().find(|l| {
            l.kind == Kind::Fork
                && home == Some(l.home.as_path())
                && l.harness == sessions[i].harness
                && l.cwd == sessions[i].cwd
                && l.child == sessions[i].native()
        }) {
            forks.push((i, link));
        }
        if !launches {
            continue;
        }
        let Some(link) = links.iter().rev().find(|l| {
            l.kind == Kind::Launch && is(i, &l.harness, Some(&l.home), &l.child, l.child_pid)
        }) else {
            continue;
        };
        if let Some(p) = (0..sessions.len()).find(|&p| {
            p != i
                && is(
                    p,
                    link.parent_harness(),
                    link.parent_home.as_deref(),
                    &link.parent,
                    link.parent_pid,
                )
        }) {
            launchers.push((i, sessions[p].session_id.clone()));
        }
    }
    for (i, link) in forks {
        sessions[i].forked_from = Some(link.parent.clone());
        sessions[i].forked_from_harness = link.parent_harness.clone();
    }
    for (i, launcher) in launchers {
        sessions[i].launched_by = Some(launcher);
    }
}

/// Stable preorder. A missing parent or a cycle never makes a session disappear.
pub fn order<'a>(sessions: &[&'a Session]) -> Vec<(&'a Session, usize)> {
    let mut index: HashMap<(&str, &Path, &str), Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        index
            .entry((&s.harness, &s.cwd, s.native()))
            .or_default()
            .push(i);
    }
    let native_home = |s: &'a Session| -> Option<&'a Path> {
        harness::by_name(&s.harness)?
            .transcript
            .home_of(s.transcript_path.as_deref()?)
    };
    let mut children = vec![Vec::new(); sessions.len()];
    let mut has_parent = vec![false; sessions.len()];
    for (i, s) in sessions.iter().enumerate() {
        // A launch hangs from its launcher by the id the launcher is listed under.
        if let Some(p) = s.launched_by.as_deref().and_then(|launcher| {
            sessions
                .iter()
                .position(|p| p.session_id == launcher && p.session_id != s.session_id)
        }) {
            children[p].push(i);
            has_parent[i] = true;
            continue;
        }
        let Some(parent) = s.forked_from.as_deref() else {
            continue;
        };
        let Some(candidates) = index.get(&(s.parent_harness(), s.cwd.as_path(), parent)) else {
            continue;
        };
        // Homes only compare within one harness; a seeded fork's parent lives in another.
        let matching: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|&p| match (native_home(s), native_home(sessions[p])) {
                (Some(child), Some(parent)) if s.forked_from_harness.is_none() => child == parent,
                _ => true,
            })
            .collect();
        if let [parent] = matching.as_slice() {
            children[*parent].push(i);
            has_parent[i] = true;
        }
    }
    let mut out = Vec::with_capacity(sessions.len());
    let mut seen = vec![false; sessions.len()];
    let roots = (0..sessions.len())
        .filter(|&i| !has_parent[i])
        .chain(0..sessions.len());
    for root in roots {
        let mut stack = vec![(root, 0usize)];
        while let Some((i, depth)) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            out.push((sessions[i], depth.min(8)));
            stack.extend(children[i].iter().rev().map(|&i| (i, depth + 1)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    fn session(id: &str, parent: Option<&str>) -> Session {
        serde_json::from_value(serde_json::json!({
            "session_id":id, "harness":"claude", "cwd":"/project",
            "state":"idle", "forked_from":parent
        }))
        .unwrap()
    }

    #[test]
    fn a_fork_follows_its_parent_without_changing_sibling_order() {
        let child = session("child", Some("parent"));
        let other = session("other", None);
        let parent = session("parent", None);
        let grandchild = session("grandchild", Some("child"));
        let rows = order(&[&child, &other, &grandchild, &parent]);
        assert_eq!(
            rows.iter()
                .map(|(s, d)| (s.session_id.as_str(), *d))
                .collect::<Vec<_>>(),
            [("other", 0), ("parent", 0), ("child", 1), ("grandchild", 2)]
        );
    }

    #[test]
    fn missing_parents_and_cycles_do_not_hide_rows() {
        let a = session("a", Some("b"));
        let b = session("b", Some("a"));
        let c = session("c", Some("missing"));
        let rows = order(&[&a, &b, &c]);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|(s, _)| &s.session_id)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
    }

    #[test]
    fn parentage_is_persistent_and_scoped_to_the_native_home() {
        let state = tempfile::tempdir().unwrap();
        record(
            state.path(),
            Link {
                harness: "claude".into(),
                home: "/home/a".into(),
                cwd: "/project".into(),
                parent: "parent".into(),
                child: "child".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut rows = vec![session("child", None)];
        let links = read(state.path()).unwrap();
        apply(&links, Path::new("/home/b"), &mut rows, true);
        assert!(rows[0].forked_from.is_none());
        apply(&links, Path::new("/home/a"), &mut rows, true);
        assert_eq!(rows[0].forked_from.as_deref(), Some("parent"));
    }
    #[test]
    fn deep_fork_chains_are_iterative_and_visual_indentation_is_bounded() {
        let sessions: Vec<_> = (0..2048)
            .map(|i| {
                session(
                    &i.to_string(),
                    (i > 0).then(|| (i - 1).to_string()).as_deref(),
                )
            })
            .collect();
        let refs: Vec<_> = sessions.iter().rev().collect();
        let rows = order(&refs);
        assert_eq!(rows.len(), 2048);
        assert_eq!(rows[0].0.session_id, "0");
        assert_eq!(rows[2047].0.session_id, "2047");
        assert_eq!(rows[2047].1, 8);
    }
}
