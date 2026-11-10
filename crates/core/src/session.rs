//! Sessions: the conversations of an agent, one JSON file each in
//! `~/.atroxabot/sessions/`.
//!
//! Next to them, `.index.json` holds a [`SessionSummary`] per session so the
//! sidebar loads without reading any message bodies. The index is a cache:
//! [`SessionStore::list`] reconciles it against the files on disk (a
//! directory listing plus modification times, no reads) and re-reads only
//! sessions that are new or changed since the index was written, so a crash
//! between the two writes of a save heals itself.
//!
//! Files are written atomically with the same private permissions as the
//! config. Fields added later must be `#[serde(default)]` so older files keep
//! loading. Each file records the [`Session::VERSION`] that wrote it; one
//! from a newer version of the app is listed but never opened, since saving
//! it here would drop what this version does not know.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, write_private};
use crate::error::{Error, Result};
use crate::responses::Usage;

/// Name of the index file. The leading dot keeps it out of session scans,
/// since `.` is not a valid id character.
const INDEX_FILE: &str = ".index.json";

/// One conversation with an agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// The [`Session::VERSION`] that wrote the file.
    pub version: u32,
    /// File-name-safe identifier.
    pub id: String,
    /// The agent it belongs to.
    pub agent: String,
    /// Shown in the sidebar; taken from the first prompt until renamed.
    pub title: String,
    /// Creation time, seconds since the Unix epoch.
    pub created: u64,
    /// Last change, seconds since the Unix epoch.
    pub updated: u64,
    /// The folder it works in; empty for the user's home directory.
    pub workdir: String,
    /// The Claude Agent's own session id, to resume its context; empty for
    /// API engines, whose context is rebuilt from `entries`.
    pub remote: String,
    /// The Claude account (a connection's id) `remote` belongs to: another
    /// account keeps its own history and cannot resume it.
    pub account: String,
    /// How many tokens the model's context holds, as the engine last said; 0 when unknown.
    pub window: u64,
    /// What was said and done, oldest first.
    pub entries: Vec<Entry>,
}

/// One step of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// What the user wrote or said.
    User {
        /// Their words.
        text: String,
        /// Context the app adds before their words for the model only
        /// (that a voice call began or ended); never shown.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        /// Files sent along, by path: pictures go to the model as content,
        /// the rest are named to it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    /// One response of the model: text, reasoning and tool calls with results.
    Reply(Reply),
    /// The engine summarised everything before this point; requests start here.
    Summary {
        /// The summary the model continues from.
        text: String,
    },
    /// A line the app shows between messages (the Claude Agent summarised
    /// its own context); no model is sent it.
    Note {
        /// What it says.
        text: String,
    },
}

/// One response of the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    /// What it contained, in order.
    pub blocks: Vec<Block>,
    /// The Messages API's content blocks exactly as they arrived (thinking
    /// signatures, compaction blocks), replayed unchanged so Claude keeps its
    /// reasoning between tool calls. Empty for other engines.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<Value>,
    /// The model that wrote it.
    pub model: String,
    /// What it used.
    pub usage: Usage,
    /// Its cost in USD, when the engine reports or can price it.
    pub cost: f64,
    /// Why it ended badly (an error, a refusal, cut off); empty when it finished.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub problem: String,
    /// When it ended, in Unix seconds; 0 for a reply saved before replies were dated.
    pub at: u64,
}

/// One piece of a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Visible answer text (Markdown).
    Text {
        /// The text.
        text: String,
    },
    /// The model's reasoning, as far as the engine shows it.
    Thinking {
        /// The reasoning.
        text: String,
    },
    /// A tool call and, once it ran, its result.
    Tool(ToolUse),
}

/// A tool call and its outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolUse {
    /// Identifier the result answers.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON object in text form.
    pub input: String,
    /// What it returned; `None` while it runs or if it never ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Whether the tool failed or was refused.
    pub is_error: bool,
    /// What the subagent this call started said and did, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sub: Vec<Block>,
    /// For a subagent: what it is doing now, or how it ended.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub activity: String,
}

/// One thing on the agent's task list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Todo {
    /// What the agent's tools know it by.
    pub id: String,
    /// What is to be done.
    pub subject: String,
    /// How it reads while it is being done ("Fixing the bug"); may be empty.
    pub doing: String,
    /// How far along it is.
    pub state: TodoState,
}

