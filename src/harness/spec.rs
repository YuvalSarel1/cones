//! Embedded harness definitions. YAML describes data and selects typed native handlers.
//! These definitions are shipped with cones; user-supplied definitions are not loaded.
use crate::{config::HarnessKind, fleet::Session};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

const BUILTINS: &[(HarnessKind, &str)] = &[
    (
        HarnessKind::Claude,
        include_str!("../../assets/harnesses/claude.yaml"),
    ),
    (
        HarnessKind::Codex,
        include_str!("../../assets/harnesses/codex.yaml"),
    ),
    (
        HarnessKind::Pi,
        include_str!("../../assets/harnesses/pi.yaml"),
    ),
    (
        HarnessKind::Opencode,
        include_str!("../../assets/harnesses/opencode.yaml"),
    ),
    (
        HarnessKind::Gemini,
        include_str!("../../assets/harnesses/gemini.yaml"),
    ),
    (
        HarnessKind::Cursor,
        include_str!("../../assets/harnesses/cursor-agent.yaml"),
    ),
    (
        HarnessKind::Copilot,
        include_str!("../../assets/harnesses/copilot.yaml"),
    ),
    (
        HarnessKind::Amp,
        include_str!("../../assets/harnesses/amp.yaml"),
    ),
    (
        HarnessKind::Droid,
        include_str!("../../assets/harnesses/droid.yaml"),
    ),
    (
        HarnessKind::Kimi,
        include_str!("../../assets/harnesses/kimi.yaml"),
    ),
];

static SPECS: LazyLock<Vec<HarnessSpec>> = LazyLock::new(|| {
    BUILTINS
        .iter()
        .map(|(kind, yaml)| {
            let spec = HarnessSpec::parse(yaml).expect("invalid embedded harness definition");
            assert_eq!(spec.kind, *kind, "harness registration and YAML disagree");
            spec
        })
        .collect()
});
static KINDS: LazyLock<Vec<HarnessKind>> =
    LazyLock::new(|| BUILTINS.iter().map(|(kind, _)| *kind).collect());
static LAUNCHABLE: LazyLock<Vec<HarnessKind>> = LazyLock::new(|| {
    SPECS
        .iter()
        .filter(|spec| spec.operations.launch.is_some())
        .map(|spec| spec.kind)
        .collect()
});

/// Registration order is the composer's cycle order.
pub fn known() -> &'static [HarnessKind] {
    &KINDS
}

pub fn launchable() -> &'static [HarnessKind] {
    &LAUNCHABLE
}

pub fn spec(kind: HarnessKind) -> &'static HarnessSpec {
    SPECS
        .iter()
        .find(|s| s.kind == kind)
        .expect("unregistered harness")
}

pub fn by_name(name: &str) -> Option<&'static HarnessSpec> {
    SPECS.iter().find(|s| s.name == name)
}

