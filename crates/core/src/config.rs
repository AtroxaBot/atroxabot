//! The user's settings in `~/.atroxabot/config.json`: the connections agents
//! think through, the voice providers, and how the window looked.
//!
//! The file holds API keys, so it is written atomically and, on Unix, with
//! mode `0600`. Keys a newer version of the app added are kept as they are.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// Name of the per-user data directory inside the home directory.
const DIR_NAME: &str = ".atroxabot";
/// Name of the configuration file inside [`DIR_NAME`].
const FILE_NAME: &str = "config.json";

/// What an agent thinks through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionKind {
    /// The Claude Code binary the user installed and signed in to, driven
    /// with the Claude Agent SDK's protocol. AtroxaBot never sees its sign-in.
    #[default]
    ClaudeAgent,
    /// Anthropic's Messages API, with an API key.
    ClaudeApi,
    /// Any OpenAI-compatible gateway (`OpenRouter`, Inworld, `OpenAI`, Ollama, ...).
    #[serde(rename = "openai")]
    OpenAi,
    /// The Codex CLI the user installed and signed in to, driven through
    /// its app server. AtroxaBot never sees its sign-in.
    Codex,
}

impl ConnectionKind {
    /// Every kind, in the order menus list them.
    pub const ALL: [Self; 4] = [Self::ClaudeAgent, Self::Codex, Self::ClaudeApi, Self::OpenAi];

    /// Whether it runs an agent the user installed and signed in to (Claude
    /// Code, Codex), which keeps the conversation and brings its own tools,
    /// rather than an API the app runs the agent loop over.
    #[must_use]
    pub fn is_account(self) -> bool {
        matches!(self, Self::ClaudeAgent | Self::Codex)
    }

    /// The name menus show. "Claude Agent" is the name Anthropic's branding
    /// guidelines ask for; the product is never presented as Claude Code.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeAgent => "Claude Agent",
            Self::ClaudeApi => "Claude API",
            Self::OpenAi => "OpenAI-compatible",
            Self::Codex => "Codex",
        }
    }
}

/// One saved way to reach a model.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    /// Stable identifier agents refer to.
    pub id: String,
    /// What it speaks.
    pub kind: ConnectionKind,
    /// Name shown in menus.
    pub name: String,
    /// API root, e.g. `https://openrouter.ai/api/v1`; unused by [`ConnectionKind::ClaudeAgent`].
    pub base_url: String,
    /// The API key; only ever sent to `base_url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// For a Claude account: the folder Claude Code keeps this account's
    /// sign-in, settings and history in (its `CLAUDE_CONFIG_DIR`). Empty for
    /// Claude Code's own default folder, the account it already uses. For a
    /// Codex account, the same of Codex (its `CODEX_HOME`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub config_dir: String,
}

impl Connection {
    /// The id of the built-in Claude Agent connection, which always exists.
    pub const CLAUDE_AGENT: &'static str = "claude-agent";

    /// The built-in connection to the user's own Claude Code installation.
    #[must_use]
    pub fn claude_agent() -> Self {
        Self {
            id: Self::CLAUDE_AGENT.to_owned(),
            kind: ConnectionKind::ClaudeAgent,
            name: "Claude".to_owned(),
            ..Self::default()
        }
    }

    /// Another Claude account, kept apart from the first: Claude Code is
    /// started for it with a configuration folder of its own under
    /// `~/.atroxabot/claude/`, which is how Anthropic documents staying
    /// signed in to several accounts at once. The user signs in to it with
    /// Claude Code itself; the app never sees that sign-in.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn claude_account(name: &str) -> Result<Self> {
        let id = crate::session::new_id();
        let config_dir = Config::dir()?.join("claude").join(&id).display().to_string();
        Ok(Self { id, kind: ConnectionKind::ClaudeAgent, name: name.to_owned(), config_dir, ..Self::default() })
    }

    /// The Codex account the user already has: whoever `codex` is signed
    /// in as in their terminal, with Codex's own settings.
    #[must_use]
    pub fn codex(name: &str) -> Self {
        Self { id: crate::session::new_id(), kind: ConnectionKind::Codex, name: name.to_owned(), ..Self::default() }
    }

    /// Another Codex account, kept apart from the first: Codex is started
    /// for it with a home folder of its own under `~/.atroxabot/codex/`
    /// (`CODEX_HOME`), where it keeps that account's sign-in, settings and
    /// conversations. The user signs in to it with Codex itself.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn codex_account(name: &str) -> Result<Self> {
        let id = crate::session::new_id();
        let config_dir = Config::dir()?.join("codex").join(&id).display().to_string();
        Ok(Self { id, kind: ConnectionKind::Codex, name: name.to_owned(), config_dir, ..Self::default() })
    }

    /// The host of its address, for showing where an API connection goes.
    #[must_use]
    pub fn host(&self) -> &str {
        let rest = self.base_url.split_once("://").map_or(self.base_url.as_str(), |(_, rest)| rest);
        rest.split('/').next().unwrap_or_default()
    }
}