/// How far along a [`Todo`] is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TodoState {
    /// Not started.
    #[default]
    Pending,
    /// Being done now.
    Active,
    /// Done.
    Done,
}

impl TodoState {
    /// Reads a task tool's `status`; `None` for one that is not a state (`deleted`).
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::Active),
            "completed" => Some(Self::Done),
            _ => None,
        }
    }
}

impl Session {
    /// The current file format.
    pub const VERSION: u32 = 1;

    /// The agent's task list as its tool calls left it: Claude Code's
    /// `TaskCreate` and `TaskUpdate`, or a whole list at once from
    /// `TodoWrite`. A task added once every earlier one is done starts a
    /// new list.
    #[must_use]
    pub fn todos(&self) -> Vec<Todo> {
        let mut todos: Vec<Todo> = Vec::new();
        let calls = self.replies().flat_map(|reply| &reply.blocks).filter_map(|block| match block {
            Block::Tool(call)
                if !call.is_error && matches!(call.name.as_str(), "TaskCreate" | "TaskUpdate" | "TodoWrite") =>
            {
                Some(call)
            }
            _ => None,
        });
        for call in calls {
            let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
            let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            match call.name.as_str() {
                "TodoWrite" => {
                    let listed = input.get("todos").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                    todos = (1..)
                        .zip(listed)
                        .map(|(number, todo)| Todo {
                            id: number.to_string(),
                            subject: text(todo, "content"),
                            doing: text(todo, "activeForm"),
                            state: TodoState::from_key(&text(todo, "status")).unwrap_or_default(),
                        })
                        .filter(|todo| !todo.subject.is_empty())
                        .collect();
                }
                "TaskCreate" => {
                    // Its number is in the answer ("Task #3 created successfully: …"), so it counts once answered.
                    let Some(output) = &call.output else { continue };
                    let named = output.split_once('#').and_then(|(_, rest)| rest.split_whitespace().next());
                    if todos.iter().all(|todo| todo.state == TodoState::Done) {
                        todos.clear();
                    }
                    todos.push(Todo {
                        id: named.map_or_else(|| (todos.len() + 1).to_string(), str::to_owned),
                        subject: text(&input, "subject"),
                        doing: text(&input, "activeForm"),
                        state: TodoState::Pending,
                    });
                }
                _ => {
                    let (id, status) = (text(&input, "taskId"), text(&input, "status"));
                    if status == "deleted" {
                        todos.retain(|todo| todo.id != id);
                    }
                    let Some(todo) = todos.iter_mut().find(|todo| todo.id == id) else { continue };
                    todo.state = TodoState::from_key(&status).unwrap_or(todo.state);
                    for (key, field) in [("subject", &mut todo.subject), ("activeForm", &mut todo.doing)] {
                        if let Some(new) = input.get(key).and_then(Value::as_str) {
                            new.clone_into(field);
                        }
                    }
                }
            }
        }
        todos
    }

    /// A new, empty session of `agent`.
    #[must_use]
    pub fn new(agent: &str) -> Self {
        let now = unix_now();
        Self {
            version: Self::VERSION,
            id: new_id(),
            agent: agent.to_owned(),
            created: now,
            updated: now,
            ..Self::default()
        }
    }

    /// The folder it works in: its own, or the home directory.
    #[must_use]
    pub fn folder(&self) -> PathBuf {
        let chosen = self.workdir.trim();
        if chosen.is_empty() {
            std::env::home_dir().filter(|p| !p.as_os_str().is_empty()).unwrap_or_default()
        } else {
            PathBuf::from(chosen)
        }
    }

    /// What was said so far, as plain text of at most about `budget` bytes
    /// (the latest kept): what an engine that never saw the conversation is
    /// told when it takes it over.
    #[must_use]
    pub fn digest(&self, budget: usize) -> String {
        let mut lines = Vec::new();
        let mut used = 0;
        for entry in self.entries.iter().rev() {
            let line = match entry {
                Entry::User { text, .. } => format!("User: {}", text.trim()),
                Entry::Summary { text } => format!("Earlier, in summary: {}", text.trim()),
                Entry::Note { .. } => continue,
                Entry::Reply(reply) => {
                    let said: Vec<&str> = reply
                        .blocks
                        .iter()
                        .filter_map(|block| if let Block::Text { text } = block { Some(text.trim()) } else { None })
                        .filter(|text| !text.is_empty())
                        .collect();
                    if said.is_empty() {
                        continue;
                    }
                    format!("You: {}", said.join("\n"))
                }
            };
            used += line.len();
            if used > budget && !lines.is_empty() {
                break;
            }
            lines.push(line);
        }
        lines.reverse();
        lines.join("\n\n")
    }

