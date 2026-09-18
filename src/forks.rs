//! Parentage recorded only after a native fork reports its new session identity.
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub harness: String,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub parent: String,
    pub child: String,
}

pub fn read(state: &Path) -> Result<Vec<Link>> {
    let path = state.join("forks.json");
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(file) => {
            file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 4 * 1024 * 1024, "fork record is too large");
            serde_json::from_slice(&bytes).context("reading fork relationships")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn record(state: &Path, link: Link) -> Result<()> {
    ensure!(
        !link.parent.is_empty() && !link.child.is_empty() && link.parent != link.child,
        "a fork must have a distinct native session id"
    );
    crate::private_dir(state)?;
    let lock = crate::private_file(&state.join("forks.lock"))?;
    lock.lock_exclusive()?;
    let mut links = read(state)?;
    links.retain(|old| {
        !(old.harness == link.harness && old.home == link.home && old.child == link.child)
    });
    links.push(link);
    let bytes = serde_json::to_vec(&links)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "fork record is too large");
    let mut file = tempfile::NamedTempFile::new_in(state)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(state.join("forks.json"))?;
    Ok(())
}

pub fn apply(links: &[Link], claude: &Path, sessions: &mut [Session]) {
    for session in sessions {
        let Some(spec) = harness::by_name(&session.harness) else {
            continue;
        };
        let home = spec.session_home(claude, session);
        if let Some(link) = links.iter().find(|link| {
            link.harness == session.harness
                && link.home == home
                && link.cwd == session.cwd
                && link.child == session.session_id
        }) {
            session.forked_from = Some(link.parent.clone());
        }
    }
}

/// Stable preorder. A missing parent or a cycle never makes a session disappear.
pub fn order<'a>(sessions: &[&'a Session]) -> Vec<(&'a Session, usize)> {
    let mut index: HashMap<(&str, &Path, &str), Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        index
            .entry((&s.harness, &s.cwd, &s.session_id))
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
        let Some(parent) = s.forked_from.as_deref() else {
            continue;
        };
        let Some(candidates) = index.get(&(s.harness.as_str(), s.cwd.as_path(), parent)) else {
            continue;
        };
        let matching: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|&p| match (native_home(s), native_home(sessions[p])) {
                (Some(child), Some(parent)) => child == parent,
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
            },
        )
        .unwrap();
        let mut rows = vec![session("child", None)];
        let links = read(state.path()).unwrap();
        apply(&links, Path::new("/home/b"), &mut rows);
        assert!(rows[0].forked_from.is_none());
        apply(&links, Path::new("/home/a"), &mut rows);
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