/// An MCP server agents can use tools of.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    /// Its name, which tool names carry.
    pub name: String,
    /// For a local server: the command line that starts it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// For a remote server: its address.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// `NAME=value` lines: the environment of a local server, or the
    /// headers of a remote one. May hold secrets.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub vars: String,
    /// Whether agents get its tools.
    pub enabled: bool,
}

impl McpServer {
    /// The `NAME=value` pairs of [`McpServer::vars`], blank and nameless lines dropped.
    #[must_use]
    pub fn pairs(&self) -> Vec<(String, String)> {
        let pair = |line: &str| {
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
        };
        self.vars.lines().filter_map(pair).collect()
    }
}

impl std::fmt::Debug for McpServer {
    // Hand-written so tokens in its variables never end up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("url", &self.url)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Connection {
    // Hand-written so the key never ends up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("has_key", &self.api_key.is_some())
            .finish_non_exhaustive()
    }
}

/// The voice providers' keys and the devices calls use.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// The provider calls use unless an agent names another: `fish` or `elevenlabs`.
    pub provider: String,
    /// Fish Audio's API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fish_key: Option<String>,
    /// `ElevenLabs`' API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevenlabs_key: Option<String>,
    /// Microphone by name; empty for the system's default.
    pub input_device: String,
    /// Speakers by name; empty for the system's default.
    pub output_device: String,
    /// Speech-to-text language hint such as `nl`; empty to detect it.
    pub language: String,
}

impl std::fmt::Debug for VoiceSettings {
    // Hand-written so the keys never end up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceSettings")
            .field("provider", &self.provider)
            .field("has_fish_key", &self.fish_key.is_some())
            .field("has_elevenlabs_key", &self.elevenlabs_key.is_some())
            .finish_non_exhaustive()
    }
}

