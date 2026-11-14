//! Where each session sits in the dashboard's list: its tag, if it has one, and the order a
//! group was given by hand. Both are keyed by session id, so they live in the state directory
//! beside the ledger, not in `jobs.yaml`.
//!
//! `pinned` is one more tag. A conversation that continues under a new id, a fork or a Claude
//! resume, reports the id it came from; a session with no entry of its own takes the place of
//! the one it came from, so a tag and a position survive both without anyone copying them.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
};

/// The tag `ctrl+t` toggles. It always sorts first and never nests inside a folder.
pub const PINNED: &str = "pinned";
const FILE: &str = "tags.json";
/// The pins kept before pins were a tag: a list of session ids.
const PINS: &str = "pins.json";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tags {
    /// Session id to tag. An empty tag clears one the session would take from its parent.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Sessions whose tag sits inside their folder, `folder > # tag`, rather than above
    /// the folders on its own.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub inside: BTreeSet<String>,
    /// A group's sessions in the order set by hand, by the group's heading.
    #[serde(default)]
    pub order: BTreeMap<String, Vec<String>>,
}

pub fn path(state: &Path) -> PathBuf {
    state.join(FILE)
}

impl Tags {
    /// The stored places. Pins kept before tags existed become `pinned` the first time this
    /// runs, written to the new file; `pins.json` is left as it was.
    pub fn load(state: &Path) -> Self {
        if let Ok(bytes) = std::fs::read(path(state)) {
            return serde_json::from_slice(&bytes).unwrap_or_default();
        }
        let pins: Vec<String> = std::fs::read(state.join(PINS))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let tags = Self {
            tags: pins.into_iter().map(|id| (id, PINNED.to_owned())).collect(),
            order: BTreeMap::new(),
            inside: BTreeSet::new(),
        };
        if !tags.tags.is_empty() {
            let _ = tags.save(state);
        }
        tags
    }

    // ponytail: last writer wins between a dashboard and `cones tag` writing in the same
    // instant; both re-read before writing, so only a simultaneous write is lost.
    pub fn save(&self, state: &Path) -> Result<()> {
        std::fs::create_dir_all(state)?;
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.persist(path(state))
            .with_context(|| format!("write {}", path(state).display()))?;
        Ok(())
    }

    /// The session's tag: its own, else the one of the conversation it continues, which
    /// `parent` names.
    pub fn of<'a>(&'a self, id: &str, parent: impl Fn(&str) -> Option<&'a str>) -> Option<&'a str> {
        let mut at = id;
        for _ in 0..8 {
            if let Some(tag) = self.tags.get(at) {
                return (!tag.is_empty()).then_some(tag.as_str());
            }
            at = parent(at)?;
        }
        None
    }

    /// Whether the session's tag sits inside its folder: decided by whichever entry `of`
    /// takes the tag from.
    pub fn inside_of<'a>(&'a self, id: &'a str, parent: impl Fn(&str) -> Option<&'a str>) -> bool {
        let mut at = id;
        for _ in 0..8 {
            if self.tags.contains_key(at) {
                return self.inside.contains(at);
            }
            let Some(p) = parent(at) else { return false };
            at = p;
        }
        false
    }

    /// Give `id` the tag `tag`, inside its folder or on its own, or clear it. Clearing a tag
    /// the session only takes from its parent leaves an empty entry, so the parent's tag
    /// stops reaching it.
    pub fn set(&mut self, id: &str, tag: Option<&str>, inherited: bool, inside: bool) {
        if inside && tag.is_some_and(|t| t != PINNED) {
            self.inside.insert(id.to_owned());
        } else {
            self.inside.remove(id);
        }
        match tag {
            Some(tag) => self.tags.insert(id.to_owned(), tag.to_owned()),
            None if inherited => self.tags.insert(id.to_owned(), String::new()),
            None => self.tags.remove(id),
        };
    }

    /// Where `id` sorts in its group's hand order, taking the place of the conversation it
    /// continues when it is not listed itself. `None` sorts after every listed session.
    pub fn position<'a>(
        &self,
        group: &str,
        id: &'a str,
        parent: impl Fn(&str) -> Option<&'a str>,
    ) -> Option<usize> {
        let list = self.order.get(group)?;
        let mut at = id;
        for _ in 0..8 {
            if let Some(i) = list.iter().position(|x| x == at) {
                return Some(i);
            }
            at = parent(at)?;
        }
        None
    }
}

/// A tag as typed: surrounding space and a leading `#` dropped. A heading is one line.
pub fn name(raw: &str) -> Result<String> {
    let name = raw.trim().trim_start_matches('#').trim();
    ensure!(!name.is_empty(), "a tag needs a name");
    ensure!(
        !name.chars().any(char::is_control),
        "a tag name is one line of text"
    );
    Ok(name.to_owned())
}
