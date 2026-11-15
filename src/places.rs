//! Where each session sits in the dashboard's list, and the order a group was given by hand.
//! Both are keyed by session id, so they live in the state directory beside the ledger, not in
//! `jobs.yaml`.
//!
//! A session sits in any number of places: its folder, a tag heading a group above the folders,
//! or a tag inside its own folder. `pinned` is a tag that heads the list. A session with no entry
//! sits in its folder, and removing its last place puts it back there, so it never disappears.
//! A conversation that continues under a new id, a fork or a Claude resume, reports the id it
//! came from; a session with no entry of its own takes the places and the position of the one it
//! came from, so both survive without anyone copying them.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::Write,
    path::{Path, PathBuf},
};

/// The tag `ctrl+t` toggles. It always sorts first.
pub const PINNED: &str = "pinned";
const FILE: &str = "tags.json";
/// The pins kept before pins were a tag: a list of session ids.
const PINS: &str = "pins.json";

/// One place a session is listed in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Place {
    /// The session's own folder.
    Folder,
    /// A group of its own above the folders, `# NAME`; `pinned` heads the list.
    Tag(String),
    /// An indented `# NAME` heading inside the session's own folder.
    FolderTag(String),
}

impl Place {
    /// `folder`, `# NAME`, `folder › # NAME` (or `folder > # NAME`), or `pinned`.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw == "folder" {
            return Ok(Self::Folder);
        }
        if raw == PINNED {
            return Ok(Self::Tag(PINNED.into()));
        }
        for sep in ["›", ">"] {
            if let Some(rest) = raw.strip_prefix("folder").map(str::trim_start)
                && let Some(tag) = rest.strip_prefix(sep)
            {
                return Ok(Self::FolderTag(name(tag)?));
            }
        }
        if raw.starts_with('#') {
            return Ok(Self::Tag(name(raw)?));
        }
        bail!("{raw:?} is not a place: write `folder`, `# NAME`, `folder › # NAME` or `pinned`")
    }

    /// The tag's name, for either kind of tag.
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Folder => None,
            Self::Tag(t) | Self::FolderTag(t) => Some(t),
        }
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Folder => f.write_str("folder"),
            Self::Tag(t) if t == PINNED => f.write_str(PINNED),
            Self::Tag(t) => write!(f, "# {t}"),
            Self::FolderTag(t) => write!(f, "folder › # {t}"),
        }
    }
}

impl TryFrom<String> for Place {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<Place> for String {
    fn from(p: Place) -> Self {
        p.to_string()
    }
}

pub type Set = BTreeSet<Place>;

fn folder_only() -> Set {
    Set::from([Place::Folder])
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Places {
    /// Session id to the places it sits in.
    #[serde(default)]
    pub places: BTreeMap<String, Set>,
    /// A group's sessions in the order set by hand, by the group's heading.
    #[serde(default)]
    pub order: BTreeMap<String, Vec<String>>,
    /// Session ids marked with ctrl+p, whose titles are drawn in the highlight colour.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub highlighted: BTreeSet<String>,
}

/// The file as an earlier cones wrote it: one tag per session, empty for none, and the sessions
/// whose tag sat inside their folder.
#[derive(Deserialize)]
struct Stored {
    #[serde(default)]
    places: Option<BTreeMap<String, Set>>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    inside: BTreeSet<String>,
    #[serde(default)]
    order: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    highlighted: BTreeSet<String>,
}

pub fn path(state: &Path) -> PathBuf {
    state.join(FILE)
}

impl Places {
    /// The stored places. A file from before places, and pins kept before tags, convert the
    /// first time this runs and are written back in this form; `pins.json` is left as it was.
    pub fn load(state: &Path) -> Self {
        let (places, migrated) = match std::fs::read(path(state)) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(Stored {
                    places: Some(places),
                    order,
                    highlighted,
                    ..
                }) => {
                    return Self {
                        places,
                        order,
                        highlighted,
                    };
                }
                Ok(old) => (Self::from_tags(old), true),
                Err(_) => return Self::default(),
            },
            Err(_) => {
                let pins: Vec<String> = std::fs::read(state.join(PINS))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
                let places = Self {
                    places: pins
                        .into_iter()
                        .map(|id| (id, Set::from([Place::Tag(PINNED.into())])))
                        .collect(),
                    ..Self::default()
                };
                let any = !places.places.is_empty();
                (places, any)
            }
        };
        if migrated {
            let _ = places.save(state);
        }
        places
    }

    fn from_tags(old: Stored) -> Self {
        let places = old
            .tags
            .into_iter()
            .map(|(id, tag)| {
                let place = match tag.as_str() {
                    "" => Place::Folder,
                    PINNED => Place::Tag(tag),
                    _ if old.inside.contains(&id) => Place::FolderTag(tag),
                    _ => Place::Tag(tag),
                };
                (id, Set::from([place]))
            })
            .collect();
        Self {
            places,
            order: old.order,
            highlighted: old.highlighted,
        }
    }

    // ponytail: last writer wins between a dashboard and `cones place` writing in the same
    // instant; both re-read before writing, so only a simultaneous write is lost.
    pub fn save(&self, state: &Path) -> Result<()> {
        std::fs::create_dir_all(state)?;
        let mut file = tempfile::NamedTempFile::new_in(state)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.persist(path(state))
            .with_context(|| format!("write {}", path(state).display()))?;
        Ok(())
    }

    /// The session's places: its own, else those of the conversation it continues, which
    /// `parent` names, else its folder.
    pub fn of<'a>(&self, id: &'a str, parent: impl Fn(&str) -> Option<&'a str>) -> Set {
        let mut at = id;
        for _ in 0..8 {
            if let Some(set) = self.places.get(at) {
                return set.clone();
            }
            match parent(at) {
                Some(p) => at = p,
                None => break,
            }
        }
        folder_only()
    }

    /// Give `id` the set `set`; an empty one means its folder. A set equal to what it would
    /// take anyway is not stored.
    pub fn set<'a>(&mut self, id: &'a str, set: Set, parent: impl Fn(&str) -> Option<&'a str>) {
        let set = if set.is_empty() { folder_only() } else { set };
        self.places.remove(id);
        if self.of(id, parent) != set {
            self.places.insert(id.to_owned(), set);
        }
    }

    /// Add `place` to the session, remove `from` first if given: a move. Both are idempotent.
    pub fn change<'a>(
        &mut self,
        id: &'a str,
        from: Option<&Place>,
        to: Option<&Place>,
        parent: impl Fn(&str) -> Option<&'a str> + Copy,
    ) {
        let mut set = self.of(id, parent);
        if let Some(from) = from {
            set.remove(from);
        }
        if let Some(to) = to {
            set.insert(to.clone());
        }
        self.set(id, set, parent);
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
