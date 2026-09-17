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

/// Registration order is the composer's cycle order.
pub fn known() -> &'static [HarnessKind] {
    &KINDS
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub launch: Launch,
    pub probe: Probe,
    pub default_session: SessionKind,
    pub session_kinds: BTreeMap<String, SessionKind>,
    pub commands: Commands,
    pub execution: Execution,
    pub input: Input,
    pub viewer: Viewer,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Home {
    pub env: String,
    /// Claude receives an explicit root from its caller. Other defaults remain siblings of it.
    pub sibling: Option<PathBuf>,
    pub siblings: Option<Siblings>,
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
    pub handler: Native,
    pub registry: Option<PathBuf>,
    pub daemon: Option<Daemon>,
    pub exclude_subcommands: Vec<String>,
    pub process: Option<String>,
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
                let mut words = line.command.split_whitespace();
                words
                    .next()
                    .and_then(|name| Path::new(name).file_name())
                    .is_some_and(|name| name == program.as_str())
                    && !words.next().is_some_and(|arg| {
                        self.exclude_subcommands
                            .iter()
                            .any(|excluded| excluded == arg)
                    })
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub handler: StateHandler,
    pub rules: Vec<StateRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateHandler {
    ClaudeRegistry,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Native {
    Claude,
    Codex,
    Pi,
}

impl Native {
    pub fn sessions(self, home: &Path) -> Result<Vec<Session>> {
        match self {
            Self::Claude => crate::fleet::sessions(home),
            Self::Codex => crate::codex::sessions(home),
            Self::Pi => crate::pi::sessions(home),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
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
    pub headline: Headline,
    pub sources: Vec<TextSource>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Headline {
    First,
    Last,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSource {
    pub when: BTreeMap<String, String>,
    pub unless: Vec<String>,
    pub path: String,
    pub shape: TextShape,
    pub types: Vec<String>,
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
        let mut lines = self
            .parts(event, false)
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRoot {
    pub path: PathBuf,
    /// Number of directory levels below the root; null means recursive.
    pub depth: Option<usize>,
    pub archived: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statusline {
    pub directory: PathBuf,
    pub window_pointer: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub handler: LaunchHandler,
    pub identity: LaunchIdentity,
    pub session_kind: Option<String>,
    pub prefix: Vec<String>,
    pub remote: Vec<String>,
    pub model: Option<Model>,
    pub prompt: Vec<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchHandler {
    ClaudeBackground,
    CodexRemote,
    Terminal,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub source: ModelSource,
    pub flag: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Claude,
    Codex,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub args: Vec<String>,
    pub require_success: bool,
    pub contains: Vec<String>,
    pub version: Version,
    pub error: String,
    pub description: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum Version {
    None,
    Text,
    JsonLines { pointer: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKind {
    pub join: Join,
    pub stop: Stop,
    pub lifetime: Lifetime,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub enforcement: Support,
    pub result: Support,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub return_to_list: Vec<ReturnBinding>,
    pub empty_prompt: EmptyPrompt,
    pub markers: String,
    pub ignore_braille: bool,
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
#[serde(deny_unknown_fields)]
pub struct Viewer {
    pub peek: Peek,
    pub peek_blocked_states: Vec<StateWord>,
    pub retention: Retention,
    pub input_alignment: InputAlignment,
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
}

impl HarnessSpec {
    pub fn parse(yaml: &str) -> Result<Self> {
        let spec: Self = serde_yaml::from_str(yaml).context("reading harness definition")?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported harness definition version {}",
            self.version
        );
        ensure!(
            self.name == self.kind.to_string(),
            "harness name and kind disagree"
        );
        ensure!(!self.icon.trim().is_empty(), "harness icon is empty");
        ensure!(
            !self.home.env.is_empty()
                && self
                    .home
                    .env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !self.home.env.as_bytes()[0].is_ascii_digit(),
            "invalid home environment variable"
        );
        if let Some(path) = &self.home.sibling {
            relative(path)?;
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
        ensure!(
            (self.kind == HarnessKind::Claude)
                == (self.state.handler == StateHandler::ClaudeRegistry),
            "state handler does not implement this harness"
        );
        ensure!(
            (self.state.handler == StateHandler::ClaudeRegistry) == self.state.rules.is_empty(),
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
        ensure!(!self.transcript.roots.is_empty(), "no transcript roots");
        ensure!(
            self.transcript.roots.iter().filter(|r| !r.archived).count() == 1,
            "exactly one live transcript root is required"
        );
        let mut roots = HashSet::new();
        for root in &self.transcript.roots {
            relative(&root.path)?;
            ensure!(roots.insert(&root.path), "duplicate transcript root");
        }
        if let Some(statusline) = &self.transcript.statusline {
            relative(&statusline.directory)?;
            pointer(&statusline.window_pointer)?;
        }
        for message in [
            &self.transcript.messages.user,
            &self.transcript.messages.assistant,
        ] {
            if let Some(id) = &message.id {
                pointer(id)?;
            }
            ensure!(!message.sources.is_empty(), "missing message sources");
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
        if let Version::JsonLines { pointer: path } = &self.probe.version {
            pointer(path)?;
        }
        ensure!(!self.probe.args.is_empty(), "empty capability probe");
        validate_args(&self.probe.args, &[])?;
        validate_args(&self.launch.prefix, &[])?;
        validate_args(&self.launch.remote, &["remote", "cwd"])?;
        validate_args(&self.launch.prompt, &["prompt"])?;
        ensure!(
            self.launch.prompt.last().map(String::as_str) == Some("{prompt}")
                && self
                    .launch
                    .prompt
                    .iter()
                    .filter(|a| *a == "{prompt}")
                    .count()
                    == 1
                && self.launch.prompt.iter().any(|a| a == "--"),
            "launch must pass one prompt after --"
        );
        if let Some(model) = &self.launch.model {
            ensure!(
                model.flag.starts_with('-') && !model.flag.contains('{'),
                "invalid model flag"
            );
        }
        validate_args(&self.commands.attach, &["id", "short_id", "remote"])?;
        validate_args(
            &self.commands.resume,
            &["id", "short_id", "remote", "transcript"],
        )?;
        validate_args(&self.commands.remove, &["id", "short_id"])?;
        validate_args(&self.commands.unarchive, &["id"])?;
        ensure!(
            !self.commands.resume.is_empty(),
            "missing history resume command"
        );
        for kind in self.session_kinds.values().chain([&self.default_session]) {
            ensure!(
                kind.join == Join::Unavailable || kind.lifetime == Lifetime::Daemon,
                "a terminal session cannot advertise a native join"
            );
            ensure!(
                kind.join != Join::Attach || !self.commands.attach.is_empty(),
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
                require_operand(&self.commands.attach, &["{id}", "{short_id}"])?;
                require_operand(&self.commands.resume, &["{id}", "{short_id}"])?;
            }
            Resume::CodexRemote => {
                for command in [&self.commands.attach, &self.commands.resume] {
                    validate_args(command, &["id", "remote"])?;
                    require_operand(command, &["{id}"])?;
                    require_operand(command, &["{remote}"])?;
                }
                require_operand(&self.launch.remote, &["{remote}"])?;
                require_operand(&self.launch.remote, &["{cwd}"])?;
            }
            Resume::Transcript => {
                validate_args(&self.commands.resume, &["transcript"])?;
                require_operand(&self.commands.resume, &["{transcript}"])?;
                ensure!(
                    self.commands.attach.is_empty(),
                    "a transcript resume is not a live attach"
                );
            }
        }
        if !self.commands.remove.is_empty() {
            require_operand(&self.commands.remove, &["{id}", "{short_id}"])?;
        }
        if !self.commands.unarchive.is_empty() {
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
            self.viewer.retention != Retention::EvictLive || self.viewer.peek != Peek::Unavailable,
            "only a rejoinable viewer can be evicted"
        );
        ensure!(
            (self.launch.identity == LaunchIdentity::BackgroundId)
                == (self.launch.handler == LaunchHandler::ClaudeBackground),
            "launch identity and native lifetime disagree"
        );
        ensure!(
            self.launch.identity != LaunchIdentity::ReportedThread
                || self.discovery.handler == Native::Codex,
            "reported-thread handover requires a native thread handler"
        );
        // Each handler interprets a native protocol. A YAML claim cannot create an implementation.
        let (native, launch, resume) = match self.kind {
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
        };
        ensure!(
            self.discovery.handler == native
                && self.transcript.handler == native
                && self.launch.handler == launch
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

impl Home {
    pub fn resolve(&self, claude: &Path) -> PathBuf {
        // An explicit Claude root is authoritative, including test roots.
        if self.sibling.is_none() {
            return claude.to_owned();
        }
        self.resolve_with(claude, std::env::var_os(&self.env).as_deref())
            .expect("validated sibling default")
    }

    /// Pure resolver for fixture checks; a relative or empty override keeps native semantics.
    pub fn resolve_with(&self, claude: &Path, value: Option<&OsStr>) -> Option<PathBuf> {
        value
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                self.sibling.as_ref().map(|p| {
                    let mut parts = p.components();
                    let first = parts.next().expect("validated sibling path");
                    claude
                        .with_file_name(first.as_os_str())
                        .join(parts.as_path())
                })
            })
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
        Ok(self.description.replace("{version}", &version))
    }
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
