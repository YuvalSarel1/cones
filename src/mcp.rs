//! Native MCP configuration, read where the harness keeps it. cones starts no MCP server, runs
//! no harness command and restarts nothing: it reads the native files, stages changes the user
//! can see, and on save rewrites only the servers table of the scopes that changed.
use crate::config::HarnessKind;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Toml,
}

/// Where a scope's file lives. The dashboard knows the harness home and the project directory;
/// everything else about a scope is in the table below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The harness home cones resolved for the session, such as `CODEX_HOME`.
    Home,
    /// Claude Code reads `.claude.json` from `CLAUDE_CONFIG_DIR` when that is set and from the
    /// user home otherwise, so the default home's parent holds the file, not the home itself.
    ClaudeConfig,
    /// The project directory the session runs in.
    Project,
}

#[derive(Debug)]
pub struct Scope {
    pub name: &'static str,
    /// One line, shown beside the scope so the reach of a change is visible before saving.
    pub about: &'static str,
    pub base: Base,
    pub file: &'static str,
    pub format: Format,
    /// Keys leading to the servers table. `{project}` is the project directory.
    pub at: &'static [&'static str],
}

// ponytail: a table, not a definition block. Two harnesses document their MCP files; move this
// into assets/harnesses/*.yaml when a third one needs a scope cones cannot express here.
const CLAUDE: &[Scope] = &[
    Scope {
        name: "user",
        about: "every project under this native home",
        base: Base::ClaudeConfig,
        file: ".claude.json",
        format: Format::Json,
        at: &["mcpServers"],
    },
    Scope {
        name: "project",
        about: "checked in with the repository",
        base: Base::Project,
        file: ".mcp.json",
        format: Format::Json,
        at: &["mcpServers"],
    },
    Scope {
        name: "local",
        about: "this project only, private to this home",
        base: Base::ClaudeConfig,
        file: ".claude.json",
        format: Format::Json,
        at: &["projects", "{project}", "mcpServers"],
    },
];

const CODEX: &[Scope] = &[Scope {
    name: "user",
    about: "every project under this CODEX_HOME",
    base: Base::Home,
    file: "config.toml",
    format: Format::Toml,
    at: &["mcp_servers"],
}];

/// The scopes a harness actually supports. An empty list means cones has not verified where that
/// harness keeps MCP configuration; it never guesses.
pub fn scopes(kind: HarnessKind) -> &'static [Scope] {
    match kind {
        HarnessKind::Claude => CLAUDE,
        HarnessKind::Codex => CODEX,
        _ => &[],
    }
}

impl Scope {
    pub fn path(&self, home: &Path, project: &Path) -> PathBuf {
        let dir = match self.base {
            Base::Home => home.to_path_buf(),
            Base::ClaudeConfig => claude_config_dir(home),
            Base::Project => project.to_path_buf(),
        };
        dir.join(self.file)
    }

    fn keys(&self, project: &Path) -> Vec<String> {
        self.at
            .iter()
            .map(|key| {
                if *key == "{project}" {
                    project.to_string_lossy().into_owned()
                } else {
                    (*key).to_owned()
                }
            })
            .collect()
    }
}