/// User configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Saved API connections; the Claude Agent one is built in, not listed.
    pub connections: Vec<Connection>,
    /// Voice providers and devices.
    pub voice: VoiceSettings,
    /// Colour scheme key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// How replies show reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_view: Option<String>,
    /// The window's size and state, e.g. `1200x800 maximized`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Interface zoom in percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom: Option<String>,
    /// The agent that was open last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_agent: Option<String>,
    /// MCP servers agents can use.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServer>,
    /// Folders sessions worked in lately, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_folders: Vec<String>,
    /// The user took the built-in Claude account (Claude Code's own sign-in) off the list.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_claude_agent: bool,
    /// What the user calls the built-in Claude account, if not "Claude".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_name: Option<String>,
    /// Whether the side panel (the session's usage and its folder's git) is open.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub panel: bool,
    /// Whether the accounts' email addresses are kept off the screen (for
    /// streaming or sharing it): an account shows its plan only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_emails: bool,
    /// Whether Claude accounts stop guessing, after a reply, what the user
    /// writes next (shown in the message box, taken with Tab). They guess
    /// unless this is set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_suggestions: bool,
    /// The sessions in the order the user dragged them into, by id. One
    /// not named here (started since) comes before them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_order: Vec<String>,
    /// What closing the window does: `background` (keep running in the tray
    /// or Dock), `minimize` or `quit`; the platform's default if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close: Option<String>,
    /// What happens when an account's usage limit stops a turn: `wait` (go
    /// on once it lifts), `switch` (go on with another account of the same
    /// kind that has usage left) or `stop`; `wait` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_of_limits: Option<String>,
    /// The repositories the app made worktrees in: what it tidies after
    /// their sessions are gone (see [`crate::worktree`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub worktree_repos: Vec<String>,
    /// How many days a session may go unused before its worktree, when
    /// nothing in it is uncommitted, is taken away: a number, or `never`.
    /// Three days when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_idle: Option<String>,
    /// Settings a newer version wrote, kept as they are.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Config {
    /// Returns `~/.atroxabot`, the directory holding all local app data.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn dir() -> Result<PathBuf> {
        std::env::home_dir()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|home| home.join(DIR_NAME))
            .ok_or(Error::NoHomeDir)
    }

    /// Loads the configuration, returning defaults if the file does not exist.
    ///
    /// # Errors
    /// I/O failures other than "not found", or a malformed file.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::dir()?.join(FILE_NAME))
    }

    /// Loads the configuration from an explicit path.
    ///
    /// # Errors
    /// See [`Config::load`].
    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the configuration atomically to `~/.atroxabot/config.json`.
    ///
    /// # Errors
    /// Any I/O failure while creating the directory or writing the file.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::dir()?.join(FILE_NAME))
    }

    /// Writes the configuration atomically to an explicit path.
    ///
    /// # Errors
    /// See [`Config::save`].
    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_private(path, &serde_json::to_vec_pretty(self)?)
    }

    /// The connection with `id`: a saved one, or the built-in Claude Agent
    /// (unless the user took it off the list).
    #[must_use]
    pub fn connection(&self, id: &str) -> Option<Connection> {
        if id == Connection::CLAUDE_AGENT {
            return (!self.hide_claude_agent).then(|| self.built_in());
        }
        self.connections.iter().find(|c| c.id == id).cloned()
    }

    /// The built-in Claude account, under the name the user gave it.
    fn built_in(&self) -> Connection {
        let mut account = Connection::claude_agent();
        if let Some(name) = self.claude_name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            name.clone_into(&mut account.name);
        }
        account
    }

    /// Calls the connection `id` (a saved one, or the built-in Claude
    /// account) `name` from now on. An empty name changes nothing.
    pub fn rename_connection(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if id == Connection::CLAUDE_AGENT {
            self.claude_name = Some(name.to_owned());
        } else if let Some(connection) = self.connections.iter_mut().find(|c| c.id == id) {
            name.clone_into(&mut connection.name);
        }
    }

    /// Remembers that a session worked in `folder`: it moves to the front
    /// of the recent folders, of which a handful are kept.
    pub fn use_folder(&mut self, folder: &str) {
        self.recent_folders.retain(|recent| recent != folder);
        self.recent_folders.insert(0, folder.to_owned());
        self.recent_folders.truncate(8);
    }

    /// How many seconds a session may go unused before its clean worktree
    /// is taken away; `None` to keep it for as long as the session is there.
    #[must_use]
    pub fn worktree_idle_seconds(&self) -> Option<u64> {
        const DAY: u64 = 86_400;
        match self.worktree_idle.as_deref().map(str::trim) {
            Some("never") => None,
            Some(days) => Some(days.parse::<u64>().map_or(3, |days| days.clamp(1, 3650)) * DAY),
            None => Some(3 * DAY),
        }
    }

    /// Every connection an agent can use, the built-in one (if the user kept it) first.
    #[must_use]
    pub fn all_connections(&self) -> Vec<Connection> {
        let built_in = (!self.hide_claude_agent).then(|| self.built_in());
        built_in.into_iter().chain(self.connections.iter().cloned()).collect()
    }
}