#[derive(Debug)]
pub struct HarnessSpec {
    pub version: u32,
    pub kind: HarnessKind,
    pub name: String,
    pub icon: String,
    pub colour: Option<[u8; 3]>,
    pub home: Home,
    pub discovery: Discovery,
    pub state: State,
    pub transcript: Transcript,
    pub launch: Option<Launch>,
    pub default_session: SessionKind,
    pub session_kinds: BTreeMap<String, SessionKind>,
    pub commands: Commands,
    pub operations: Operations,
    pub execution: Execution,
    pub input: Input,
    pub viewer: Viewer,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    version: u32,
    kind: HarnessKind,
    icon: String,
    colour: Option<[u8; 3]>,
    home: Home,
    discovery: Discovery,
    #[serde(default)]
    state: State,
    transcript: Transcript,
    #[serde(default)]
    operations: Operations,
    #[serde(default)]
    execution: Execution,
    #[serde(default)]
    viewer: Viewer,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operations {
    pub launch: Option<Launch>,
    pub attach: Option<Operation>,
    pub resume: Option<Operation>,
    pub fork: Option<Operation>,
    pub remove: Option<Operation>,
    /// End a live session and keep its conversation. `{id}` or `{short_id}`. Distinct from
    /// `remove`, which deletes the record: a harness with no `stop` cannot be stopped from
    /// cones, and saying so is better than deleting the work instead.
    pub stop: Option<Operation>,
    pub unarchive: Option<Operation>,
    /// Deliver one note to a live session. `{id}`, `{text}` and, where the harness talks to a
    /// daemon, `{remote}`. A harness without it cannot be written to from cones.
    pub message: Option<Operation>,
    #[serde(default)]
    pub rename: bool,
    #[serde(default)]
    pub sessions: Sessions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub args: Vec<String>,
    pub probe: Option<Probe>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sessions {
    #[serde(default)]
    pub default: SessionKind,
    #[serde(default)]
    pub kinds: BTreeMap<String, SessionKind>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Home {
    pub env: String,
    pub default: HomeDefault,
    pub siblings: Option<Siblings>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "base", rename_all = "snake_case", deny_unknown_fields)]
pub enum HomeDefault {
    Provided,
    ProvidedParent {
        path: PathBuf,
    },
    User {
        path: PathBuf,
    },
    /// The environment value is the XDG data base, with the application path appended.
    XdgData {
        path: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Siblings {
    pub prefix: String,
    pub marker: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    #[serde(skip)]
    pub handler: Native,
    pub registry: Option<PathBuf>,
    pub daemon: Option<Daemon>,
    #[serde(default)]
    pub exclude_subcommands: Vec<String>,
    pub process: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub entrypoints: Vec<PathBuf>,
    pub process_title: Option<String>,
}

/// A daemon that holds threads with no client attached, and the two files in the home that say
/// which. Discovery reads both, so they belong beside the registry path rather than in the handler.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Daemon {
    pub pid: PathBuf,
    pub locks: PathBuf,
}

impl Discovery {
    /// The OS parser is shared. Each definition supplies only its native process filter.
    pub fn processes<'a>(&self, ps: &'a str) -> Vec<crate::fleet::ProcessLine<'a>> {
        let Some(program) = &self.process else {
            return Vec::new();
        };
        crate::fleet::process_lines(ps)
            .into_iter()
            .filter(|line| {
                if self.process_title.as_deref() == Some(line.command.trim()) {
                    return true;
                }
                let mut words = line.command.split_whitespace();
                let first = words.next().unwrap_or("");
                let executable = Path::new(first)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("");
                let native = |name: &str| name == program;
                let alias = self.aliases.iter().any(|name| name == executable)
                    && Path::new(first)
                        .canonicalize()
                        .ok()
                        .and_then(|p| p.file_name().map(OsStr::to_owned))
                        .is_some_and(|name| name == program.as_str());
                let python = executable
                    .strip_prefix("python")
                    .is_some_and(|tail| tail.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
                let matches = if native(executable) || alias {
                    true
                } else if matches!(executable, "node" | "bun") || python {
                    words.next().is_some_and(|script| {
                        Path::new(script)
                            .file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(native)
                            || self
                                .entrypoints
                                .iter()
                                .any(|suffix| Path::new(script).ends_with(suffix))
                    })
                } else {
                    false
                };
                matches
                    && !words.next().is_some_and(|arg| {
                        self.exclude_subcommands
                            .iter()
                            .any(|excluded| excluded == arg)
                    })
            })
            .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(skip)]
    pub handler: StateHandler,
    #[serde(default)]
    pub rules: Vec<StateRule>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateHandler {
    ClaudeRegistry,
    #[default]
    EventMap,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRule {
    pub when: BTreeMap<String, String>,
    pub field: String,
    pub values: BTreeMap<String, StateWord>,
    /// Null preserves prior state on an unrecognized value. A value explicitly replaces it.
    pub otherwise: Option<StateWord>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateWord {
    Active,
    Idle,
    Done,
    Failed,
    Stopped,
    Blocked,
    #[serde(rename = "-")]
    Unknown,
}

impl StateWord {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Blocked => "blocked",
            Self::Unknown => "-",
        }
    }
}

impl State {
    pub fn read(&self, event: &serde_json::Value) -> Option<&'static str> {
        self.rules.iter().find_map(|rule| {
            if !rule.when.iter().all(|(path, value)| {
                event.pointer(path).and_then(serde_json::Value::as_str) == Some(value.as_str())
            }) {
                return None;
            }
            event
                .pointer(&rule.field)
                .and_then(serde_json::Value::as_str)
                .and_then(|value| rule.values.get(value))
                .copied()
                .or(rule.otherwise)
                .map(StateWord::as_str)
        })
    }
}

/// Complex native report interpretation remains ordinary Rust.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Native {
    #[default]
    Claude,
    Codex,
    Pi,
    Opencode,
    External(HarnessKind),
}

impl Native {
    pub fn sessions(self, home: &Path) -> Result<Vec<Session>> {
        match self {
            Self::Claude => crate::fleet::sessions(home),
            Self::Codex => crate::codex::sessions(home),
            Self::Pi => crate::pi::sessions(home),
            Self::Opencode => crate::opencode::sessions(home),
            Self::External(kind) => crate::agents::sessions(kind, home),
        }
    }