    /// Total cost in USD of every reply.
    #[must_use]
    pub fn cost(&self) -> f64 {
        self.replies().map(|r| r.cost).sum()
    }

    /// Total tokens in and out of every reply.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.replies().map(|r| r.usage.input_tokens.saturating_add(r.usage.output_tokens)).fold(0, u64::saturating_add)
    }

    /// The prompt and output tokens of the latest reply: how full the context is.
    #[must_use]
    pub fn context_tokens(&self) -> u64 {
        // A reply still being written has no counts yet: the one before it says.
        let sizes = self.replies().rev().map(|r| r.usage.input_tokens.saturating_add(r.usage.output_tokens));
        sizes.into_iter().find(|size| *size > 0).unwrap_or(0)
    }

    fn replies(&self) -> impl DoubleEndedIterator<Item = &Reply> {
        self.entries.iter().filter_map(|e| if let Entry::Reply(reply) = e { Some(reply) } else { None })
    }

    /// The tool call `id`, wherever it is: in a reply, or in what a subagent did.
    pub fn find_tool(&mut self, id: &str) -> Option<&mut ToolUse> {
        fn within<'a>(blocks: &'a mut [Block], id: &str) -> Option<&'a mut ToolUse> {
            for block in blocks.iter_mut().rev() {
                if let Block::Tool(call) = block {
                    if call.id == id {
                        return Some(call);
                    }
                    if let Some(found) = within(&mut call.sub, id) {
                        return Some(found);
                    }
                }
            }
            None
        }
        self.entries.iter_mut().rev().find_map(|entry| match entry {
            Entry::Reply(reply) => within(&mut reply.blocks, id),
            _ => None,
        })
    }

    /// Sends `files` along with the latest prompt.
    pub fn attach(&mut self, attached: &[String]) {
        if let Some(Entry::User { files, .. }) = self.entries.last_mut() {
            files.extend_from_slice(attached);
        }
    }

    /// The first words of the latest answer or prompt, for the sidebar.
    #[must_use]
    pub fn preview(&self) -> String {
        let text = self.entries.iter().rev().find_map(|entry| match entry {
            Entry::User { text, .. } => Some(text.as_str()),
            Entry::Reply(reply) => reply.blocks.iter().rev().find_map(|b| match b {
                Block::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            }),
            Entry::Summary { .. } | Entry::Note { .. } => None,
        });
        text.unwrap_or_default().split_whitespace().take(24).collect::<Vec<_>>().join(" ").chars().take(120).collect()
    }

    /// Every line of text in the session, for searching.
    fn lines(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().flat_map(|entry| -> Vec<&str> {
            match entry {
                Entry::User { text, .. } | Entry::Summary { text } => text.lines().collect(),
                Entry::Note { .. } => Vec::new(),
                Entry::Reply(reply) => reply
                    .blocks
                    .iter()
                    .filter_map(|b| if let Block::Text { text } = b { Some(text.lines()) } else { None })
                    .flatten()
                    .collect(),
            }
        })
    }

    /// The entry for the sidebar and the index.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            agent: self.agent.clone(),
            title: self.title.clone(),
            preview: self.preview(),
            created: self.created,
            updated: self.updated,
            cost: self.cost(),
            tokens: self.tokens(),
        }
    }
}

/// What the sidebar shows of a session, without its messages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// File-name-safe identifier.
    pub id: String,
    /// The agent it belongs to.
    pub agent: String,
    /// Its title.
    pub title: String,
    /// The first words of its latest message.
    pub preview: String,
    /// Creation time, seconds since the Unix epoch.
    pub created: u64,
    /// Last change, seconds since the Unix epoch.
    pub updated: u64,
    /// Total cost in USD.
    pub cost: f64,
    /// Total tokens in and out.
    pub tokens: u64,
}

/// A session whose messages contain the search text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHit {
    /// Session id.
    pub session: String,
    /// The agent it belongs to.
    pub agent: String,
    /// Session title.
    pub title: String,
    /// The matching line, shortened around the match.
    pub snippet: String,
    /// Last change, for ordering.
    pub updated: u64,
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A fresh, file-name-safe id for a session, agent or connection: the
/// creation time in hex nanoseconds, made unique within the process (two ids
/// asked for in the same tick, on a clock that ticks in microseconds, differ).
#[must_use]
pub fn new_id() -> String {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let mut last = LAST.load(Ordering::Relaxed);
    let id = loop {
        let next = now.max(last.saturating_add(1));
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break next,
            Err(actual) => last = actual,
        }
    };
    format!("{id:x}")
}