/// `CLAUDE_CONFIG_DIR` holds `.claude.json` when it is set; otherwise the user home does. cones
/// resolves the default home as `~/.claude`, so that default means "no override in force".
fn claude_config_dir(home: &Path) -> PathBuf {
    let user = dirs::home_dir().unwrap_or_default();
    if home == user.join(crate::fleet::CLAUDE_DIR) {
        user
    } else {
        home.to_path_buf()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// `stdio`, `http`, `sse` or `unstated` — what the entry itself says, never a guess.
    pub transport: String,
    /// The command or address the entry names, shortened for one row.
    pub detail: String,
}

#[derive(Debug)]
pub struct ScopeView {
    pub scope: &'static Scope,
    pub path: PathBuf,
    /// False when the native file does not exist; an absent file is not an error.
    pub present: bool,
    pub servers: Vec<Server>,
    /// An unreadable or malformed file stays explicit instead of reading as empty.
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct Inspection {
    pub kind: HarnessKind,
    pub home: PathBuf,
    pub project: PathBuf,
    pub scopes: Vec<ScopeView>,
}

impl Inspection {
    pub fn supported(&self) -> bool {
        !self.scopes.is_empty()
    }

    pub fn view(&self, scope: &str) -> Option<&ScopeView> {
        self.scopes.iter().find(|v| v.scope.name == scope)
    }
}

/// Read every supported scope. Reads only: no file is created, touched or written.
pub fn inspect(kind: HarnessKind, home: &Path, project: &Path) -> Inspection {
    let scopes = scopes(kind)
        .iter()
        .map(|scope| {
            let path = scope.path(home, project);
            let mut view = ScopeView {
                scope,
                present: path.exists(),
                path,
                servers: Vec::new(),
                error: None,
            };
            if view.present {
                match read_servers(scope, &view.path, project) {
                    Ok(servers) => view.servers = servers,
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
            }
            view
        })
        .collect();
    Inspection {
        kind,
        home: home.to_path_buf(),
        project: project.to_path_buf(),
        scopes,
    }
}

fn read_servers(scope: &Scope, path: &Path, project: &Path) -> Result<Vec<Server>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let keys = scope.keys(project);
    match scope.format {
        Format::Json => {
            let root: Value =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            let mut at = &root;
            for key in &keys {
                match at.get(key) {
                    Some(next) => at = next,
                    None => return Ok(Vec::new()),
                }
            }
            let table = match at.as_object() {
                Some(table) => table,
                None => bail!("{} is not a table of servers", keys.join(".")),
            };
            Ok(table
                .iter()
                .map(|(name, value)| json_server(name, value))
                .collect())
        }
        Format::Toml => {
            let doc = text
                .parse::<toml_edit::DocumentMut>()
                .with_context(|| format!("parse {}", path.display()))?;
            let mut at = doc.as_item();
            for key in &keys {
                match at.get(key) {
                    Some(next) => at = next,
                    None => return Ok(Vec::new()),
                }
            }
            let table = match at.as_table_like() {
                Some(table) => table,
                None => bail!("{} is not a table of servers", keys.join(".")),
            };
            Ok(table
                .iter()
                .map(|(name, item)| toml_server(name, item))
                .collect())
        }
    }
}

fn json_server(name: &str, value: &Value) -> Server {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let transport = text("type")
        .or_else(|| text("transport"))
        .unwrap_or_else(|| {
            if value.get("command").is_some() {
                "stdio".to_owned()
            } else if value.get("url").is_some() {
                "remote".to_owned()
            } else {
                "unstated".to_owned()
            }
        });
    let detail = text("url").unwrap_or_else(|| {
        let args = value
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        format!("{} {}", text("command").unwrap_or_default(), args)
            .trim()
            .to_owned()
    });
    Server {
        name: name.to_owned(),
        transport,
        detail,
    }
}

fn toml_server(name: &str, item: &toml_edit::Item) -> Server {
    let text = |key: &str| {
        item.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    };
    let transport = text("transport")
        .or_else(|| text("type"))
        .unwrap_or_else(|| {
            if item.get("url").is_some() {
                "remote".to_owned()
            } else if item.get("command").is_some() {
                "stdio".to_owned()
            } else {
                "unstated".to_owned()
            }
        });
    let detail = text("url").unwrap_or_else(|| {
        let args = item
            .get("args")
            .and_then(|v| v.as_array())
            .map(|args| {
                args.iter()
                    .filter_map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        format!("{} {}", text("command").unwrap_or_default(), args)
            .trim()
            .to_owned()
    });
    Server {
        name: name.to_owned(),
        transport,
        detail,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Delete a server from one scope. Other scopes keep their own copy.
    Remove { scope: &'static str, name: String },
    /// Write a server another scope already defines, leaving the source alone.
    Copy {
        from: &'static str,
        to: &'static str,
        name: String,
    },
}

impl Change {
    pub fn summary(&self) -> String {
        match self {
            Self::Remove { scope, name } => format!("remove {name} from {scope}"),
            Self::Copy { from, to, name } => format!("copy {name} from {from} to {to}"),
        }
    }

    /// The scopes a change writes to. Nothing else is opened for writing.
    pub fn writes(&self) -> &'static str {
        match self {
            Self::Remove { scope, .. } => scope,
            Self::Copy { to, .. } => to,
        }
    }
}

/// Apply staged changes. Copies run before removals so moving a server between scopes in one
/// save works, and every step is idempotent, so retrying after a failed write is safe.
pub fn apply(kind: HarnessKind, home: &Path, project: &Path, changes: &[Change]) -> Result<()> {
    let scope = |name: &str| -> Result<&'static Scope> {
        scopes(kind)
            .iter()
            .find(|s| s.name == name)
            .with_context(|| format!("{kind} has no {name} MCP scope"))
    };
    let mut edits: BTreeMap<PathBuf, (&'static Scope, Vec<Edit>)> = BTreeMap::new();
    let mut push = |target: &'static Scope, edit: Edit| {
        edits
            .entry(target.path(home, project))
            .or_insert((target, Vec::new()))
            .1
            .push(edit);
    };
    for change in changes.iter().filter(|c| matches!(c, Change::Copy { .. })) {
        let Change::Copy { from, to, name } = change else {
            unreachable!()
        };
        let (from, to) = (scope(from)?, scope(to)?);
        if from.format != to.format {
            bail!("{} and {} store servers differently", from.name, to.name);
        }
        let value = read_entry(from, &from.path(home, project), project, name)?
            .with_context(|| format!("{name} is not in {}", from.name))?;
        push(to, Edit::Insert(name.clone(), value));
    }
    for change in changes
        .iter()
        .filter(|c| matches!(c, Change::Remove { .. }))
    {
        let Change::Remove {
            scope: name,
            name: server,
        } = change
        else {
            unreachable!()
        };
        push(scope(name)?, Edit::Remove(server.clone()));
    }
    for (path, (scope, edits)) in edits {
        write_scope(scope, &path, project, &edits)
            .with_context(|| format!("save {} scope to {}", scope.name, path.display()))?;
    }
    Ok(())
}

enum Edit {
    Insert(String, Value),
    Remove(String),
}

/// A server definition as JSON. Only JSON scopes can be copied, which the caller checks.
fn read_entry(scope: &Scope, path: &Path, project: &Path, name: &str) -> Result<Option<Value>> {
    if scope.format != Format::Json || !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mut at = &root;
    for key in scope.keys(project) {
        match at.get(&key) {
            Some(next) => at = next,
            None => return Ok(None),
        }
    }
    Ok(at.get(name).cloned())
}

fn write_scope(scope: &Scope, path: &Path, project: &Path, edits: &[Edit]) -> Result<()> {
    let keys = scope.keys(project);
    let existing = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let text = match scope.format {
        Format::Json => {
            let mut root: Value = match existing.as_deref() {
                Some(text) => serde_json::from_str(text)
                    .with_context(|| format!("parse {}", path.display()))?,
                None => Value::Object(Map::new()),
            };
            let mut at = &mut root;
            for key in &keys {
                if !at.is_object() {
                    bail!("{} is not a table", keys.join("."));
                }
                at = at
                    .as_object_mut()
                    .expect("checked above")
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            let table = at
                .as_object_mut()
                .with_context(|| format!("{} is not a table of servers", keys.join(".")))?;
            for edit in edits {
                match edit {
                    Edit::Insert(name, value) => {
                        table.insert(name.clone(), value.clone());
                    }
                    Edit::Remove(name) => {
                        table.remove(name);
                    }
                }
            }
            let mut text = serde_json::to_string_pretty(&root)?;
            text.push('\n');
            text
        }
        Format::Toml => {
            let mut doc = match existing.as_deref() {
                Some(text) => text
                    .parse::<toml_edit::DocumentMut>()
                    .with_context(|| format!("parse {}", path.display()))?,
                None => toml_edit::DocumentMut::new(),
            };
            let mut at = doc.as_item_mut();
            for key in &keys {
                let table = at
                    .as_table_like_mut()
                    .with_context(|| format!("{} is not a table", keys.join(".")))?;
                if table.get(key).is_none() {
                    let mut empty = toml_edit::Table::new();
                    empty.set_implicit(true);
                    table.insert(key, toml_edit::Item::Table(empty));
                }
                at = table.get_mut(key).expect("inserted above");
            }
            let table = at
                .as_table_like_mut()
                .with_context(|| format!("{} is not a table of servers", keys.join(".")))?;
            for edit in edits {
                match edit {
                    // Only JSON scopes are copied, so a TOML scope never inserts.
                    Edit::Insert(name, _) => bail!("cannot write {name} into a TOML scope"),
                    Edit::Remove(name) => {
                        table.remove(name);
                    }
                }
            }
            doc.to_string()
        }
    };
    replace(path, &text)
}

/// Write through a temporary file in the same directory so a failure leaves the native file as
/// it was, and keep the mode the harness chose: `.claude.json` holds credentials.
fn replace(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mode = fs::metadata(path).ok().map(|m| m.permissions().mode());
    let mut file = tempfile::Builder::new()
        .prefix(".cones-mcp")
        .tempfile_in(dir)
        .with_context(|| format!("write in {}", dir.display()))?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    if let Some(mode) = mode {
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A home that is not `~/.claude`, so the scope table treats it as a native override.
    fn claude_home(dir: &Path) -> PathBuf {
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        home
    }

    fn claude_json(project: &Path) -> String {
        serde_json::json!({
            "installMethod": "native",
            "mcpServers": {
                "weather": {"command": "weather-mcp", "args": ["--port", "1"]},
                "docs": {"type": "http", "url": "https://docs.example/mcp"}
            },
            "projects": {
                project.to_string_lossy(): {
                    "allowedTools": ["Bash"],
                    "mcpServers": {"local-only": {"command": "./mcp"}}
                },
                "/elsewhere": {"mcpServers": {"other": {"command": "other"}}}
            }
        })
        .to_string()
    }

    #[test]
    fn scopes_come_only_from_verified_harnesses() {
        assert_eq!(
            scopes(HarnessKind::Claude)
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            ["user", "project", "local"]
        );
        assert_eq!(
            scopes(HarnessKind::Codex)
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            ["user"]
        );
        for kind in [HarnessKind::Pi, HarnessKind::Opencode, HarnessKind::Gemini] {
            assert!(scopes(kind).is_empty(), "{kind} is not verified");
        }
        let dir = tempfile::tempdir().unwrap();
        let found = inspect(HarnessKind::Pi, dir.path(), dir.path());
        assert!(!found.supported());
        assert!(found.scopes.is_empty());
    }

    #[test]
    fn reading_reports_every_scope_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write(&home.join(".claude.json"), &claude_json(&project));
        let before = fs::read_to_string(home.join(".claude.json")).unwrap();

        let found = inspect(HarnessKind::Claude, &home, &project);
        let user = found.view("user").unwrap();
        assert!(user.present && user.error.is_none());
        assert_eq!(
            user.servers,
            [
                Server {
                    name: "docs".into(),
                    transport: "http".into(),
                    detail: "https://docs.example/mcp".into()
                },
                Server {
                    name: "weather".into(),
                    transport: "stdio".into(),
                    detail: "weather-mcp --port 1".into()
                },
            ]
        );
        let local = found.view("local").unwrap();
        assert_eq!(
            local.servers.iter().map(|s| &s.name).collect::<Vec<_>>(),
            ["local-only"]
        );
        let missing = found.view("project").unwrap();
        assert!(!missing.present, "absent .mcp.json is not an error");
        assert!(missing.servers.is_empty() && missing.error.is_none());
        assert_eq!(missing.path, project.join(".mcp.json"));

        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            before
        );
        assert!(!project.join(".mcp.json").exists());
    }

    #[test]
    fn an_unreadable_scope_stays_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        write(&project.join(".mcp.json"), "{ not json");
        let found = inspect(HarnessKind::Claude, &home, &project);
        let view = found.view("project").unwrap();
        assert!(view.present && view.servers.is_empty());
        assert!(
            view.error.as_deref().unwrap_or_default().contains("parse"),
            "{:?}",
            view.error
        );
    }

    #[test]
    fn staging_changes_nothing_until_it_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write(&home.join(".claude.json"), &claude_json(&project));
        let before = fs::read_to_string(home.join(".claude.json")).unwrap();
        let staged = [
            Change::Remove {
                scope: "user",
                name: "weather".into(),
            },
            Change::Copy {
                from: "user",
                to: "local",
                name: "docs".into(),
            },
        ];
        assert_eq!(
            staged.iter().map(Change::summary).collect::<Vec<_>>(),
            ["remove weather from user", "copy docs from user to local"]
        );
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            before
        );
    }

    #[test]
    fn applying_touches_one_scope_and_keeps_unrelated_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write(&home.join(".claude.json"), &claude_json(&project));
        write(
            &project.join(".mcp.json"),
            &serde_json::json!({"mcpServers": {"weather": {"command": "weather-mcp"}}}).to_string(),
        );

        apply(
            HarnessKind::Claude,
            &home,
            &project,
            &[Change::Remove {
                scope: "project",
                name: "weather".into(),
            }],
        )
        .unwrap();

        let found = inspect(HarnessKind::Claude, &home, &project);
        assert!(found.view("project").unwrap().servers.is_empty());
        assert_eq!(
            found
                .view("user")
                .unwrap()
                .servers
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>(),
            ["docs", "weather"],
            "the user scope keeps its own copy"
        );
        let root: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(root["installMethod"], "native");
        assert!(root["projects"]["/elsewhere"]["mcpServers"]["other"].is_object());
    }

    #[test]
    fn copying_writes_the_target_scope_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write(&home.join(".claude.json"), &claude_json(&project));

        apply(
            HarnessKind::Claude,
            &home,
            &project,
            &[
                Change::Copy {
                    from: "user",
                    to: "project",
                    name: "docs".into(),
                },
                Change::Remove {
                    scope: "user",
                    name: "docs".into(),
                },
            ],
        )
        .unwrap();

        let found = inspect(HarnessKind::Claude, &home, &project);
        assert_eq!(
            found.view("project").unwrap().servers,
            [Server {
                name: "docs".into(),
                transport: "http".into(),
                detail: "https://docs.example/mcp".into()
            }],
            "a copy carries the whole definition, and the removal ran after it"
        );
        assert_eq!(
            found
                .view("user")
                .unwrap()
                .servers
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>(),
            ["weather"]
        );
        assert_eq!(
            found
                .view("local")
                .unwrap()
                .servers
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>(),
            ["local-only"],
            "the scope nobody named is untouched"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_native_file_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        write(&home.join(".claude.json"), &claude_json(&project));
        let before = fs::read_to_string(home.join(".claude.json")).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();

        let failed = apply(
            HarnessKind::Claude,
            &home,
            &project,
            &[Change::Remove {
                scope: "user",
                name: "weather".into(),
            }],
        )
        .unwrap_err();

        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            format!("{failed:#}").contains("user scope"),
            "{failed:#}: the message names the scope that did not save"
        );
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            before
        );
    }

    #[test]
    fn saving_keeps_the_mode_of_a_file_holding_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let home = claude_home(dir.path());
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let config = home.join(".claude.json");
        write(&config, &claude_json(&project));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        apply(
            HarnessKind::Claude,
            &home,
            &project,
            &[Change::Remove {
                scope: "user",
                name: "weather".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn codex_servers_read_and_remove_without_disturbing_the_rest_of_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let config = home.join("config.toml");
        write(
            &config,
            "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.weather]\ncommand = \"weather-mcp\"\nargs = [\"--port\", \"1\"]\n\n[mcp_servers.docs]\nurl = \"https://docs.example/mcp\"\n",
        );

        let found = inspect(HarnessKind::Codex, &home, dir.path());
        assert_eq!(
            found.view("user").unwrap().servers,
            [
                Server {
                    name: "weather".into(),
                    transport: "stdio".into(),
                    detail: "weather-mcp --port 1".into()
                },
                Server {
                    name: "docs".into(),
                    transport: "remote".into(),
                    detail: "https://docs.example/mcp".into()
                },
            ]
        );

        apply(
            HarnessKind::Codex,
            &home,
            dir.path(),
            &[Change::Remove {
                scope: "user",
                name: "weather".into(),
            }],
        )
        .unwrap();

        let text = fs::read_to_string(&config).unwrap();
        assert!(
            text.starts_with("# my settings\nmodel = \"gpt-5\"\n"),
            "{text}"
        );
        assert!(!text.contains("weather"), "{text}");
        assert!(text.contains("[mcp_servers.docs]"), "{text}");
    }

    #[test]
    fn a_scope_the_harness_does_not_have_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let config = home.join("config.toml");
        write(&config, "[mcp_servers.docs]\nurl = \"https://docs\"\n");
        let before = fs::read_to_string(&config).unwrap();
        let failed = apply(
            HarnessKind::Codex,
            &home,
            dir.path(),
            &[Change::Remove {
                scope: "project",
                name: "docs".into(),
            }],
        )
        .unwrap_err();
        assert!(
            format!("{failed:#}").contains("no project MCP scope"),
            "{failed:#}"
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), before);
    }
}