    /// Adding a native reader requires an adapter for the shared accounting path.
    /// Terminal-only entries cannot interpret usage until they acquire native readers.
    pub(crate) fn accounting(self) -> Box<dyn crate::cost::Reader> {
        use crate::cost::Accounting;
        match self {
            Self::Claude => Box::new(Accounting::<crate::fleet::CostAdapter>::default()),
            Self::Codex => Box::new(Accounting::<crate::codex::CostAdapter>::default()),
            Self::Pi => Box::new(Accounting::<crate::pi::CostAdapter>::default()),
            Self::Opencode => Box::new(Accounting::<crate::opencode::CostAdapter>::default()),
            Self::External(kind) => crate::agents::accounting(kind),
        }
    }
}

fn handlers(kind: HarnessKind) -> (Native, LaunchHandler, Resume) {
    match kind {
        HarnessKind::Claude => (
            Native::Claude,
            LaunchHandler::ClaudeBackground,
            Resume::BackgroundThenAttach,
        ),
        HarnessKind::Codex => (
            Native::Codex,
            LaunchHandler::CodexRemote,
            Resume::CodexRemote,
        ),
        HarnessKind::Pi => (Native::Pi, LaunchHandler::Terminal, Resume::Transcript),
        HarnessKind::Opencode => (Native::Opencode, LaunchHandler::Terminal, Resume::SessionId),
        kind @ HarnessKind::Gemini
        | kind @ HarnessKind::Cursor
        | kind @ HarnessKind::Copilot
        | kind @ HarnessKind::Amp
        | kind @ HarnessKind::Droid
        | kind @ HarnessKind::Kimi => (
            Native::External(kind),
            LaunchHandler::Terminal,
            Resume::SessionId,
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    #[serde(default = "yes")]
    pub available: bool,
    #[serde(skip)]
    pub handler: Native,
    pub roots: Vec<ScanRoot>,
    pub statusline: Option<Statusline>,
    pub messages: Messages,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Messages {
    pub user: MessageText,
    pub assistant: MessageText,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageText {
    pub id: Option<String>,
    #[serde(default)]
    pub headline: Headline,
    pub sources: Vec<TextSource>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Headline {
    #[default]
    First,
    Last,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSource {
    pub when: BTreeMap<String, String>,
    #[serde(default)]
    pub unless: Vec<String>,
    pub path: String,
    pub shape: TextShape,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextShape {
    String,
    Blocks,
    StringOrBlocks,
}

impl MessageText {
    pub fn parts<'a>(&'a self, event: &'a serde_json::Value, attachments: bool) -> Vec<&'a str> {
        let mut parts = Vec::new();
        for source in &self.sources {
            if !source.when.iter().all(|(path, value)| {
                event.pointer(path).and_then(serde_json::Value::as_str) == Some(value.as_str())
            }) || source
                .unless
                .iter()
                .any(|path| event.pointer(path).is_some_and(|v| v == true))
            {
                continue;
            }
            let Some(content) = event.pointer(&source.path) else {
                continue;
            };
            if source.shape != TextShape::Blocks
                && let Some(text) = content.as_str()
            {
                parts.push(text);
            }
            if source.shape == TextShape::String {
                continue;
            }
            for block in content.as_array().into_iter().flatten() {
                let kind = block["type"].as_str().unwrap_or("");
                if (source.types.is_empty() || source.types.iter().any(|t| t == kind))
                    && let Some(text) = block["text"].as_str()
                {
                    parts.push(text);
                } else if attachments && let Some(label) = source.labels.get(kind) {
                    parts.push(label);
                }
            }
        }
        parts
    }

    pub fn headline(&self, event: &serde_json::Value) -> Option<String> {
        self.headline_with_attachments(event, false)
    }

    pub fn headline_with_attachments(
        &self,
        event: &serde_json::Value,
        attachments: bool,
    ) -> Option<String> {
        let mut lines = self
            .parts(event, attachments)
            .into_iter()
            .filter_map(crate::fleet::headline);
        match self.headline {
            Headline::First => lines.next(),
            Headline::Last => lines.next_back(),
        }
    }

    pub fn id<'a>(&self, event: &'a serde_json::Value) -> Option<&'a str> {
        event.pointer(self.id.as_deref()?)?.as_str()
    }
}

impl Transcript {
    pub fn live_scan_root(&self) -> &ScanRoot {
        self.roots
            .iter()
            .find(|root| !root.archived)
            .expect("this harness has no native transcript root")
    }

    pub fn live_path(&self, home: &Path) -> PathBuf {
        self.live_scan_root().resolve(home)
    }

    pub fn live_root(&self) -> &Path {
        &self
            .roots
            .iter()
            .find(|root| !root.archived)
            .expect("validated live root")
            .path
    }

    pub fn home_of<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let root = self.live_root();
        path.ancestors()
            .find(|p| p.ends_with(root))?
            .ancestors()
            .nth(root.components().count())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRoot {
    pub path: PathBuf,
    pub env: Option<String>,
    /// Number of directory levels below the root; null means recursive.
    pub depth: Option<usize>,
    pub archived: bool,
}

impl ScanRoot {
    pub fn override_dir(&self) -> Option<PathBuf> {
        std::env::var_os(self.env.as_ref()?)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    pub fn resolve(&self, home: &Path) -> PathBuf {
        self.resolve_with(home, self.override_dir().as_deref())
    }

    pub fn resolve_with(&self, home: &Path, directory: Option<&Path>) -> PathBuf {
        directory
            .filter(|path| !path.as_os_str().is_empty())
            .map_or_else(|| home.join(&self.path), Path::to_owned)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statusline {
    pub directory: PathBuf,
    pub window_pointer: String,
    pub cost_pointer: Option<String>,
    pub effort_pointer: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    #[serde(skip)]
    pub handler: LaunchHandler,
    pub identity: LaunchIdentity,
    pub session_kind: Option<String>,
    #[serde(default)]
    pub prefix: Vec<String>,
    #[serde(default)]
    pub remote: Vec<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Native reasoning-effort flag, when the harness has one of its own.
    pub effort: Option<String>,
    /// Env variable that sends this harness to Amazon Bedrock, when it has one.
    /// AWS_PROFILE and AWS_REGION are not declared here: every harness resolves
    /// AWS the same way, so cones passes them to all of them.
    pub bedrock: Option<String>,
    pub prompt: Vec<String>,
    #[serde(default)]
    pub stdin_prompt: bool,
    #[serde(default)]
    pub prompt_flag: Option<String>,
    pub probe: Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchIdentity {
    BackgroundId,
    ClientPid,
    ReportedThread,
}

impl LaunchIdentity {
    pub fn owns_client_pid(self) -> bool {
        self != Self::BackgroundId
    }

    pub fn unresolved_key(self, name: &str, key: &str, pid: u32) -> bool {
        self.owns_client_pid()
            && (key.starts_with(&format!("{name}:start:"))
                || (self == Self::ReportedThread && key == format!("{name}-{pid}")))
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchHandler {
    ClaudeBackground,
    CodexRemote,
    #[default]
    Terminal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub args: Vec<String>,
    #[serde(default)]
    pub output: ProbeOutput,
    #[serde(default = "yes")]
    pub require_success: bool,
    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub version: Version,
    pub minimum_version: Option<String>,
    pub error: String,
    pub description: String,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutput {
    #[default]
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum Version {
    #[default]
    None,
    Text,
    JsonLines {
        pointer: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKind {
    pub join: Join,
    pub stop: Stop,
    pub lifetime: Lifetime,
}

impl Default for SessionKind {
    fn default() -> Self {
        Self {
            join: Join::Unavailable,
            stop: Stop::Signal,
            lifetime: Lifetime::Terminal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Join {
    Unavailable,
    Attach,
    CodexRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    Signal,
    Remove,
    ForgetClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifetime {
    Terminal,
    Daemon,
}

#[derive(Debug)]
pub struct Commands {
    pub attach: Vec<String>,
    pub resume: Vec<String>,
    pub remove: Vec<String>,
    pub unarchive: Vec<String>,
    pub resume_handler: Resume,
    pub viewer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resume {
    BackgroundThenAttach,
    CodexRemote,
    Transcript,
    SessionId,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    #[serde(default)]
    pub enforcement: Support,
    #[serde(default)]
    pub result: Support,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub return_to_list: Vec<ReturnBinding>,
    pub empty_prompt: EmptyPrompt,
    pub markers: String,
    pub ignore_braille: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            return_to_list: vec![
                ReturnBinding {
                    key: ReturnKey::CtrlZ,
                    when: ReturnWhen::Always,
                },
                ReturnBinding {
                    key: ReturnKey::Tab,
                    when: ReturnWhen::EmptyPrompt,
                },
                ReturnBinding {
                    key: ReturnKey::Left,
                    when: ReturnWhen::EmptyPrompt,
                },
            ],
            empty_prompt: EmptyPrompt::Marker,
            markers: "|>$#│┊┃▐›❯".into(),
            ignore_braille: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnBinding {
    pub key: ReturnKey,
    pub when: ReturnWhen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum ReturnKey {
    #[serde(rename = "ctrl+z")]
    CtrlZ,
    #[serde(rename = "tab")]
    Tab,
    #[serde(rename = "left")]
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnWhen {
    Always,
    EmptyPrompt,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Viewer {
    pub label: Option<String>,
    pub input: Input,
    pub peek: Peek,
    pub peek_blocked_states: Vec<StateWord>,
    pub retention: Retention,
    pub input_alignment: InputAlignment,
}

impl Default for Viewer {
    fn default() -> Self {
        Self {
            label: None,
            input: Input::default(),
            peek: Peek::Unavailable,
            peek_blocked_states: vec![StateWord::Failed, StateWord::Stopped],
            retention: Retention::Retain,
            input_alignment: InputAlignment::BottomRule,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Peek {
    Unavailable,
    Join,
    ExistingDaemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    EvictLive,
    Retain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAlignment {
    BottomRule,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyPrompt {
    Marker,
    Bordered,
    Opencode,
}

impl HarnessSpec {
    pub fn parse(yaml: &str) -> Result<Self> {
        let mut definition: Definition =
            serde_yaml::from_str(yaml).context("reading harness definition")?;
        let (native, launch, resume) = handlers(definition.kind);
        definition.discovery.handler = native;
        definition.transcript.handler = native;
        definition.state.handler = if native == Native::Claude {
            StateHandler::ClaudeRegistry
        } else {
            StateHandler::EventMap
        };
        if let Some(operation) = &mut definition.operations.launch {
            operation.handler = launch;
        }
        let name = definition.kind.to_string();
        let arguments = |operation: &Option<Operation>| {
            operation
                .as_ref()
                .map_or_else(Vec::new, |op| op.args.clone())
        };
        let spec = Self {
            version: definition.version,
            kind: definition.kind,
            name: name.clone(),
            icon: definition.icon,
            colour: definition.colour,
            home: definition.home,
            discovery: definition.discovery,
            state: definition.state,
            transcript: definition.transcript,
            launch: definition.operations.launch.clone(),
            default_session: definition.operations.sessions.default.clone(),
            session_kinds: definition.operations.sessions.kinds.clone(),
            commands: Commands {
                attach: arguments(&definition.operations.attach),
                resume: arguments(&definition.operations.resume),
                remove: arguments(&definition.operations.remove),
                unarchive: arguments(&definition.operations.unarchive),
                resume_handler: resume,
                viewer: definition.viewer.label.clone().unwrap_or(name),
            },
            operations: definition.operations,
            execution: definition.execution,
            input: definition.viewer.input.clone(),
            viewer: definition.viewer,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "unsupported harness definition version {}",
            self.version
        );
        ensure!(!self.icon.trim().is_empty(), "harness icon is empty");
        ensure!(
            (self.home.env.is_empty() && self.kind.terminal_only())
                || (!self.home.env.is_empty()
                    && self
                        .home
                        .env
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && !self.home.env.as_bytes()[0].is_ascii_digit()),
            "invalid home environment variable"
        );
        match &self.home.default {
            HomeDefault::Provided => {}
            HomeDefault::ProvidedParent { path }
            | HomeDefault::User { path }
            | HomeDefault::XdgData { path } => relative(path)?,
        }
        if let Some(path) = &self.discovery.registry {
            relative(path)?;
        }
        ensure!(
            (self.discovery.handler == Native::Claude) == self.discovery.registry.is_some(),
            "registry discovery requires a native registry path"
        );
        if let Some(daemon) = &self.discovery.daemon {
            relative(&daemon.pid)?;
            relative(&daemon.locks)?;
        }
        ensure!(
            (self.discovery.handler == Native::Codex) == self.discovery.daemon.is_some(),
            "a daemon holding threads with no client requires its pid and lock paths"
        );
        ensure!(
            (self.discovery.handler != Native::Claude) == self.discovery.process.is_some(),
            "process discovery requires a process name"
        );
        if let Some(process) = &self.discovery.process {
            ensure!(
                !process.is_empty() && !process.contains(|c: char| c.is_whitespace() || c == '/'),
                "invalid native process name"
            );
        }
        for alias in &self.discovery.aliases {
            ensure!(
                !alias.is_empty() && !alias.contains(|c: char| c.is_whitespace() || c == '/'),
                "invalid executable alias"
            );
        }
        for path in &self.discovery.entrypoints {
            relative(path)?;
        }
        if let Some(title) = &self.discovery.process_title {
            ensure!(
                !title.trim().is_empty()
                    && title.len() <= 128
                    && !title.chars().any(char::is_control),
                "invalid native process title"
            );
        }
        ensure!(
            (self.kind == HarnessKind::Claude)
                == (self.state.handler == StateHandler::ClaudeRegistry),
            "state handler does not implement this harness"
        );
        ensure!(
            self.kind.terminal_only()
                || (self.state.handler == StateHandler::ClaudeRegistry)
                    == self.state.rules.is_empty(),
            "registry state uses its native handler; event state requires rules"
        );
        for rule in &self.state.rules {
            ensure!(
                !rule.when.is_empty() && !rule.values.is_empty(),
                "state rule needs a guard and mapped values"
            );
            for path in rule.when.keys().chain([&rule.field]) {
                pointer(path)?;
            }
        }
        if let Some(siblings) = &self.home.siblings {
            ensure!(
                !siblings.prefix.is_empty() && !siblings.prefix.contains('/'),
                "invalid sibling prefix"
            );
            relative(&siblings.marker)?;
        }
        ensure!(
            self.transcript.available != self.kind.terminal_only(),
            "native readers require transcript definitions"
        );
        ensure!(
            self.transcript.available != self.transcript.roots.is_empty(),
            "available transcripts need roots; unavailable transcripts must not invent roots"
        );
        ensure!(
            self.transcript.roots.iter().filter(|r| !r.archived).count()
                == usize::from(self.transcript.available),
            "exactly one live transcript root is required"
        );
        let mut roots = HashSet::new();
        for root in &self.transcript.roots {
            relative(&root.path)?;
            if let Some(env) = &root.env {
                environment_name(env)?;
            }
            ensure!(roots.insert(&root.path), "duplicate transcript root");
        }
        if let Some(statusline) = &self.transcript.statusline {
            relative(&statusline.directory)?;
            pointer(&statusline.window_pointer)?;
            if let Some(cost) = &statusline.cost_pointer {
                pointer(cost)?;
            }
            if let Some(effort) = &statusline.effort_pointer {
                pointer(effort)?;
            }
        }
        for message in [
            &self.transcript.messages.user,
            &self.transcript.messages.assistant,
        ] {
            if let Some(id) = &message.id {
                pointer(id)?;
            }
            ensure!(
                self.transcript.available != message.sources.is_empty(),
                "message sources must match transcript availability"
            );
            for source in &message.sources {
                ensure!(
                    !source.when.is_empty(),
                    "message source requires an event guard"
                );
                for path in source
                    .when
                    .keys()
                    .chain(&source.unless)
                    .chain([&source.path])
                {
                    pointer(path)?;
                }
            }
        }
        if let Some(launch) = &self.launch {
            launch.probe.validate()?;
            validate_args(&launch.prefix, &[])?;
            validate_args(&launch.remote, &["remote", "cwd"])?;
            validate_args(&launch.prompt, &["prompt"])?;
            let named_prompt =
                self.kind == HarnessKind::Opencode && launch.prompt == ["--prompt", "{prompt}"];
            ensure!(
                launch.prompt.last().map(String::as_str) == Some("{prompt}")
                    && launch.prompt.iter().filter(|a| *a == "{prompt}").count() == 1
                    && (launch.prompt.iter().any(|a| a == "--")
                        || named_prompt
                        || launch.prompt_flag.is_some()
                        || launch.stdin_prompt),
                "launch must pass one prompt after --, or OpenCode's --prompt"
            );
            for flag in [
                &launch.model,
                &launch.provider,
                &launch.effort,
                &launch.prompt_flag,
            ]
            .into_iter()
            .flatten()
            {
                ensure!(
                    flag.starts_with('-')
                        && !flag.contains(['{', '\0'])
                        && !flag.contains(char::is_whitespace),
                    "invalid model, provider or effort flag"
                );
            }
            ensure!(
                !launch.stdin_prompt || launch.handler == LaunchHandler::Terminal,
                "stdin prompts require a terminal adapter"
            );
            ensure!(
                !launch.stdin_prompt || launch.prompt_flag.is_none(),
                "stdin and a named prompt flag are mutually exclusive"
            );
            ensure!(
                launch.provider.is_none() || self.kind == HarnessKind::Pi,
                "provider selection requires a native provider adapter"
            );
        }
        for operation in [
            &self.operations.attach,
            &self.operations.resume,
            &self.operations.fork,
            &self.operations.remove,
            &self.operations.unarchive,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(!operation.args.is_empty(), "empty operation command");
            if let Some(probe) = &operation.probe {
                probe.validate()?;
            }
        }
        if let Some(fork) = &self.operations.fork {
            ensure!(
                !self.kind.terminal_only(),
                "fork requires a verified native adapter"
            );
            validate_args(
                &fork.args,
                &["id", "short_id", "remote", "transcript", "new_id"],
            )?;
            require_operand(&fork.args, &["{id}", "{transcript}"])?;
        }
        validate_args(&self.commands.attach, &["id", "short_id", "remote"])?;
        validate_args(
            &self.commands.resume,
            &["id", "short_id", "remote", "transcript"],
        )?;
        validate_args(&self.commands.remove, &["id", "short_id"])?;
        if let Some(stop) = &self.operations.stop {
            validate_args(&stop.args, &["id", "short_id"])?;
            require_operand(&stop.args, &["{id}", "{short_id}"])?;
        }
        validate_args(&self.commands.unarchive, &["id"])?;
        ensure!(
            !self.operations.rename
                || matches!(self.kind, HarnessKind::Claude | HarnessKind::Codex),
            "rename requires a native rename adapter"
        );
        for kind in self.session_kinds.values().chain([&self.default_session]) {
            ensure!(
                kind.join == Join::Unavailable || kind.lifetime == Lifetime::Daemon,
                "a terminal session cannot advertise a native join"
            );
            ensure!(
                kind.join == Join::Unavailable || self.operations.attach.is_some(),
                "missing attach command"
            );
            ensure!(
                kind.stop != Stop::Remove || !self.commands.remove.is_empty(),
                "missing remove command"
            );
            ensure!(
                match kind.join {
                    Join::Unavailable => true,
                    Join::Attach => self.commands.resume_handler == Resume::BackgroundThenAttach,
                    Join::CodexRemote => self.commands.resume_handler == Resume::CodexRemote,
                },
                "join capability has no matching native handler"
            );
            ensure!(
                kind.stop != Stop::ForgetClient || self.discovery.handler == Native::Codex,
                "forgetting a client requires a native thread handler"
            );
        }
        match self.commands.resume_handler {
            Resume::BackgroundThenAttach => {
                validate_args(&self.commands.attach, &["id", "short_id"])?;
                validate_args(&self.commands.resume, &["id", "short_id"])?;
                if self.operations.attach.is_some() {
                    require_operand(&self.commands.attach, &["{id}", "{short_id}"])?;
                }
                if self.operations.resume.is_some() {
                    ensure!(
                        self.operations.attach.is_some(),
                        "background resume requires attach"
                    );
                    require_operand(&self.commands.resume, &["{id}", "{short_id}"])?;
                }
            }
            Resume::CodexRemote => {
                for operation in [&self.operations.attach, &self.operations.resume]
                    .into_iter()
                    .flatten()
                {
                    validate_args(&operation.args, &["id", "remote"])?;
                    require_operand(&operation.args, &["{id}"])?;
                    require_operand(&operation.args, &["{remote}"])?;
                }
                if let Some(launch) = &self.launch {
                    require_operand(&launch.remote, &["{remote}"])?;
                    require_operand(&launch.remote, &["{cwd}"])?;
                }
            }
            Resume::Transcript => {
                validate_args(&self.commands.resume, &["transcript"])?;
                if self.operations.resume.is_some() {
                    require_operand(&self.commands.resume, &["{transcript}"])?;
                }
                ensure!(
                    self.commands.attach.is_empty(),
                    "a transcript resume is not a live attach"
                );
            }
            Resume::SessionId => {
                validate_args(&self.commands.resume, &["id"])?;
                if self.operations.resume.is_some() {
                    require_operand(&self.commands.resume, &["{id}"])?;
                }
                ensure!(
                    self.commands.attach.is_empty(),
                    "session resume is not a live attach"
                );
            }
        }
        if !self.commands.remove.is_empty() {
            require_operand(&self.commands.remove, &["{id}", "{short_id}"])?;
        }
        if !self.commands.unarchive.is_empty() {
            ensure!(
                self.operations.resume.is_some(),
                "unarchive requires resume"
            );
            require_operand(&self.commands.unarchive, &["{id}"])?;
        }
        ensure!(
            self.input.empty_prompt != EmptyPrompt::Marker || !self.input.markers.is_empty(),
            "marker input needs a prompt marker"
        );
        let mut return_keys = HashSet::new();
        for binding in &self.input.return_to_list {
            ensure!(return_keys.insert(binding.key), "duplicate return key");
        }
        ensure!(
            self.input
                .return_to_list
                .iter()
                .any(|b| b.when == ReturnWhen::Always),
            "a harness needs an unconditional way back to the list"
        );
        ensure!(
            self.viewer.peek != Peek::ExistingDaemon || self.discovery.handler == Native::Codex,
            "existing-daemon peek needs a native daemon probe"
        );
        ensure!(
            self.viewer.peek == Peek::Unavailable || self.operations.attach.is_some(),
            "viewer peek requires an attach operation"
        );
        ensure!(
            self.viewer.retention != Retention::EvictLive || self.viewer.peek != Peek::Unavailable,
            "only a rejoinable viewer can be evicted"
        );
        if let Some(launch) = &self.launch {
            ensure!(
                (launch.identity == LaunchIdentity::BackgroundId)
                    == (launch.handler == LaunchHandler::ClaudeBackground),
                "launch identity and native lifetime disagree"
            );
            ensure!(
                launch.identity != LaunchIdentity::ReportedThread
                    || self.discovery.handler == Native::Codex,
                "reported-thread handover requires a native thread handler"
            );
        }
        let (native, launch, resume) = handlers(self.kind);
        ensure!(
            self.discovery.handler == native
                && self.transcript.handler == native
                && self
                    .launch
                    .as_ref()
                    .is_none_or(|operation| operation.handler == launch)
                && self.commands.resume_handler == resume,
            "native handler does not implement this harness"
        );
        ensure!(
            self.kind == HarnessKind::Claude
                || (self.execution.enforcement != Support::Supported
                    && self.execution.result != Support::Supported),
            "supervised support requires a native execution adapter"
        );
        Ok(())
    }

    /// The env variable that sends this harness to Bedrock, when its definition names one.
    pub fn bedrock_switch(&self) -> Option<&str> {
        self.launch
            .as_ref()
            .and_then(|launch| launch.bedrock.as_deref())
    }

    pub fn session(&self, kind: Option<&str>) -> &SessionKind {
        kind.and_then(|k| self.session_kinds.get(k))
            .unwrap_or(&self.default_session)
    }

    pub fn permits_peek(&self, session: &Session) -> bool {
        self.viewer.peek != Peek::Unavailable
            && self.session(session.kind.as_deref()).join != Join::Unavailable
            && !self
                .viewer
                .peek_blocked_states
                .iter()
                .any(|state| state.as_str() == session.state)
    }

    /// Resolve a row's original home before falling back to the launch default.
    pub fn session_home(&self, claude: &Path, session: &Session) -> PathBuf {
        if self.discovery.handler == Native::Codex
            && let Some(home) = session
                .transcript_path
                .as_deref()
                .and_then(crate::codex::home_of)
        {
            return home.to_owned();
        }
        self.home.resolve(claude)
    }
}

/// A variable in an environment as `ps` prints it. Read from the right, because the environment
/// follows the command line: a prompt that names a variable cannot claim another home.
fn env_value<'a>(env: &'a str, name: &str) -> Option<&'a str> {
    env.rsplit(' ')
        .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

impl Home {
    /// The home a process runs against, read from its own environment as `ps` prints it:
    /// space-separated `NAME=value` pairs after the command line. `None` when that environment
    /// names no user home, which is how an unreadable environment arrives.
    pub fn of_process(&self, env: &str) -> Option<PathBuf> {
        let user = PathBuf::from(env_value(env, "HOME")?);
        let provided = env_value(env, &spec(HarnessKind::Claude).home.env)
            .map_or_else(|| user.join(crate::fleet::CLAUDE_DIR), PathBuf::from);
        Some(self.resolve_with(&provided, &user, env_value(env, &self.env).map(OsStr::new)))
    }

    pub fn resolve(&self, claude: &Path) -> PathBuf {
        // An explicit Claude root is authoritative, including test roots.
        if matches!(self.default, HomeDefault::Provided) {
            return claude.to_owned();
        }
        self.resolve_with(
            claude,
            &dirs::home_dir().unwrap_or_default(),
            (!self.env.is_empty())
                .then(|| std::env::var_os(&self.env))
                .flatten()
                .as_deref(),
        )
    }

    /// Native overrides keep their path semantics; definitions state the default's base.
    pub fn resolve_with(&self, provided: &Path, user: &Path, value: Option<&OsStr>) -> PathBuf {
        if let HomeDefault::XdgData { path } = &self.default {
            return value
                .filter(|v| !v.is_empty())
                .map_or_else(|| user.join(".local/share"), PathBuf::from)
                .join(path);
        }
        value
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| match &self.default {
                HomeDefault::Provided => provided.to_owned(),
                HomeDefault::ProvidedParent { path } => {
                    provided.parent().unwrap_or(provided).join(path)
                }
                HomeDefault::User { path } => user.join(path),
                HomeDefault::XdgData { .. } => unreachable!(),
            })
    }

    /// Preserve the native meaning of home overrides when resuming a saved session. Naming the
    /// default home is not the same as leaving the variable alone: Claude Code reads its global
    /// configuration from `~/.claude.json` when `CLAUDE_CONFIG_DIR` is unset and from
    /// `$CLAUDE_CONFIG_DIR/.claude.json` when it is set, so passing the default would start the
    /// native session against an empty configuration and re-run onboarding.
    pub fn set_command_home(&self, command: &mut std::process::Command, home: &Path) {
        if self.env.is_empty() {
            return;
        }
        let user = dirs::home_dir().unwrap_or_default();
        let default = self.resolve_with(&user.join(crate::fleet::CLAUDE_DIR), &user, None);
        if home == default && std::env::var_os(&self.env).is_none_or(|v| v.is_empty()) {
            return;
        }
        let value = match &self.default {
            HomeDefault::XdgData { path } => home
                .ancestors()
                .nth(path.components().count())
                .unwrap_or(home),
            _ => home,
        };
        command.env(&self.env, value);
    }

    pub fn all(&self, claude: &Path) -> Vec<PathBuf> {
        let base = self.resolve(claude);
        let Some(siblings) = &self.siblings else {
            return vec![base];
        };
        if std::env::var_os(&self.env).is_some_and(|v| !v.is_empty()) {
            return vec![base];
        }
        let mut extra: Vec<_> = base
            .parent()
            .and_then(|p| std::fs::read_dir(p).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&siblings.prefix))
                    && p.join(&siblings.marker).is_file()
            })
            .collect();
        extra.sort();
        let mut out = vec![base];
        out.extend(extra);
        out
    }
}

impl Probe {
    fn validate(&self) -> Result<()> {
        if let Version::JsonLines { pointer: path } = &self.version {
            pointer(path)?;
        }
        ensure!(!self.args.is_empty(), "empty capability probe");
        if let Some(minimum) = &self.minimum_version {
            ensure!(
                version_number(minimum).is_some(),
                "invalid minimum native version"
            );
            ensure!(
                !matches!(self.version, Version::None),
                "minimum version requires version output"
            );
        }
        validate_args(&self.args, &[])
    }

    pub fn report(&self, success: bool, stdout: &str) -> Result<String> {
        ensure!(
            (!self.require_success || success) && self.contains.iter().all(|s| stdout.contains(s)),
            "{}",
            self.error
        );
        let version = match &self.version {
            Version::None => String::new(),
            Version::Text => stdout.trim().to_owned(),
            Version::JsonLines { pointer } => stdout
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .find_map(|v| v.pointer(pointer)?.as_str().map(str::to_owned))
                .unwrap_or_default(),
        };
        ensure!(
            matches!(self.version, Version::None) || !version.is_empty(),
            "{}: probe did not report a version",
            self.error
        );
        if let Some(minimum) = &self.minimum_version {
            ensure!(
                version_number(&version)
                    .zip(version_number(minimum))
                    .is_some_and(|(got, required)| got >= required),
                "{}: requires version {minimum} or later, reported {version}",
                self.error
            );
        }
        Ok(self.description.replace("{version}", &version))
    }
}

fn version_number(value: &str) -> Option<[u64; 3]> {
    let word = value.split_whitespace().next()?;
    let mut parts = word.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    parts.next().is_none().then_some([major, minor, patch])
}

/// Templates substitute complete argv entries, never shell fragments or lossy paths.
pub fn args(template: &[String], values: &[(&str, &OsStr)]) -> Result<Vec<OsString>> {
    template
        .iter()
        .map(|arg| {
            if let Some(key) = arg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                values
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_os_string())
                    .with_context(|| format!("missing command argument {key}"))
            } else {
                Ok(arg.into())
            }
        })
        .collect()
}

fn relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "expected a relative native path: {}",
        path.display()
    );
    Ok(())
}

fn environment_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.bytes().enumerate().all(|(i, b)| {
                b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
            }),
        "invalid environment variable name"
    );
    Ok(())
}

fn pointer(path: &str) -> Result<()> {
    ensure!(path.starts_with('/'), "invalid JSON pointer: {path}");
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            ensure!(
                matches!(chars.next(), Some('0' | '1')),
                "invalid JSON pointer escape: {path}"
            );
        }
    }
    Ok(())
}

fn require_operand(args: &[String], operands: &[&str]) -> Result<()> {
    ensure!(
        args.iter().any(|a| operands.contains(&a.as_str())),
        "command is missing operand {}",
        operands.join(" or ")
    );
    Ok(())
}

fn validate_args(args: &[String], allowed: &[&str]) -> Result<()> {
    for arg in args {
        ensure!(!arg.contains('\0'), "NUL in command argument");
        if arg.contains(['{', '}']) {
            let key = arg.strip_prefix('{').and_then(|s| s.strip_suffix('}'));
            ensure!(
                key.is_some_and(|k| allowed.contains(&k)),
                "unknown or partial argument placeholder: {arg}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