/// Ids become file names, so only a conservative character set is allowed.
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// On-disk shape of [`INDEX_FILE`].
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Index {
    version: u32,
    sessions: Vec<SessionSummary>,
}

/// Reads and writes the session files in one directory, keeping the index
/// up to date.
#[derive(Debug)]
pub struct SessionStore {
    dir: PathBuf,
    /// Summaries, newest first. Valid once `indexed` is set.
    index: Vec<SessionSummary>,
    indexed: bool,
}

impl SessionStore {
    /// The store in `~/.atroxabot/sessions`.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn open() -> Result<Self> {
        Ok(Self::at(Config::dir()?.join("sessions")))
    }

    /// A store in an explicit directory (created on first save). Cheap: no
    /// I/O happens until a method needs it.
    #[must_use]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir, index: Vec::new(), indexed: false }
    }

    /// The directory holding the session files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        if valid_id(id) {
            Ok(self.dir.join(format!("{id}.json")))
        } else {
            Err(Error::Io(std::io::Error::new(ErrorKind::InvalidInput, "invalid session id")))
        }
    }

    /// Summaries of every session, most recently updated first, plus the
    /// errors of session files that could not be read (those files are left
    /// untouched and not listed).
    ///
    /// Reads only the index, re-reading session files that are missing from
    /// it or changed after it was written.
    ///
    /// # Errors
    /// The directory exists but cannot be listed.
    pub fn list(&mut self) -> Result<(Vec<SessionSummary>, Vec<Error>)> {
        let index_path = self.dir.join(INDEX_FILE);
        let (mut index, index_time) = match fs::read(&index_path) {
            // A corrupt index is rebuilt from the session files.
            Ok(bytes) => match serde_json::from_slice::<Index>(&bytes) {
                Ok(index) => (index.sessions, fs::metadata(&index_path).and_then(|m| m.modified()).ok()),
                Err(_) => (Vec::new(), None),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Vec::new(), None),
            Err(e) => return Err(e.into()),
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };

        let (mut on_disk, mut errors, mut changed) = (HashSet::new(), Vec::new(), false);
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            let path = entry.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else {
                continue;
            };
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            on_disk.insert(id.to_owned());
            let known = index.iter().position(|s| s.id == id);
            let modified = entry.metadata().and_then(|m| m.modified()).ok();
            let stale = match (known, index_time, modified) {
                (Some(_), Some(indexed), Some(modified)) => modified > indexed,
                _ => true,
            };
            if !stale {
                continue;
            }
            // A session from a newer version is listed, though it won't open.
            match self.read(id) {
                Ok(session) => {
                    let summary = session.summary();
                    match known {
                        Some(i) => index[i] = summary,
                        None => index.push(summary),
                    }
                    changed = true;
                }
                Err(e) => {
                    // Keep a stale entry out of the list rather than show it.
                    if let Some(i) = known {
                        index.remove(i);
                    }
                    errors.push(e);
                }
            }
        }
        let before = index.len();
        index.retain(|s| on_disk.contains(&s.id));
        changed |= index.len() != before;

        index.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.index = index;
        self.indexed = true;
        if changed && let Err(e) = self.write_index() {
            // Listing still worked; the next save retries the write.
            errors.push(e);
        }
        Ok((self.index.clone(), errors))
    }

    /// Reads one session with all its messages.
    ///
    /// # Errors
    /// An invalid id, a missing or unreadable file, malformed JSON, or
    /// [`Error::Newer`] for a session saved by a newer version of the app.
    pub fn load(&self, id: &str) -> Result<Session> {
        let session = self.read(id)?;
        if session.version > Session::VERSION {
            return Err(Error::Newer { what: "session" });
        }
        Ok(session)
    }

    /// Reads one session whatever version wrote it.
    fn read(&self, id: &str) -> Result<Session> {
        let mut session: Session = serde_json::from_slice(&fs::read(self.path(id)?)?)?;
        // The file name is authoritative; it is what `delete` removes.
        id.clone_into(&mut session.id);
        Ok(session)
    }

    /// Writes `session` atomically and updates the index.
    ///
    /// # Errors
    /// An invalid id or any I/O failure.
    pub fn save(&mut self, session: &Session) -> Result<()> {
        write_private(&self.path(&session.id)?, &serde_json::to_vec(session)?)?;
        self.ensure_indexed()?;
        let summary = session.summary();
        match self.index.iter_mut().find(|s| s.id == session.id) {
            Some(entry) => *entry = summary,
            None => self.index.push(summary),
        }
        self.index.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.write_index()
    }

    /// Every readable session with all its messages, for the statistics.
    /// Reads every file, so run it off the UI thread.
    #[must_use]
    pub fn all(&self) -> Vec<Session> {
        let Ok(entries) = fs::read_dir(&self.dir) else { return Vec::new() };
        let ids = entries.flatten().filter_map(|entry| {
            let path = entry.path();
            let json = path.extension().is_some_and(|ext| ext == "json");
            path.file_stem().and_then(|s| s.to_str()).filter(|id| json && valid_id(id)).map(str::to_owned)
        });
        // Unreadable files are skipped; `list` reports them.
        ids.filter_map(|id| self.load(&id).ok()).collect()
    }

    /// Case-insensitive search of every session's messages for `query`,
    /// newest sessions first, at most one hit per session. Reads every file,
    /// so run it off the UI thread.
    ///
    /// # Errors
    /// The directory exists but cannot be listed.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let needle = query.trim().to_lowercase();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut hits = Vec::new();
        if needle.is_empty() {
            return Ok(hits);
        }
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else {
                continue;
            };
            // Unreadable files are skipped; `list` reports them.
            let Ok(session) = self.load(id) else { continue };
            let found = session.lines().find(|line| line.to_lowercase().contains(&needle));
            if let Some(snippet) = found.map(|line| snippet(line, &needle)) {
                let Session { id, agent, title, updated, .. } = session;
                hits.push(SearchHit { session: id, agent, title, snippet, updated });
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.updated));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Deletes a session's file and its index entry; deleting a missing
    /// session succeeds.
    ///
    /// # Errors
    /// An invalid id or any I/O failure other than "not found".
    pub fn delete(&mut self, id: &str) -> Result<()> {
        match fs::remove_file(self.path(id)?) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        self.ensure_indexed()?;
        self.index.retain(|s| s.id != id);
        self.write_index()
    }

    /// Loads the index before the first change, so writing it back never
    /// drops sessions this store has not listed yet.
    fn ensure_indexed(&mut self) -> Result<()> {
        if !self.indexed {
            self.list()?;
        }
        Ok(())
    }

    fn write_index(&self) -> Result<()> {
        let index = Index { version: 1, sessions: self.index.clone() };
        write_private(&self.dir.join(INDEX_FILE), &serde_json::to_vec(&index)?)
    }
}