/// Atomically replaces `path` with `bytes`, readable only by the user.
///
/// Writes a sibling temporary file and renames it over the original, so a
/// crash mid-write never leaves a truncated file behind. The temporary name
/// is unique to this process and write, so two writers never share one. On
/// Unix the file is created `0600` inside a `0700` directory, which is synced
/// after the rename; on Windows the profile ACLs apply.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent();
    if let Some(dir) = dir {
        create_private_dir(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}-{}.tmp", std::process::id(), WRITES.fetch_add(1, Ordering::Relaxed)));
    let tmp = PathBuf::from(tmp);
    let written = private_file(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    #[cfg(unix)]
    if let Some(dir) = dir {
        // Makes the rename itself durable; best effort, the data is safe.
        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Takes the lock named `name` in `~/.atroxabot/` that keeps a second copy of
/// the app off the same data: `Ok(None)` when another process holds it.
/// The lock lasts while the returned file is open, and the OS releases it
/// when the process ends, so a crash never leaves it stuck.
///
/// # Errors
/// No home directory, or the lock file cannot be opened.
pub fn lock_instance(name: &str) -> Result<Option<fs::File>> {
    let dir = Config::dir()?;
    create_private_dir(&dir)?;
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_unknown_keys() {
        let text = r#"{"connections":[{"id":"a","kind":"openai","name":"OpenRouter","base_url":"https://openrouter.ai/api/v1","api_key":"sk-or"}],"theme":"black","future":{"x":1}}"#;
        let config: Config = serde_json::from_str(text).unwrap();
        assert_eq!(config.connections[0].kind, ConnectionKind::OpenAi);
        assert_eq!(config.extra["future"]["x"], 1);
        let again: Config = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(again, config);
        assert_eq!(config.connection("claude-agent").unwrap().kind, ConnectionKind::ClaudeAgent);
        assert_eq!(config.all_connections().len(), 2);
        assert_eq!(config.connections[0].host(), "openrouter.ai");

        let mut config = Config::default();
        for folder in ["a", "b", "a"] {
            config.use_folder(folder);
        }
        assert_eq!(config.recent_folders, ["a", "b"], "newest first, once each");
        let idle = |setting: Option<&str>| {
            Config { worktree_idle: setting.map(str::to_owned), ..Config::default() }.worktree_idle_seconds()
        };
        assert_eq!(
            [idle(None), idle(Some("7")), idle(Some("never")), idle(Some("soon")), idle(Some("0"))],
            [Some(259_200), Some(604_800), None, Some(259_200), Some(86_400)]
        );
        let server = McpServer { vars: "TOKEN = s3cret\n\n=x\nplain\nA=b=c".into(), ..McpServer::default() };
        assert_eq!(server.pairs(), [("TOKEN".to_owned(), "s3cret".to_owned()), ("A".to_owned(), "b=c".to_owned())]);
        assert!(!format!("{server:?}").contains("s3cret"));
    }

    #[test]
    fn providers_are_renamed_the_built_in_one_too() {
        let mut config = Config::default();
        config.connections.push(Connection { id: "or".into(), name: "OpenRouter".into(), ..Connection::default() });
        config.rename_connection(Connection::CLAUDE_AGENT, " Personal ");
        config.rename_connection("or", "Router");
        config.rename_connection("or", "   ");
        config.rename_connection("gone", "Nobody");
        let names: Vec<String> = config.all_connections().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Personal", "Router"]);
        assert_eq!(config.connection(Connection::CLAUDE_AGENT).map(|c| c.name).as_deref(), Some("Personal"));
        assert_eq!(Config::default().all_connections()[0].name, "Claude");
        // Taken off the list, the built-in account is no provider any more.
        config.hide_claude_agent = true;
        assert_eq!(config.all_connections().len(), 1);
        assert!(config.connection(Connection::CLAUDE_AGENT).is_none() && config.connection("or").is_some());
    }

    #[test]
    fn keys_never_show_in_debug_output() {
        let config = Config {
            connections: vec![Connection { api_key: Some("k3y".into()), ..Connection::default() }],
            voice: VoiceSettings { fish_key: Some("f1sh".into()), ..VoiceSettings::default() },
            ..Config::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("k3y") && !debug.contains("f1sh"));
    }

    #[test]
    fn saves_atomically() {
        let dir = std::env::temp_dir().join(format!("atroxabot-config-{}", std::process::id()));
        let path = dir.join("config.json");
        let config = Config { theme: Some("white".into()), ..Config::default() };
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
        assert_eq!(Config::load_from(&dir.join("missing.json")).unwrap(), Config::default());
        fs::remove_dir_all(dir).unwrap();
    }
}