/// About 90 characters of `line` around the first match of `needle`
/// (already lower-cased), with ellipses where cut.
fn snippet(line: &str, needle: &str) -> String {
    const CONTEXT: usize = 40;
    let chars: Vec<char> = line.trim().chars().collect();
    let lower: Vec<char> = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let needle: Vec<char> = needle.chars().collect();
    // Case folding can change lengths; fall back to the start if it did.
    let at = if lower.len() == chars.len() {
        lower.windows(needle.len()).position(|w| w == needle.as_slice()).unwrap_or(0)
    } else {
        0
    };
    let start = at.saturating_sub(CONTEXT);
    let end = (at + needle.len() + CONTEXT).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> SessionStore {
        let dir = std::env::temp_dir().join(format!("atroxabot-{name}-{}", std::process::id())).join("sessions");
        let _ = fs::remove_dir_all(&dir);
        SessionStore::at(dir)
    }

    fn session(agent: &str, prompt: &str, answer: &str) -> Session {
        let mut session = Session::new(agent);
        session.entries.push(Entry::User { text: prompt.into(), note: String::new(), files: Vec::new() });
        session.entries.push(Entry::Reply(Reply {
            blocks: vec![
                Block::Thinking { text: "hm".into() },
                Block::Tool(ToolUse { id: "t".into(), name: "read".into(), input: "{}".into(), ..ToolUse::default() }),
                Block::Text { text: answer.into() },
            ],
            usage: Usage::new(10, 5),
            cost: 0.25,
            ..Reply::default()
        }));
        session
    }

    #[test]
    fn saves_lists_searches_and_deletes() {
        let mut store = store("sessions");
        let a = session("agent-1", "find the bug", "It is in parser.rs, line 3.");
        let mut b = session("agent-2", "hello", "Hi there.");
        b.updated = a.updated + 10;
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        let (listed, errors) = store.list().unwrap();
        assert!(errors.is_empty());
        assert_eq!(listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [b.id.as_str(), a.id.as_str()]);
        assert_eq!((listed[1].tokens, listed[1].cost), (15, 0.25));
        assert_eq!(listed[1].preview, "It is in parser.rs, line 3.");
        assert_eq!(store.load(&a.id).unwrap(), a);
        assert_eq!(a.digest(10_000), "User: find the bug\n\nYou: It is in parser.rs, line 3.");
        assert_eq!(a.digest(8), "You: It is in parser.rs, line 3.", "the latest is kept");

        let hits = store.search("PARSER", 10).unwrap();
        assert_eq!((hits.len(), hits[0].session.as_str(), hits[0].agent.as_str()), (1, a.id.as_str(), "agent-1"));
        store.delete(&a.id).unwrap();
        assert_eq!(store.list().unwrap().0.len(), 1);
        assert!(store.load("../etc").is_err(), "ids never leave the folder");
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn newer_sessions_are_listed_but_stay_closed() {
        let mut store = store("newer");
        let mut newer = session("a", "x", "y");
        newer.version = Session::VERSION + 1;
        store.save(&newer).unwrap();
        assert_eq!(store.list().unwrap().0.len(), 1);
        assert!(matches!(store.load(&newer.id), Err(Error::Newer { .. })));
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn the_task_list_is_read_from_the_tool_calls() {
        let call = |name: &str, input: Value, output: Option<&str>| {
            Block::Tool(ToolUse {
                name: name.into(),
                input: input.to_string(),
                output: output.map(str::to_owned),
                ..ToolUse::default()
            })
        };
        let create = |subject: &str, output: Option<&str>| {
            call("TaskCreate", serde_json::json!({ "subject": subject, "activeForm": format!("{subject}…") }), output)
        };
        let update =
            |id: &str, status: &str| call("TaskUpdate", serde_json::json!({ "taskId": id, "status": status }), None);
        let mut session = Session::new("a");
        let reply = |blocks| Entry::Reply(Reply { blocks, ..Reply::default() });
        session.entries.push(reply(vec![
            create("Read the file", Some("Task #1 created successfully: Read the file")),
            create("Fix the bug", Some("Task #2 created successfully: Fix the bug")),
            create("Not answered yet", None),
            update("1", "completed"),
            update("2", "in_progress"),
            update("9", "completed"),
            call("TaskUpdate", serde_json::json!({ "taskId": "2", "subject": "Fix both bugs" }), None),
            call("TaskUpdate", serde_json::json!("garbage"), None),
        ]));
        let todos = session.todos();
        let seen: Vec<_> = todos.iter().map(|todo| (todo.id.as_str(), todo.subject.as_str(), todo.state)).collect();
        assert_eq!(seen, [("1", "Read the file", TodoState::Done), ("2", "Fix both bugs", TodoState::Active)]);
        assert_eq!(todos[1].doing, "Fix the bug…");

        // Deleting takes one off; a task added once all are done starts a new list.
        session.entries.push(reply(vec![
            update("2", "deleted"),
            create("Write the test", Some("Task #3 created successfully: Write the test")),
        ]));
        assert_eq!(session.todos().iter().map(|todo| todo.id.as_str()).collect::<Vec<_>>(), ["3"]);

        // A whole list at once replaces what was there; a failed call changes nothing.
        let listed = serde_json::json!({ "todos": [
            { "content": "One", "status": "completed", "activeForm": "Doing one" },
            { "content": "Two", "status": "in_progress" }, "junk" ] });
        let failed =
            ToolUse { name: "TodoWrite".into(), input: "{\"todos\":[]}".into(), is_error: true, ..ToolUse::default() };
        session.entries.push(reply(vec![call("TodoWrite", listed, Some("ok")), Block::Tool(failed)]));
        let states: Vec<_> = session.todos().iter().map(|todo| todo.state).collect();
        assert_eq!(states, [TodoState::Done, TodoState::Active]);
    }

    #[test]
    fn ids_are_unique_and_safe() {
        let ids: HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.iter().all(|id| valid_id(id)));
        assert!(!valid_id("") && !valid_id("a/b") && !valid_id(".index"));
    }
}
