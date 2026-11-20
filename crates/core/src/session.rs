//! Sessions: the conversations of an agent, two files each in
//! `~/.atroxabot/sessions/`.
//!
//! # What is on disk
//!
//! * `<id>.json` is one JSON object: the [`Session`] with only its *open*
//!   entries (the latest ones, which still change) and `sealed`, the number
//!   of entries before them that the log keeps. It is written whole and
//!   atomically at every save.
//! * `<id>.log` holds the *sealed* entries, oldest first, one compact JSON
//!   [`Entry`] a line (`serde_json` never writes a raw newline). It is only
//!   ever added to at its end, or cut at the end of a line. A session with
//!   nothing sealed has none, or an empty one.
//!
//! A session's entries are the first `sealed` lines of the log, then the
//! entries of the JSON file. Whatever the log holds after those lines (a line
//! a crash tore, lines a later save stopped counting) is not read, and is cut
//! off before the next line is added. A log with fewer lines than counted
//! makes the session unreadable: it is reported as any unreadable session is,
//! and its files are left as they are.
//!
//! So a long session is not written again at every save: what a save writes
//! is the latest turn. The log is read a line at a time, never as one string.
//!
//! Files are written with the same private permissions as the config. Fields
//! added later must be `#[serde(default)]` so older files keep loading. Each
//! JSON file records the [`Session::VERSION`] that wrote it; one from a newer
//! version of the app is listed but never opened, since saving it here would
//! drop what this version does not know. Version 1 had no log: its one file
//! held every entry and no `sealed`. Such a file loads as it always did, and
//! its next save writes it as this version, which an app of version 1 then
//! leaves closed: it would read the open entries alone and save over the rest.
//!
//! # How a save decides
//!
//! The store that saves (one, on the app's writer thread) remembers of every
//! session the entries it last wrote and where each line of the log ends
//! (`Written`). The first entries of those, as many as there are lines, are
//! the log `L`, which is also exactly what the JSON file on disk counts; all
//! of them are the previous snapshot `P`. Given the session `S` to save:
//!
//! 1. `common` is the length of the longest prefix where `L[i]` is `S[i]`.
//! 2. `target` is the smaller of the index of the last [`Entry::User`] of `S`
//!    (0 without one) and the first index where `S` differs from `P`. The
//!    entries before it came through a save unchanged and belong to earlier
//!    turns: they are worth sealing.
//! 3. If `common < L.len()` (a sealed entry changed, or entries went away: a
//!    `/clear`, a summary put in), `sealed = min(common, target)`, which is
//!    `target`. The JSON file is written with `S[sealed..]`, *then* the log
//!    is cut back to `sealed` lines.
//! 4. Otherwise, if `target > L.len()`: the log is cut back to `L.len()`
//!    lines where it is longer, `S[L.len()..target]` are added as lines and
//!    flushed to the disk, *then* the JSON file is written with `S[target..]`.
//! 5. Otherwise the JSON file is written with `S[L.len()..]`.
//!
//! "Is" in these comparisons is [`Arc::ptr_eq`] for a reply and `==` for the
//! other, small entries. A reply is shared, not copied ([`Entry::Reply`]),
//! and changed only through [`Arc::make_mut`], which copies it while anyone
//! else holds it. The store holds every reply it last wrote, so **a reply
//! that changed since then is another allocation**, and one that is still
//! the same allocation is what the disk holds. That is why the store keeps
//! the last snapshot, and why nothing may change a reply behind its `Arc`.
//!
//! A store that knows nothing of a session it is to save (it never saved it,
//! and was handed no [`Kept`]) cannot tell what changed: it writes every
//! entry to the JSON file with `sealed` 0, then empties the log. To spare
//! that, what [`SessionStore::load_kept`] found is given to the saving store
//! with [`SessionStore::adopt`], which takes it only while the JSON file is
//! still the one that was read.
//!
//! # A crash between two steps
//!
//! What is on disk always loads as the session before the save or the one
//! after it, never with entries missing:
//!
//! * In 3, before the JSON file is replaced: the old session. After it, with
//!   the log not yet cut: the new one, for the lines up to `sealed` did not
//!   change and the ones after them are no longer counted.
//! * In 4, once the log is cut back to `L.len()` lines: the old session,
//!   which counts no more than those. While lines are added, or all are but
//!   the JSON file is not yet replaced: still the old session, the new lines
//!   (the last perhaps torn) lying beyond its count. After it: the new one.
//!   The lines are flushed before the JSON file that counts them is written,
//!   so that no power cut leaves a count the log does not hold.
//! * In 5, and wherever the JSON file is written: the old file or the new,
//!   since it is replaced atomically.
//! * In [`SessionStore::delete`], the JSON file goes first: a crash then
//!   leaves a log no session counts, which nothing reads.
//!
//! A save that fails part of the way leaves the same states, and the store
//! believing what it believed before, which is still true of the disk. A log
//! found shorter than the store knows it to be is not the one it wrote: the
//! session is then written whole, as by a store that knows nothing.
//!
//! # Readers on other threads
//!
//! Searches, the statistics and opening a session read on threads of their
//! own while the writer saves. A reader that takes lines from the log reads
//! the JSON file again afterwards and starts over when it changed meanwhile,
//! so it never joins the count of one save to the log of another.
//!
//! # The index
//!
//! Next to the sessions, `.index.json` holds a [`SessionSummary`] per session
//! so the sidebar loads without reading any message bodies. The index is a
//! cache: [`SessionStore::list`] reconciles it against the files on disk (a
//! directory listing plus modification times, no reads) and re-reads only
//! sessions that are new or changed since the index was written, so a crash
//! between the writes of a save heals itself.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hasher as _;
use std::io::{BufRead as _, BufReader, ErrorKind, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::Agent;
use crate::config::{Config, create_private_dir, write_private};
use crate::error::{Error, Result};
use crate::responses::{Picture, Usage, Wire};
use crate::worktree::Worktree;

/// Name of the index file. The leading dot keeps it out of session scans,
/// since `.` is not a valid id character.
const INDEX_FILE: &str = ".index.json";
/// The index's format: one written in another is made again from the
/// session files, so every summary holds what this version keeps in it.
const INDEX_VERSION: u32 = 2;

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
    /// Whether the user gave it its title: `/clear` then keeps it, where a
    /// title taken from the first prompt goes with the messages.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub named: bool,
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
    /// The provider (a connection's id) the session thinks through, with
    /// [`Session::model`] and [`Session::effort`]: its own, wherever its
    /// agent goes on to start its next sessions. Empty (a session that
    /// never chose), or a provider that no longer exists, means the
    /// agent's own choice.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The model the session answers with; empty for the provider's default.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// How hard it thinks; empty for the model's usual effort.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// What the context held after the latest turn, by kind, as far as the
    /// engine said; empty when it did not.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextPart>,
    /// The system prompt an API engine started the session with: rendered
    /// at its first turn and sent unchanged from then on, so the prompt
    /// cache and the model's kept reasoning stay valid while the agent's
    /// notes or the project's instruction files change. `/clear` starts
    /// over without it. Empty until then, and for the account engines.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// What an account engine (Claude Code, Codex) was told of the agent's
    /// notes when it first started the session ([`Agent::memory_section`]):
    /// every later start says the same, whatever the notes hold by then,
    /// so the engine resumes the conversation under the system prompt it
    /// has cached. `/clear` starts over without it. `None` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Until when the provider keeps the conversation in its prompt
    /// cache, in Unix seconds, as its engine last worked out
    /// ([`crate::AgentEvent::Cached`]): a message sent by then reads the
    /// context at a fraction of its price, one sent later all of it anew.
    /// 0 when not known (Codex, other APIs, another provider or model
    /// chosen since).
    #[serde(skip_serializing_if = "is_zero")]
    pub cached_until: u64,
    /// Whether its model turned out to take no pictures (its provider
    /// refused one): a `read` of one shows it no more.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blind: bool,
    /// What its engine had counted for it in all, by model, when a turn last ended
    /// ([`crate::AgentEvent::Spent`]): Claude Code's running totals, which it keeps with the
    /// conversation and takes up again when that is resumed. What the next turn used is what is
    /// counted beyond them. Empty for the engines that keep no such count, and until one is said.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub metered: Vec<Spent>,
    /// The git worktree of its own it works in, when it asked for one:
    /// [`Session::workdir`] is a folder in it once it is made. See [`crate::worktree`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    /// What was said and done, oldest first.
    pub entries: Vec<Entry>,
}

/// Whether a number is 0: such a field is left out of the file.
#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde calls it with a reference")]
fn is_zero(number: &u64) -> bool {
    *number == 0
}

/// One kind of thing a context holds, and the tokens it takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextPart {
    /// What it is: "System prompt", "MCP tools", "Messages", …
    pub name: String,
    /// The tokens it takes.
    pub tokens: u64,
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
    /// One response of the model: text, reasoning and tool calls with
    /// results. Shared, so that a copy of a session copies no reply: change
    /// one through [`Arc::make_mut`] only, which is also how the store tells
    /// that it changed (see the module's "How a save decides").
    Reply(Arc<Reply>),
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

impl From<Reply> for Entry {
    /// The reply as an entry of a session.
    fn from(reply: Reply) -> Self {
        Self::Reply(Arc::new(reply))
    }
}

impl Entry {
    /// Whether this is `other` as a save sees it: for a reply the very same
    /// one, not merely one that reads the same (a reply that was changed is
    /// another allocation, see the module comment); for the other entries,
    /// which are small, an equal one.
    fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Reply(one), Self::Reply(two)) => Arc::ptr_eq(one, two),
            (Self::Reply(_), _) | (_, Self::Reply(_)) => false,
            _ => self == other,
        }
    }
}

/// One response of the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    /// What it contained, in order.
    pub blocks: Vec<Block>,
    /// The reply exactly as its wire sent it (Claude's content blocks with
    /// their thinking signatures, a Chat Completions message with its
    /// reasoning, the Responses API's output items), replayed unchanged so
    /// the model keeps its reasoning between tool calls. Empty for the
    /// account engines and for a reply cut short.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw: Vec<Value>,
    /// The wire `raw` came by, which alone can take it back.
    #[serde(skip_serializing_if = "Wire::is_messages")]
    pub wire: Wire,
    /// The model that wrote it.
    pub model: String,
    /// What it used. For an account's engine, whose reply is a whole turn, this is the turn's last
    /// request alone: how full the context is ([`Session::context_tokens`]), not what the turn used.
    pub usage: Usage,
    /// What every request of its turn used, by model: each step of it, and those of its subagents
    /// ([`crate::AgentEvent::Spent`]). Empty where `usage` says it all: a reply of an API engine, which
    /// is one request, and one saved before this was kept.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spent: Vec<Spent>,
    /// Its cost in USD, when the engine reports or can price it.
    pub cost: f64,
    /// Why it ended badly (an error, a refusal, cut off); empty when it finished.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub problem: String,
    /// When it ended, in Unix seconds; 0 for a reply saved before replies were dated.
    pub at: u64,
}

impl Reply {
    /// What its turn used in all: every request made for it where the engine counts them
    /// ([`Reply::spent`]), else what the reply says of itself ([`Reply::usage`]).
    #[must_use]
    pub fn used(&self) -> Usage {
        if self.spent.is_empty() {
            return self.usage;
        }
        self.spent.iter().fold(Usage::default(), |sum, spent| sum.plus(spent.usage))
    }
}

/// What one model used: for a turn ([`Reply::spent`]), or for a session so far ([`Session::metered`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spent {
    /// The model's id.
    pub model: String,
    /// The tokens of its requests, added up.
    pub usage: Usage,
    /// What they cost in USD, when the engine says.
    #[serde(skip_serializing_if = "is_nothing")]
    pub cost: f64,
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
fn is_nothing(cost: &f64) -> bool {
    *cost == 0.0
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
    /// The user said no when it asked to run: it never ran.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub denied: bool,
    /// What the subagent this call started said and did, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sub: Vec<Block>,
    /// For a subagent: what it is doing now, or how it ended.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// A picture its result showed the model (a `read` of a PNG), kept so
    /// every later request sends the same bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<Picture>,
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
    /// What the agent wrote of it beyond its subject; may be empty.
    pub detail: String,
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
    /// The current file format: 2 since a session's sealed entries are kept
    /// in a log beside its JSON file.
    pub const VERSION: u32 = 2;

    /// The agent's task list as its tool calls left it: Claude Code's
    /// `TaskCreate` and `TaskUpdate`, or a whole list at once from
    /// `TodoWrite` (or an API agent's `todo`). A task added once every earlier one is done starts a
    /// new list.
    #[must_use]
    pub fn todos(&self) -> Vec<Todo> {
        let mut todos: Vec<Todo> = Vec::new();
        let calls = self.replies().flat_map(|reply| &reply.blocks).filter_map(|block| match block {
            Block::Tool(call)
                if !call.is_error
                    && matches!(call.name.as_str(), "TaskCreate" | "TaskUpdate" | "TodoWrite" | "todo") =>
            {
                Some(call)
            }
            _ => None,
        });
        for call in calls {
            let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
            let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            match call.name.as_str() {
                // An API agent's `todo` takes the same list.
                "TodoWrite" | "todo" => {
                    let listed = input.get("todos").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                    todos = (1..)
                        .zip(listed)
                        .map(|(number, todo)| Todo {
                            id: number.to_string(),
                            subject: text(todo, "content"),
                            doing: text(todo, "activeForm"),
                            detail: String::new(),
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
                        detail: text(&input, "description"),
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
                    let fields = [
                        ("subject", &mut todo.subject),
                        ("activeForm", &mut todo.doing),
                        ("description", &mut todo.detail),
                    ];
                    for (key, field) in fields {
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

    /// Gives `agent` the provider, model and effort this session keeps,
    /// when it keeps any and its provider still exists (`known`): a
    /// session thinks with its own choice, not with what its agent was
    /// given since.
    pub fn apply_choice(&self, agent: &mut Agent, known: bool) {
        if known && !self.provider.is_empty() {
            self.provider.clone_into(&mut agent.connection);
            self.model.clone_into(&mut agent.model);
            self.effort.clone_into(&mut agent.effort);
        }
    }

    /// Makes `agent`'s provider, model and effort this session's own.
    pub fn keep_choice(&mut self, agent: &Agent) {
        // Another provider or model has nothing of this conversation cached.
        if agent.connection != self.provider || agent.model != self.model {
            self.cached_until = 0;
        }
        agent.connection.clone_into(&mut self.provider);
        agent.model.clone_into(&mut self.model);
        agent.effort.clone_into(&mut self.effort);
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

    /// The folder the user chose for it: its own, or for a session in a
    /// worktree the folder of the repository that its folder there stands for.
    #[must_use]
    pub fn origin(&self) -> String {
        let Some(worktree) = self.worktree.as_ref().filter(|worktree| worktree.made()) else {
            return self.workdir.clone();
        };
        match Path::new(&self.workdir).strip_prefix(&worktree.path) {
            Ok(inside) if inside.as_os_str().is_empty() => worktree.repo.clone(),
            Ok(inside) => Path::new(&worktree.repo).join(inside).display().to_string(),
            Err(_) => worktree.repo.clone(),
        }
    }

    /// What an agent is told, after where it works, when that is a
    /// worktree; empty otherwise. The same for the whole session.
    #[must_use]
    pub fn worktree_note(&self) -> String {
        let Some(worktree) = self.worktree.as_ref().filter(|worktree| worktree.made()) else {
            return String::new();
        };
        format!(
            "\n\nThat folder is a git worktree made for this session: a checkout of its own of the repository `{}`, \
             on the branch `{}` (started from `{}`). Work there and commit your work on that branch; the user's own \
             checkout in `{}` is not yours to change. The worktree is removed once its branch is merged, so leave \
             nothing in it uncommitted that should be kept.",
            worktree.repo, worktree.branch, worktree.base, worktree.repo
        )
    }

    /// The agent's own prompt for an account engine (Claude Code, Codex)
    /// that starts or resumes this session: its instructions as they are
    /// now, where it works, and its notes as the session first saw them.
    /// The second value is what it says of the notes when that was made
    /// just now: to be kept as [`Session::memory`].
    #[must_use]
    pub fn account_prompt(&self, agent: &Agent) -> (String, Option<String>) {
        let (memory, made) = match (&self.memory, agent.memory_section()) {
            // An agent that keeps no notes (any more) is told nothing of them.
            (_, None) => (String::new(), None),
            (Some(kept), Some(_)) => (kept.clone(), None),
            (None, Some(now)) => (now.clone(), Some(now)),
        };
        (format!("{}{}", agent.system_prompt_seeing(&self.folder(), &memory), self.worktree_note()), made)
    }

    /// What was said so far, as plain text of at most about `budget` bytes
    /// (the latest kept): what an engine that never saw the conversation is
    /// told when it takes it over.
    #[must_use]
    pub fn digest(&self, budget: usize) -> String {
        Self::digest_of(&self.entries, budget)
    }

    /// [`Session::digest`] of what was said before the message the user
    /// sent last, where that is the latest thing in the session: what an
    /// engine is told that is about to be given that message itself.
    #[must_use]
    pub fn digest_before_last(&self, budget: usize) -> String {
        let earlier = match self.entries.split_last() {
            Some((Entry::User { .. }, earlier)) => earlier,
            _ => &self.entries,
        };
        Self::digest_of(earlier, budget)
    }

    fn digest_of(entries: &[Entry], budget: usize) -> String {
        let mut lines = Vec::new();
        let mut used = 0;
        for entry in entries.iter().rev() {
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

    /// The tokens in and out of every request made for it ([`Reply::used`]).
    #[must_use]
    pub fn tokens(&self) -> u64 {
        let used = self.replies().map(Reply::used);
        used.map(|used| used.input_tokens.saturating_add(used.output_tokens)).fold(0, u64::saturating_add)
    }

    /// The prompt and output tokens of the latest reply: how full the context is.
    #[must_use]
    pub fn context_tokens(&self) -> u64 {
        // A reply still being written has no counts yet: the one before it says.
        let sizes = self.replies().rev().map(|r| r.usage.input_tokens.saturating_add(r.usage.output_tokens));
        sizes.into_iter().find(|size| *size > 0).unwrap_or(0)
    }

    fn replies(&self) -> impl DoubleEndedIterator<Item = &Reply> {
        self.entries.iter().filter_map(|e| if let Entry::Reply(reply) = e { Some(reply.as_ref()) } else { None })
    }

    /// The tool call `id`, wherever it is: in a reply, or in what a subagent
    /// did. Only the reply that holds it is made this session's own to
    /// change; every other one stays shared with whoever else holds it.
    pub fn find_tool(&mut self, id: &str) -> Option<&mut ToolUse> {
        fn holds(blocks: &[Block], id: &str) -> bool {
            blocks.iter().any(|block| matches!(block, Block::Tool(call) if call.id == id || holds(&call.sub, id)))
        }
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
        // Looked for without asking to change anything: `Arc::make_mut` on a reply that is shared copies it.
        let at =
            self.entries.iter().rposition(|entry| matches!(entry, Entry::Reply(reply) if holds(&reply.blocks, id)))?;
        match &mut self.entries[at] {
            Entry::Reply(reply) => within(&mut Arc::make_mut(reply).blocks, id),
            _ => None,
        }
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
            worktree: self.worktree.clone().filter(Worktree::made),
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
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
    /// The worktree it works in, once made: what the cleaning up goes by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    /// The provider it thinks through ([`Session::provider`]): known
    /// before its messages are read, so it opens showing its own.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The model it answers with ([`Session::model`]).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// How hard it thinks ([`Session::effort`]).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
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

/// The first [`Session::VERSION`] whose JSON file counts entries kept in a
/// log: an older file holds every entry itself, whatever else it says.
const LOGGED: u32 = 2;

/// How a session's JSON file is written: the session with only its open
/// entries, and how many came before them.
#[derive(Serialize)]
struct Stored<'a> {
    #[serde(flatten)]
    session: &'a Session,
    /// The entries before the session's that its log keeps, a line each.
    sealed: u64,
}

/// What a session's JSON file says of its log.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Count {
    sealed: u64,
}

/// What stands for a session's JSON file when asking whether it is still the
/// one that was read: its length and a hash of its bytes.
type Mark = (u64, u64);

/// The [`Mark`] of a JSON file's `bytes`.
fn mark(bytes: &[u8]) -> Mark {
    // Not a secret and nothing to defend: only ever compared with another of this run.
    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(bytes);
    (bytes.len() as u64, hasher.finish())
}

/// What a store knows a session's files to hold, which lets it add to them
/// rather than write them again.
#[derive(Default)]
struct Written {
    /// The entries as they were last written (or read), all of them: what
    /// the next save is compared with. The store's hold on them is also
    /// what makes a reply that changes since become another allocation.
    entries: Vec<Entry>,
    /// Where each line of the log ends, in bytes from its start: line `i`
    /// holds `entries[i]`, and the JSON file counts exactly these lines.
    ends: Vec<u64>,
}

impl std::fmt::Debug for Written {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The entries themselves would be megabytes.
        f.debug_struct("Written").field("entries", &self.entries.len()).field("sealed", &self.ends.len()).finish()
    }
}

/// What reading a session found on disk, for the store that will save it
/// ([`SessionStore::adopt`]): with it the first save after opening a session
/// adds a turn to its files, where a store that knows nothing of them has to
/// write them all again. It shares the session's replies, so it costs no
/// memory worth the name while the session is held.
#[derive(Debug)]
pub struct Kept {
    written: Written,
    /// The JSON file this was read from.
    mark: Mark,
}

/// A session as its files hold it.
struct Found {
    session: Session,
    /// Where the line of each sealed entry ends in the log.
    ends: Vec<u64>,
    /// The bytes of the JSON file it was read from.
    file: Vec<u8>,
}

/// The first `count` entries of the log at `path`, and where the line of
/// each ends. What the log holds after them is not read.
fn read_log(path: &Path, count: u64) -> Result<(Vec<Entry>, Vec<u64>)> {
    let mut reader = BufReader::with_capacity(1 << 16, fs::File::open(path)?);
    let (mut entries, mut ends, mut end, mut line) = (Vec::new(), Vec::new(), 0_u64, Vec::new());
    // Counted up to, never allocated for: the count is whatever the file says.
    while (entries.len() as u64) < count {
        line.clear();
        end += reader.read_until(b'\n', &mut line)? as u64;
        if line.last() != Some(&b'\n') {
            // The end of the log, or a line a crash tore: either way a line that was counted is not there.
            let message = "the session's log holds fewer entries than its file counts";
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, message).into());
        }
        entries.push(serde_json::from_slice(&line)?);
        ends.push(end);
    }
    Ok((entries, ends))
}

/// Adds `entries` to the log at `path`, a line each, after its first `end`
/// bytes: what it holds beyond those (lines no JSON file counts, left by a
/// crash or a save that failed) is cut off first. The lines are on the disk
/// when this returns where each of them ends; `None` when the log is shorter
/// than `end`, so not the one the store knows.
fn append(path: &Path, end: u64, entries: &[Entry]) -> Result<Option<Vec<u64>>> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let length = file.metadata()?.len();
    if length < end {
        return Ok(None);
    }
    if length > end {
        file.set_len(end)?;
    }
    file.seek(SeekFrom::Start(end))?;
    let (mut ends, mut end, mut line) = (Vec::with_capacity(entries.len()), end, Vec::new());
    for entry in entries {
        line.clear();
        serde_json::to_writer(&mut line, entry)?;
        line.push(b'\n');
        file.write_all(&line)?;
        end += line.len() as u64;
        ends.push(end);
    }
    file.sync_all()?;
    #[cfg(unix)]
    if length == 0
        && let Some(dir) = path.parent()
    {
        // A log that may just have been made: its name is to be on the disk too before a JSON file
        // counts on it. Best effort, as for the files written atomically.
        let _ = fs::File::open(dir).and_then(|dir| dir.sync_all());
    }
    Ok(Some(ends))
}

/// Cuts the log at `path` back to its first `end` bytes; one that is no
/// longer, or not there, is left alone.
fn cut(path: &Path, end: u64) -> std::io::Result<()> {
    match fs::metadata(path) {
        Ok(log) if log.len() > end => fs::OpenOptions::new().write(true).open(path)?.set_len(end),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Reads and writes the session files in one directory, keeping the index
/// up to date.
#[derive(Debug)]
pub struct SessionStore {
    dir: PathBuf,
    /// Summaries, newest first. Valid once `indexed` is set.
    index: Vec<SessionSummary>,
    indexed: bool,
    /// What the files of each session hold, for those this store saved or
    /// was handed ([`SessionStore::adopt`]) and did not let go of since.
    written: HashMap<String, Written>,
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
        Self { dir, index: Vec::new(), indexed: false, written: HashMap::new() }
    }

    /// The directory holding the session files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The JSON file and the log of the session `id`.
    fn paths(&self, id: &str) -> Result<(PathBuf, PathBuf)> {
        if valid_id(id) {
            Ok((self.dir.join(format!("{id}.json")), self.dir.join(format!("{id}.log"))))
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
            // A corrupt index, or one in another format, is rebuilt from the session files.
            Ok(bytes) => match serde_json::from_slice::<Index>(&bytes) {
                Ok(index) if index.version == INDEX_VERSION => {
                    (index.sessions, fs::metadata(&index_path).and_then(|m| m.modified()).ok())
                }
                Ok(_) | Err(_) => (Vec::new(), None),
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
                Ok(found) => {
                    let summary = found.session.summary();
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
    /// An invalid id, a missing or unreadable file, malformed JSON, a log
    /// that holds fewer entries than the file counts, or [`Error::Newer`]
    /// for a session saved by a newer version of the app.
    pub fn load(&self, id: &str) -> Result<Session> {
        Ok(self.found(id)?.session)
    }

    /// Reads one session as [`SessionStore::load`] does, and with it what
    /// the store that will save it needs to add to its files rather than
    /// write them again: hand that to [`SessionStore::adopt`] of that store
    /// before the session is saved.
    ///
    /// # Errors
    /// Those of [`SessionStore::load`].
    pub fn load_kept(&self, id: &str) -> Result<(Session, Kept)> {
        let Found { session, ends, file } = self.found(id)?;
        // The replies are shared with the session, not copied.
        let kept = Kept { written: Written { entries: session.entries.clone(), ends }, mark: mark(&file) };
        Ok((session, kept))
    }

    /// Takes what a [`SessionStore::load_kept`] found of the session `id`,
    /// so the next save of the session that came with it adds to its files.
    /// It is taken only if the session's JSON file is still the one that was
    /// read: after a save since, the files are no longer what `kept` says,
    /// and the store goes on with what it knows itself (which, when that is
    /// nothing, makes the next save write everything).
    pub fn adopt(&mut self, id: &str, kept: Kept) {
        let Ok((json, _)) = self.paths(id) else { return };
        if fs::read(json).is_ok_and(|bytes| mark(&bytes) == kept.mark) {
            self.written.insert(id.to_owned(), kept.written);
        }
    }

    /// Lets go of what the store knows of the session `id`'s files, and so
    /// of its hold on the session's replies: for a session the app no longer
    /// holds. Saving it again later writes all of it once.
    pub fn forget(&mut self, id: &str) {
        self.written.remove(id);
    }

    /// Reads one session this version can open.
    fn found(&self, id: &str) -> Result<Found> {
        let mut found = self.read(id)?;
        if found.session.version > Session::VERSION {
            return Err(Error::Newer { what: "session" });
        }
        // What is held from here on is in this version's shape, and is saved as such.
        found.session.version = Session::VERSION;
        Ok(found)
    }

    /// Reads one session whatever version wrote it.
    fn read(&self, id: &str) -> Result<Found> {
        /// How often a session whose files change while they are read is read again.
        const TRIES: usize = 4;
        let (json, log) = self.paths(id)?;
        let mut bytes = fs::read(&json)?;
        let mut tries = 1;
        loop {
            let mut session: Session = serde_json::from_slice(&bytes)?;
            // Only a file of a version that keeps a log counts entries in one.
            let sealed = if session.version >= LOGGED { serde_json::from_slice::<Count>(&bytes)?.sealed } else { 0 };
            let mut ends = Vec::new();
            if sealed > 0 {
                let read = read_log(&log, sealed);
                // The writer may have saved this very session meanwhile, on its own thread: the log
                // read is then not the one this JSON file counted.
                let now = fs::read(&json)?;
                if now != bytes {
                    if tries == TRIES {
                        let message = "the session's files kept changing while they were read";
                        return Err(std::io::Error::new(ErrorKind::ResourceBusy, message).into());
                    }
                    (bytes, tries) = (now, tries + 1);
                    continue;
                }
                let (mut entries, lines) = read?;
                entries.append(&mut session.entries);
                (session.entries, ends) = (entries, lines);
            }
            // The file name is authoritative; it is what `delete` removes.
            id.clone_into(&mut session.id);
            return Ok(Found { session, ends, file: bytes });
        }
    }

    /// Writes `session` and updates the index: the entries of its latest
    /// turn to its JSON file, atomically, and the ones before to its log,
    /// which is added to rather than written again wherever the store knows
    /// what it holds (the module comment says how, and what a crash leaves).
    ///
    /// # Errors
    /// An invalid id or any I/O failure.
    pub fn save(&mut self, session: &Session) -> Result<()> {
        let (json, log) = self.paths(&session.id)?;
        // A store that knows nothing of the files writes every entry, and empties the log.
        let mut written = self.written.remove(&session.id).unwrap_or_default();
        let saved = Self::write(&json, &log, &mut written, session);
        // Kept whether it worked or not: a save that failed changed nothing of what is known.
        self.written.insert(session.id.clone(), written);
        saved?;
        self.ensure_indexed()?;
        let summary = session.summary();
        match self.index.iter_mut().find(|s| s.id == session.id) {
            Some(entry) => *entry = summary,
            None => self.index.push(summary),
        }
        self.index.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.write_index()
    }

    /// Writes `session` to its files, which hold what `written` says, and
    /// notes in `written` what they hold then. `written` changes only once
    /// the JSON file is replaced: until then the files still load as before.
    fn write(json: &Path, log: &Path, written: &mut Written, session: &Session) -> Result<()> {
        let entries = &session.entries;
        let logged = written.ends.len();
        // The first entry that is not the one last written; what the log shares with the session
        // (`common` in the module comment) is this, or all of the log.
        let unchanged = written.entries.iter().zip(entries).take_while(|(old, new)| old.is(new)).count();
        let turn = entries.iter().rposition(|entry| matches!(entry, Entry::User { .. })).unwrap_or(0);
        let target = turn.min(unchanged);
        let rewinds = unchanged < logged;
        let sealed = if rewinds { target } else { target.max(logged) };

        let mut added = Vec::new();
        if sealed > logged {
            let end = written.ends.last().copied().unwrap_or(0);
            let Some(ends) = append(log, end, &entries[logged..sealed])? else {
                // The log is not the one this store wrote: all of the session is here, so it is
                // written as by a store that knows nothing.
                *written = Written::default();
                return Self::write(json, log, written, session);
            };
            added = ends;
        }

        // The store's own copy: its entries are what the next save is compared with.
        let mut open = session.clone();
        open.version = open.version.max(Session::VERSION);
        let latest = open.entries.split_off(sealed);
        let mut earlier = std::mem::replace(&mut open.entries, latest);
        write_private(json, &serde_json::to_vec(&Stored { session: &open, sealed: sealed as u64 })?)?;

        written.ends.truncate(sealed);
        written.ends.append(&mut added);
        earlier.append(&mut open.entries);
        written.entries = earlier;
        if rewinds || sealed == 0 {
            // Only tidying: the JSON file no longer counts those lines, and what stays of them is
            // cut off before the next line is added.
            let _ = cut(log, written.ends.last().copied().unwrap_or(0));
        }
        Ok(())
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
            // A session is its JSON file; its log is read with it.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
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

    /// Deletes a session's files and its index entry, and forgets what the
    /// store knew of them; deleting a missing session succeeds.
    ///
    /// # Errors
    /// An invalid id or any I/O failure other than "not found".
    pub fn delete(&mut self, id: &str) -> Result<()> {
        let (json, log) = self.paths(id)?;
        self.forget(id);
        // The JSON file first: without it the log is nobody's, where a JSON file without the log it
        // counts would be a session that cannot be read.
        for file in [json, log] {
            match fs::remove_file(file) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
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
        let index = Index { version: INDEX_VERSION, sessions: self.index.clone() };
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

    #[test]
    fn an_account_engine_is_resumed_under_the_prompt_it_began_with() {
        // The agent's notes change while it works: what a session was first told of them is what it is told
        // again, so the engine's cached conversation still follows the same system prompt.
        let agent = Agent { instructions: "Be brief.".into(), ..Agent::new("Scout") };
        let mut session = Session { workdir: "/work".into(), ..Session::new(&agent.id) };
        let (first, made) = session.account_prompt(&agent);
        let made = made.expect("made at the first start");
        assert!(first.contains("You keep notes across sessions") && first.ends_with(&made), "{first}");
        session.apply(&crate::AgentEvent::Memory(format!("{made}{}old notes", crate::agent::NOTES_HEADING)));
        let (again, kept) = session.account_prompt(&agent);
        assert!(kept.is_none(), "nothing new to keep");
        assert!(again.ends_with("old notes") && again.starts_with("You are Scout."), "{again}");
        assert_eq!(session.account_prompt(&agent).0, again, "the same at every start");
        // The agent's own instructions are the user's to change: that reaches the session.
        let edited = Agent { instructions: "Be thorough.".into(), ..agent.clone() };
        assert!(session.account_prompt(&edited).0.contains("Be thorough.") && again.contains("Be brief."));
        // One that no longer keeps notes is told nothing of them.
        let (plain, kept) = session.account_prompt(&Agent { memory: false, ..agent });
        assert!(kept.is_none() && !plain.contains("notes"), "{plain}");
    }

    #[test]
    fn what_is_known_of_the_cache_goes_with_the_provider_and_model() {
        let mut agent = Agent::new("Scout");
        (agent.connection, agent.model) = ("claude".into(), "opus".into());
        let mut session = Session::new(&agent.id);
        session.keep_choice(&agent);
        session.apply(&crate::AgentEvent::Cached(5_000));
        assert_eq!(session.cached_until, 5_000);
        // Another effort thinks with the same model: still cached. Another model or provider: not known.
        agent.effort = "high".into();
        session.keep_choice(&agent);
        assert_eq!(session.cached_until, 5_000);
        agent.model = "haiku".into();
        session.keep_choice(&agent);
        assert_eq!(session.cached_until, 0);
        session.apply(&crate::AgentEvent::Cached(6_000));
        agent.connection = "work".into();
        session.keep_choice(&agent);
        assert_eq!(session.cached_until, 0);
        // Left out of the file while it is not known.
        assert!(!serde_json::to_string(&session).unwrap().contains("cached_until"));
    }

    #[test]
    fn a_turn_keeps_what_its_requests_used_and_the_session_what_was_counted() {
        let spent = |input, cost| Spent { model: "opus".into(), usage: Usage::new(input, 10), cost };
        let mut session = Session::new("scout");
        session.entries.push(Entry::User { text: "go".into(), note: String::new(), files: Vec::new() });
        // Saved without either while no engine says them: an older file reads the same.
        let plain = serde_json::to_string(&session).unwrap();
        assert!(!plain.contains("spent") && !plain.contains("metered"));
        session.apply(&crate::AgentEvent::ReplyStarted);
        session.apply(&crate::AgentEvent::Spent { turn: vec![spent(8_000, 0.5)], total: vec![spent(90_000, 4.0)] });
        session.apply(&ended());
        let Some(Entry::Reply(reply)) = session.entries.last() else { panic!("a reply") };
        // The reply says how full the context is; its turn used every request made for it.
        assert_eq!((reply.usage, reply.used()), (Usage::new(100, 20), Usage::new(8_000, 10)));
        assert_eq!((session.context_tokens(), session.tokens()), (120, 8_010));
        assert_eq!(session.metered, [spent(90_000, 4.0)]);
        // An engine that keeps no count of the session leaves the one there is.
        session.apply(&crate::AgentEvent::ReplyStarted);
        session.apply(&crate::AgentEvent::Spent { turn: vec![spent(500, 0.0)], total: Vec::new() });
        session.apply(&ended());
        assert_eq!((session.metered.len(), session.tokens()), (1, 8_520));
        let read: Session = serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!((&read.metered, &read.entries), (&session.metered, &session.entries));
    }

    #[test]
    fn a_session_keeps_its_provider_model_and_effort() {
        let mut agent = Agent::new("Scout");
        (agent.connection, agent.model, agent.effort) = ("claude".into(), "opus".into(), "max".into());
        let mut session = Session::new(&agent.id);
        let mut seen = agent.clone();
        session.apply_choice(&mut seen, true);
        assert_eq!(seen.model, "opus", "a session that chose nothing thinks as its agent does");
        session.keep_choice(&agent);
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!((back.provider.as_str(), back.model.as_str(), back.effort.as_str()), ("claude", "opus", "max"));
        // The sidebar knows it without the messages.
        let summary = back.summary();
        assert_eq!(
            (summary.provider.as_str(), summary.model.as_str(), summary.effort.as_str()),
            ("claude", "opus", "max")
        );
        // The agent goes on to another provider and model; the session keeps what it was left with.
        (agent.connection, agent.model, agent.effort) = ("openai".into(), "gpt".into(), String::new());
        let mut seen = agent.clone();
        back.apply_choice(&mut seen, true);
        assert_eq!((seen.connection.as_str(), seen.model.as_str(), seen.effort.as_str()), ("claude", "opus", "max"));
        // Its provider was deleted: it thinks as its agent does.
        let mut seen = agent.clone();
        back.apply_choice(&mut seen, false);
        assert_eq!((seen.connection.as_str(), seen.model.as_str(), seen.effort.as_str()), ("openai", "gpt", ""));
    }

    #[test]
    fn an_index_of_another_format_is_made_again() {
        let mut store = store("index-format");
        let mut kept = Session::new("scout");
        (kept.provider, kept.model) = ("claude".into(), "opus".into());
        store.save(&kept).unwrap();
        // As a version before this one wrote it: no provider in the summaries.
        let old = format!(r#"{{"version":1,"sessions":[{{"id":"{}","agent":"scout","title":"Old"}}]}}"#, kept.id);
        fs::write(store.dir().join(INDEX_FILE), old).unwrap();
        let (listed, errors) = SessionStore::at(store.dir().to_path_buf()).list().unwrap();
        assert!(errors.is_empty());
        assert_eq!((listed.len(), listed[0].provider.as_str(), listed[0].model.as_str()), (1, "claude", "opus"));
        let written: Index = serde_json::from_slice(&fs::read(store.dir().join(INDEX_FILE)).unwrap()).unwrap();
        assert_eq!(written.version, INDEX_VERSION);
    }

    fn session(agent: &str, prompt: &str, answer: &str) -> Session {
        let mut session = Session::new(agent);
        session.entries.push(Entry::User { text: prompt.into(), note: String::new(), files: Vec::new() });
        session.entries.push(Entry::from(Reply {
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
        let reply = |blocks| Entry::from(Reply { blocks, ..Reply::default() });
        session.entries.push(reply(vec![
            create("Read the file", Some("Task #1 created successfully: Read the file")),
            create("Fix the bug", Some("Task #2 created successfully: Fix the bug")),
            create("Not answered yet", None),
            update("1", "completed"),
            update("2", "in_progress"),
            update("9", "completed"),
            call(
                "TaskUpdate",
                serde_json::json!({ "taskId": "2", "subject": "Fix both bugs", "description": "In the parser" }),
                None,
            ),
            call("TaskUpdate", serde_json::json!("garbage"), None),
        ]));
        let todos = session.todos();
        let seen: Vec<_> = todos.iter().map(|todo| (todo.id.as_str(), todo.subject.as_str(), todo.state)).collect();
        assert_eq!(seen, [("1", "Read the file", TodoState::Done), ("2", "Fix both bugs", TodoState::Active)]);
        assert_eq!(todos[1].doing, "Fix the bug…");
        assert_eq!(todos[1].detail, "In the parser", "what the agent wrote of it is kept for its tooltip");

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

    use crate::AgentEvent;

    /// Numbers that look thrown, the same at every run.
    struct Dice(u64);

    impl Dice {
        /// One of `0..sides`.
        fn roll(&mut self, sides: usize) -> usize {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as usize % sides
        }
    }

    /// A tool call that took `input` bytes and returned `output` bytes.
    fn call(id: &str, input: usize, output: usize) -> ToolUse {
        ToolUse {
            id: id.into(),
            name: "read".into(),
            input: format!("{{\"path\":\"{}\"}}", "a".repeat(input)),
            output: Some("b\n".repeat(output / 2)),
            ..ToolUse::default()
        }
    }

    /// Adds a turn to `session`: a prompt, and a reply that called a tool which returned `output` bytes.
    fn turn(session: &mut Session, prompt: &str, output: usize) {
        session.prompt(prompt, "");
        session.apply(&AgentEvent::ReplyStarted);
        session.apply(&AgentEvent::ToolCall(call(&format!("call-{}", session.entries.len()), 16, output)));
        session.apply(&AgentEvent::Text(format!("Done with {prompt}.")));
        session.apply(&ended());
    }

    fn ended() -> AgentEvent {
        AgentEvent::ReplyEnded {
            raw: vec![serde_json::json!({ "type": "text", "text": "Done." })],
            wire: Wire::Messages,
            model: "opus".into(),
            usage: Usage::new(100, 20),
            cost: 0.25,
            problem: String::new(),
        }
    }

    /// The session as a store that never saw it reads it from `store`'s folder.
    fn read_back(store: &SessionStore, id: &str) -> Result<Session> {
        SessionStore::at(store.dir().to_path_buf()).load(id)
    }

    /// The bytes of a session's JSON file and of its log (none: empty).
    fn files(store: &SessionStore, id: &str) -> (Vec<u8>, Vec<u8>) {
        let (json, log) = store.paths(id).unwrap();
        (fs::read(json).unwrap(), fs::read(log).unwrap_or_default())
    }

    /// How many entries a session's JSON file says its log keeps.
    fn sealed(store: &SessionStore, id: &str) -> u64 {
        serde_json::from_slice::<Count>(&files(store, id).0).unwrap().sealed
    }

    #[test]
    fn whatever_changes_between_saves_is_read_back_as_it_was_saved() {
        let mut writer = store("changes");
        let dir = writer.dir().to_path_buf();
        let mut session = Session::new("scout");
        let mut dice = Dice(7);
        let (mut calls, mut agents) = (Vec::<String>::new(), Vec::<String>::new());
        // How often the log grew, was cut back, and was left as it was.
        let (mut grew, mut shrank, mut stayed, mut length) = (0, 0, 0, 0);
        for step in 0..1500 {
            let any = |dice: &mut Dice, ids: &[String]| ids.get(dice.roll(ids.len().max(1))).cloned();
            match dice.roll(16) {
                0 | 1 => session.prompt(&format!("prompt {step}"), ""),
                2 => {
                    session.apply(&AgentEvent::ReplyStarted);
                    session.apply(&AgentEvent::Thinking("Hm.".into()));
                    session.apply(&AgentEvent::Text(format!("Answer {step}.\nOn two lines, with a \"quote\".")));
                }
                3 => session.apply(&AgentEvent::Text(format!(" More at {step}."))),
                4 | 5 => {
                    let id = format!("call-{step}");
                    let agent = dice.roll(3) == 0;
                    let name = if agent { "Agent" } else { "read" };
                    let call = ToolUse { id: id.clone(), name: name.into(), input: "{}".into(), ..ToolUse::default() };
                    session.apply(&AgentEvent::ToolCall(call));
                    if agent {
                        agents.push(id.clone());
                    }
                    calls.push(id);
                }
                // A result for a call of any earlier entry, as a subagent in the background gives one.
                6 | 7 => {
                    if let Some(id) = any(&mut dice, &calls) {
                        session.apply(&AgentEvent::ToolResult {
                            id,
                            output: format!("result {step}"),
                            is_error: false,
                        });
                    }
                }
                8 | 9 => {
                    if let Some(parent) = any(&mut dice, &agents) {
                        let id = format!("sub-{step}");
                        let event = match dice.roll(3) {
                            0 => AgentEvent::Text(format!("Looking at {step}.")),
                            1 => AgentEvent::Thinking("Where is it?".into()),
                            _ => {
                                calls.push(id.clone());
                                AgentEvent::ToolCall(call(&id, 8, 8))
                            }
                        };
                        session.apply(&AgentEvent::Sub { parent: parent.clone(), event: Box::new(event) });
                        session.apply(&AgentEvent::Activity { id: parent, text: format!("Working · {step}") });
                    }
                }
                10 => session.apply(&ended()),
                11 => match dice.roll(4) {
                    // A summary put before everything, and the engine's own before the prompt.
                    0 => session.entries.insert(0, Entry::Summary { text: format!("Until {step}.") }),
                    1 => session.apply(&AgentEvent::Summarized(format!("So far, at {step}."))),
                    _ => session.apply(&AgentEvent::Note(format!("Summarised at {step}."))),
                },
                12 => {
                    // As `/clear` empties it.
                    if dice.roll(6) == 0 {
                        session.entries.clear();
                        calls.clear();
                        agents.clear();
                    }
                }
                13 => match dice.roll(3) {
                    0 => session.attach(&[format!("/tmp/{step}.png")]),
                    1 => session.apply(&AgentEvent::FollowUp {
                        id: format!("follow-{step}"),
                        text: format!("and {step}"),
                        note: String::new(),
                        files: Vec::new(),
                    }),
                    _ => {
                        if let Some(id) = any(&mut dice, &calls) {
                            session.apply(&AgentEvent::Denied(id));
                        }
                    }
                },
                14 => match dice.roll(8) {
                    0 => session.apply(&AgentEvent::ForgetReplays),
                    1 => session.apply(&AgentEvent::Blind),
                    2 => {
                        session.entries.pop();
                    }
                    _ => {
                        if let Some(id) = any(&mut dice, &calls) {
                            let picture = Picture { media_type: "image/png".into(), data: "AAAA".into() };
                            session.apply(&AgentEvent::ToolPicture { id, picture });
                        }
                    }
                },
                _ => {}
            }
            if dice.roll(3) > 0 {
                continue;
            }
            writer.save(&session).unwrap();
            assert_eq!(read_back(&writer, &session.id).unwrap(), session, "after step {step}");
            let now = files(&writer, &session.id).1.len();
            match now.cmp(&length) {
                std::cmp::Ordering::Greater => grew += 1,
                std::cmp::Ordering::Less => shrank += 1,
                std::cmp::Ordering::Equal => stayed += 1,
            }
            length = now;
            match dice.roll(12) {
                // The app is started again: the session is read, and the writer told what was found.
                0 => {
                    let (loaded, kept) = SessionStore::at(dir.clone()).load_kept(&session.id).unwrap();
                    session = loaded;
                    writer = SessionStore::at(dir.clone());
                    writer.adopt(&session.id, kept);
                }
                // A writer that knows nothing of the files, and one that let go of what it knew.
                1 => writer = SessionStore::at(dir.clone()),
                2 => writer.forget(&session.id),
                _ => {}
            }
        }
        assert!(grew > 20 && shrank > 20 && stayed > 20, "{grew} grew, {shrank} shrank, {stayed} stayed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_turn_more_is_added_to_the_log_and_what_it_held_is_left_alone() {
        let mut store = store("appends");
        let mut session = Session::new("scout");
        for number in 0..40 {
            turn(&mut session, &format!("turn {number}"), 100_000);
        }
        // A store that knows nothing writes everything to the JSON file; the turns that came through
        // that save unchanged are sealed by the next.
        store.save(&session).unwrap();
        assert_eq!((sealed(&store, &session.id), files(&store, &session.id).1.len()), (0, 0));
        turn(&mut session, "one more", 1_000);
        store.save(&session).unwrap();
        let (json, log) = files(&store, &session.id);
        assert_eq!(sealed(&store, &session.id), 80);
        assert!(log.len() > 4_000_000 && json.len() < 4_000, "{} in the log, {} beside it", log.len(), json.len());

        turn(&mut session, "and another", 1_000);
        store.save(&session).unwrap();
        let (json, longer) = files(&store, &session.id);
        assert_eq!(sealed(&store, &session.id), 82);
        assert!(longer.len() > log.len() && longer.len() < log.len() + 4_000, "only the turn before was added");
        assert!(longer[..log.len()] == log[..], "what the log held is as it was");
        assert!(json.len() < 4_000, "{} beside it", json.len());
        assert_eq!(read_back(&store, &session.id).unwrap(), session);

        // And so for a session the app opens: the writer is handed what the read found.
        let (mut opened, kept) = SessionStore::at(store.dir().to_path_buf()).load_kept(&session.id).unwrap();
        assert_eq!(opened, session);
        let mut writer = SessionStore::at(store.dir().to_path_buf());
        writer.adopt(&opened.id, kept);
        turn(&mut opened, "after opening", 1_000);
        writer.save(&opened).unwrap();
        let (json, last) = files(&writer, &opened.id);
        assert_eq!(sealed(&writer, &opened.id), 84);
        assert!(last.len() > longer.len() && last.len() < longer.len() + 4_000 && json.len() < 4_000);
        assert!(last[..longer.len()] == longer[..], "the first save after opening added to the log too");
        assert_eq!(read_back(&writer, &opened.id).unwrap(), opened);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    #[ignore = "prints what a save costs for a session of some fifty megabytes; run it in release"]
    fn saving_a_turn_writes_only_the_turn() {
        use std::time::Instant;
        // As the owner's longest: 150 entries, 1,000 tool calls of 1.6 KB in and 2.4 KB out, and 90
        // subagents that each made 100 calls of 4 KB.
        let mut session = Session::new("scout");
        for number in 0..75 {
            session.prompt(&format!("turn {number}"), "");
            session.apply(&AgentEvent::ReplyStarted);
            let calls = if number < 25 { 14 } else { 13 };
            for tool in 0..calls {
                session.apply(&AgentEvent::ToolCall(call(&format!("call-{number}-{tool}"), 1_600, 2_400)));
            }
            let agents = if number < 15 { 2 } else { 1 };
            for agent in 0..agents {
                let sub = (0..100).map(|tool| Block::Tool(call(&format!("sub-{number}-{agent}-{tool}"), 1_600, 2_400)));
                let agent = ToolUse {
                    name: "Agent".into(),
                    sub: sub.collect(),
                    ..call(&format!("agent-{number}-{agent}"), 200, 2_000)
                };
                session.apply(&AgentEvent::ToolCall(agent));
            }
            session.apply(&AgentEvent::Text(format!("Done with turn {number}.")));
            session.apply(&ended());
        }
        let tools = |blocks: &[Block]| blocks.iter().filter(|block| matches!(block, Block::Tool(_))).count();
        let (own, agents): (usize, usize) = session.replies().fold((0, 0), |(own, agents), reply| {
            let started =
                reply.blocks.iter().filter(|b| matches!(b, Block::Tool(call) if !call.sub.is_empty())).count();
            (own + tools(&reply.blocks) - started, agents + started)
        });
        assert_eq!((session.entries.len(), own, agents), (150, 1_000, 90));

        // As it was: every entry copied on the UI thread, then all of them written anew.
        let start = Instant::now();
        let copied: Vec<Entry> = session
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Reply(reply) => Entry::from(Reply::clone(reply)),
                other => other.clone(),
            })
            .collect();
        let copying = start.elapsed();
        let start = Instant::now();
        let whole = serde_json::to_vec(&session).unwrap();
        let serialising = start.elapsed();
        drop(copied);

        let mut store = store("one-turn");
        store.save(&session).unwrap();
        turn(&mut session, "sealing what came before", 2_400);
        store.save(&session).unwrap();
        let log = files(&store, &session.id).1.len();

        // As it is: the pointers copied, and the turn written.
        turn(&mut session, "one more", 2_400);
        let start = Instant::now();
        let snapshot = session.clone();
        let cloning = start.elapsed();
        let start = Instant::now();
        store.save(&snapshot).unwrap();
        let saving = start.elapsed();
        let (json, longer) = files(&store, &session.id);
        let written = json.len() + longer.len() - log;
        println!("a session of {} entries, {} bytes as one file", session.entries.len(), whole.len());
        println!("before: {copying:?} to copy it, {serialising:?} to serialise it, {} bytes written", whole.len());
        println!("now:    {cloning:?} to copy it, {saving:?} to save it, {written} bytes written");
        println!("        ({} to the log of {log} bytes, {} as the JSON file)", longer.len() - log, json.len());
        assert!(written < 20_000 && whole.len() > 40_000_000, "{written} of {}", whole.len());
        assert_eq!(read_back(&store, &session.id).unwrap(), session);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_crash_while_adding_to_the_log_leaves_the_session_as_it_was() {
        let mut store = store("crash-adding");
        let mut session = Session::new("scout");
        turn(&mut session, "one", 100);
        store.save(&session).unwrap();
        turn(&mut session, "two", 100);
        store.save(&session).unwrap();
        let (old, id) = (session.clone(), session.id.clone());
        let (old_json, old_log) = files(&store, &id);
        assert_eq!(sealed(&store, &id), 2);
        turn(&mut session, "three", 100);
        store.save(&session).unwrap();
        let (new_json, new_log) = files(&store, &id);
        assert!(sealed(&store, &id) == 4 && new_log.len() > old_log.len());
        let (json, log) = store.paths(&id).unwrap();

        // Between the steps of that save: the lines added, whole or torn, with the JSON file not yet replaced.
        fs::write(&json, &old_json).unwrap();
        for added in [new_log.len(), new_log.len() - 1, old_log.len() + 7, old_log.len()] {
            fs::write(&log, &new_log[..added]).unwrap();
            assert_eq!(read_back(&store, &id).unwrap(), old, "with {} bytes added", added - old_log.len());
        }
        // Then the JSON file replaced: the session as it is now.
        fs::write(&log, &new_log).unwrap();
        fs::write(&json, &new_json).unwrap();
        assert_eq!(read_back(&store, &id).unwrap(), session);

        // The app starts again where the crash left a torn line: what is added next goes after the
        // lines that count, not after the torn one.
        fs::write(&json, &old_json).unwrap();
        fs::write(&log, &new_log[..old_log.len() + 7]).unwrap();
        let (mut opened, kept) = SessionStore::at(store.dir().to_path_buf()).load_kept(&id).unwrap();
        let mut writer = SessionStore::at(store.dir().to_path_buf());
        writer.adopt(&id, kept);
        turn(&mut opened, "three again", 100);
        writer.save(&opened).unwrap();
        assert_eq!(sealed(&writer, &id), 4);
        assert!(files(&writer, &id).1.starts_with(&old_log));
        assert_eq!(read_back(&writer, &id).unwrap(), opened);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_crash_before_the_log_is_cut_back_leaves_the_session_as_it_was_saved() {
        let mut store = store("crash-cutting");
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
            store.save(&session).unwrap();
        }
        let id = session.id.clone();
        let (old, (old_json, old_log)) = (session.clone(), files(&store, &id));
        assert_eq!(sealed(&store, &id), 4);
        // The first turn's call is answered anew, as a task in the background answers late: a sealed
        // entry changed, so the count goes back to before it and the log is cut there.
        session.apply(&AgentEvent::ToolResult { id: "call-2".into(), output: "at last".into(), is_error: false });
        store.save(&session).unwrap();
        let (new_json, new_log) = files(&store, &id);
        assert_eq!(sealed(&store, &id), 1);
        assert!(new_log.len() < old_log.len() && old_log.starts_with(&new_log));
        let (json, log) = store.paths(&id).unwrap();

        // Before the JSON file was replaced: as it was. After it, with the log not yet cut: as it is.
        fs::write(&json, &old_json).unwrap();
        fs::write(&log, &old_log).unwrap();
        assert_eq!(read_back(&store, &id).unwrap(), old);
        fs::write(&json, &new_json).unwrap();
        assert_eq!(read_back(&store, &id).unwrap(), session);

        // The app starts again with the log still long: the lines no longer counted are cut off
        // before the next is added.
        let (mut opened, kept) = SessionStore::at(store.dir().to_path_buf()).load_kept(&id).unwrap();
        let mut writer = SessionStore::at(store.dir().to_path_buf());
        writer.adopt(&id, kept);
        turn(&mut opened, "four", 100);
        writer.save(&opened).unwrap();
        assert_eq!(sealed(&writer, &id), 6);
        assert_eq!(read_back(&writer, &id).unwrap(), opened);
        let lines = files(&writer, &id).1.split_inclusive(|byte| *byte == b'\n').count();
        assert_eq!(lines, 6, "nothing of the old lines is left after the new");
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_session_of_the_version_before_loads_and_is_written_as_this_one() {
        let mut store = store("version-1");
        let mut old = Session::new("scout");
        turn(&mut old, "one", 100);
        turn(&mut old, "two", 100);
        old.version = 1;
        // As version 1 wrote it: every entry in the one file. A count it never wrote means nothing.
        let (json, log) = store.paths(&old.id).unwrap();
        fs::create_dir_all(store.dir()).unwrap();
        let mut text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("sealed"));
        text.insert_str(1, "\"sealed\":3,");
        fs::write(&json, text).unwrap();

        let (mut session, kept) = store.load_kept(&old.id).unwrap();
        assert_eq!(session, Session { version: Session::VERSION, ..old.clone() });
        assert!(!log.exists());
        store.adopt(&session.id, kept);
        turn(&mut session, "three", 100);
        store.save(&session).unwrap();
        // Everything before the new turn went to the log at once, and the file says who wrote it.
        assert_eq!(sealed(&store, &old.id), 4);
        let written: Session = serde_json::from_slice(&files(&store, &old.id).0).unwrap();
        assert_eq!((written.version, written.entries.len()), (Session::VERSION, 2));
        assert_eq!(read_back(&store, &old.id).unwrap(), session);

        // One held since before it was this version is written as this version too.
        let mut held = Session { version: 1, ..Session::new("scout") };
        turn(&mut held, "one", 100);
        store.save(&held).unwrap();
        assert_eq!(read_back(&store, &held.id).unwrap().version, Session::VERSION);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_hostile_log_is_an_unreadable_session_and_never_a_panic() {
        let mut store = store("hostile-log");
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
            store.save(&session).unwrap();
        }
        let id = session.id.clone();
        let (good_json, good_log) = files(&store, &id);
        assert_eq!(sealed(&store, &id), 4);
        let (json, log) = store.paths(&id).unwrap();
        let lines: Vec<&[u8]> = good_log.split_inclusive(|byte| *byte == b'\n').collect();
        let counting = |count: &str| String::from_utf8(good_json.clone()).unwrap().replace("\"sealed\":4", count);

        // What follows the lines that count is not read: garbage, and a line a crash tore.
        let mut longer = good_log.clone();
        longer.extend_from_slice(b"garbage\n\xff\xfe\n{\"type\":\"user\",\"text\":\"to");
        fs::write(&log, &longer).unwrap();
        assert_eq!(read_back(&store, &id).unwrap(), session);

        let mut garbage = lines.concat();
        garbage[lines[0].len()] = b'!';
        let huge = [lines[0], &vec![b'x'; 16 << 20], b"\n", lines[2], lines[3]].concat();
        let nested = [lines[0], "[".repeat(1 << 20).as_bytes(), b"\n", lines[2], lines[3]].concat();
        let wrong = [lines[0], b"{\"type\":\"reply\",\"blocks\":7}\n", lines[2], lines[3]].concat();
        let torn = &good_log[..good_log.len() - 1];
        let fewer = lines[..3].concat();
        let blank = [lines[0], b"\n", lines[2], lines[3]].concat();
        let logs: [(&str, &[u8]); 8] = [
            ("a line of garbage", &garbage),
            ("a huge line", &huge),
            ("a line nested without end", &nested),
            ("a line of another shape", &wrong),
            ("a last line torn", torn),
            ("fewer lines than counted", &fewer),
            ("an empty line", &blank),
            ("nothing", b""),
        ];
        for (what, bytes) in logs {
            fs::write(&log, bytes).unwrap();
            assert!(read_back(&store, &id).is_err(), "{what}");
            assert!(SessionStore::at(store.dir().to_path_buf()).load_kept(&id).is_err(), "{what}");
            assert_eq!(
                files(&store, &id),
                (good_json.clone(), bytes.to_vec()),
                "{what}: the files are left as they are"
            );
        }
        fs::remove_file(&log).unwrap();
        assert!(read_back(&store, &id).is_err(), "no log at all");

        // A count that is no count, or more than any log holds.
        fs::write(&log, &good_log).unwrap();
        for count in
            ["\"sealed\":18446744073709551615", "\"sealed\":-1", "\"sealed\":\"4\"", "\"sealed\":4.5", "\"sealed\":5"]
        {
            fs::write(&json, counting(count)).unwrap();
            assert!(read_back(&store, &id).is_err(), "{count}");
        }
        // It is listed as unreadable, not shown, and found by no search; the other sessions are.
        let mut other = Session::new("scout");
        turn(&mut other, "findable", 100);
        store.save(&other).unwrap();
        // Without the index, which still has it from when it was saved.
        fs::remove_file(store.dir().join(INDEX_FILE)).unwrap();
        let mut fresh = SessionStore::at(store.dir().to_path_buf());
        let (listed, errors) = fresh.list().unwrap();
        assert_eq!((listed.len(), errors.len(), listed[0].id.as_str()), (1, 1, other.id.as_str()));
        assert_eq!(fresh.search("done with", 10).unwrap().len(), 1);
        assert_eq!(fresh.all().len(), 1);
        assert_eq!(files(&store, &id).1, good_log);

        // Put right, it reads again.
        fs::write(&json, &good_json).unwrap();
        assert_eq!(read_back(&store, &id).unwrap(), session);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_store_that_knows_nothing_of_a_log_writes_the_session_whole() {
        let mut store = store("knows-nothing");
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
            store.save(&session).unwrap();
        }
        let id = session.id.clone();
        assert!(sealed(&store, &id) == 4 && !files(&store, &id).1.is_empty());

        // It was never told what the files hold, and the session it is given differs from them
        // where they are sealed: nothing of the log can be trusted to be the session's.
        let mut writer = SessionStore::at(store.dir().to_path_buf());
        session.apply(&AgentEvent::ToolResult { id: "call-2".into(), output: "changed".into(), is_error: false });
        turn(&mut session, "four", 100);
        writer.save(&session).unwrap();
        assert_eq!((sealed(&writer, &id), files(&writer, &id).1.len()), (0, 0));
        assert_eq!(read_back(&writer, &id).unwrap(), session);
        // From then on it knows, and seals again.
        turn(&mut session, "five", 100);
        writer.save(&session).unwrap();
        assert_eq!(sealed(&writer, &id), 8);
        assert_eq!(read_back(&writer, &id).unwrap(), session);

        // So does one that let go of what it knew.
        writer.forget(&id);
        turn(&mut session, "six", 100);
        writer.save(&session).unwrap();
        assert_eq!((sealed(&writer, &id), files(&writer, &id).1.len()), (0, 0));
        assert_eq!(read_back(&writer, &id).unwrap(), session);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn what_a_read_found_is_not_taken_once_the_files_changed() {
        let mut writer = store("stale");
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
            writer.save(&session).unwrap();
        }
        let id = session.id.clone();
        // Read while a save was still waiting to be written: the files are not what was found.
        let (mut stale, kept) = SessionStore::at(writer.dir().to_path_buf()).load_kept(&id).unwrap();
        session.apply(&AgentEvent::ToolResult { id: "call-2".into(), output: "late".into(), is_error: false });
        turn(&mut session, "four", 100);
        writer.save(&session).unwrap();
        turn(&mut session, "five", 100);
        writer.save(&session).unwrap();
        writer.forget(&id);
        writer.adopt(&id, kept);
        // Had it been taken, the turn would be added after lines of the log that are no longer there.
        turn(&mut stale, "from what was read", 100);
        writer.save(&stale).unwrap();
        assert_eq!(sealed(&writer, &id), 0, "written whole");
        assert_eq!(read_back(&writer, &id).unwrap(), stale);

        // Nor is a missing session's, or one of another name.
        let (_, kept) = SessionStore::at(writer.dir().to_path_buf()).load_kept(&id).unwrap();
        writer.forget(&id);
        writer.adopt("../etc", kept);
        let (_, kept) = SessionStore::at(writer.dir().to_path_buf()).load_kept(&id).unwrap();
        writer.adopt("missing", kept);
        assert!(writer.written.is_empty());
        fs::remove_dir_all(writer.dir()).unwrap();
    }

    #[test]
    fn deleting_a_session_removes_its_log_too() {
        let mut store = store("delete-log");
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
            store.save(&session).unwrap();
        }
        let (json, log) = store.paths(&session.id).unwrap();
        assert!(json.exists() && log.exists());
        // Its log is no session of its own: it is listed and found once.
        assert_eq!(store.list().unwrap().0.len(), 1);
        assert_eq!(store.search("done with one", 10).unwrap().len(), 1, "what the log holds is searched");
        assert_eq!(store.all(), [session.clone()]);
        store.delete(&session.id).unwrap();
        assert!(!json.exists() && !log.exists() && store.written.is_empty());
        assert!(store.list().unwrap().0.is_empty());
        // Saved again after that, it is written whole: nothing of the files is remembered.
        store.save(&session).unwrap();
        assert_eq!(sealed(&store, &session.id), 0);
        assert_eq!(read_back(&store, &session.id).unwrap(), session);
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn a_session_read_while_it_is_saved_is_one_that_was_saved() {
        /// What the `number`th save adds: a turn, or a late answer to the first call, by turns.
        fn change(session: &mut Session, number: u64) {
            session.window = number;
            if number % 2 == 1 {
                let output = format!("answered at {number}");
                session.apply(&AgentEvent::ToolResult { id: "call-0".into(), output, is_error: false });
                return;
            }
            let (id, text) = (format!("call-{}", session.entries.len() / 2), format!("turn {number}"));
            session.entries.push(Entry::User { text: text.clone(), note: String::new(), files: Vec::new() });
            session.entries.push(Entry::from(Reply {
                blocks: vec![Block::Tool(call(&id, 100, 2_000)), Block::Text { text }],
                ..Reply::default()
            }));
        }
        const SAVES: u64 = 120;
        let mut writer = store("read-while-saved");
        let reader = SessionStore::at(writer.dir().to_path_buf());
        let mut session = Session::new("scout");
        change(&mut session, 0);
        writer.save(&session).unwrap();
        let id = session.id.clone();
        let done = std::sync::atomic::AtomicBool::new(false);
        // Every other save cuts the log back to its first line and the next adds the lines again,
        // the second of them changed: a reader that took its count from one save and its lines from
        // another would hold a session that never was.
        let read = std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                let (mut read, mut expected) = (0, Session::default());
                while !done.load(Ordering::Relaxed) {
                    // The writer swapping a file under the reader's hands may fail the read as a
                    // whole; what does get read is what counts here.
                    let Ok(loaded) = reader.load(&id) else { continue };
                    if loaded.window < expected.window || expected.entries.is_empty() {
                        expected = Session::default();
                        change(&mut expected, 0);
                    }
                    for number in expected.window + 1..=loaded.window {
                        change(&mut expected, number);
                    }
                    assert_eq!(loaded.entries, expected.entries, "as it was after save {}", loaded.window);
                    read += 1;
                }
                read
            });
            for number in 1..=SAVES {
                change(&mut session, number);
                // Not asserted on: where the reader holds a file open, replacing it may be refused.
                let _ = writer.save(&session);
            }
            done.store(true, Ordering::Relaxed);
            reading.join().unwrap()
        });
        assert!(read > 0, "nothing was read");
        writer.save(&session).unwrap();
        assert_eq!(read_back(&writer, &id).unwrap(), session);
        fs::remove_dir_all(writer.dir()).unwrap();
    }

    #[test]
    fn a_copy_of_a_session_shares_its_replies_until_one_changes() {
        let mut session = Session::new("scout");
        for prompt in ["one", "two", "three"] {
            turn(&mut session, prompt, 100);
        }
        let saved = session.clone();
        let shared = |session: &Session| -> Vec<bool> {
            session.entries.iter().zip(&saved.entries).map(|(now, then)| now.is(then)).collect()
        };
        assert!(shared(&session).iter().all(|same| *same));
        // A result for a call two turns back: only the reply that holds the call becomes another.
        session.apply(&AgentEvent::ToolResult { id: "call-2".into(), output: "late".into(), is_error: false });
        assert_eq!(shared(&session), [true, false, true, true, true, true]);
        assert!(session.find_tool("nowhere").is_none());
        assert_eq!(shared(&session), [true, false, true, true, true, true], "looking changes nothing");
        // Text goes to the last reply alone; what has nothing to forget is not touched.
        session.apply(&AgentEvent::Text("More.".into()));
        assert_eq!(shared(&session), [true, false, true, true, true, false]);
        let before = session.clone();
        session.apply(&AgentEvent::Blind);
        assert!(session.entries.iter().zip(&before.entries).all(|(now, then)| now.is(then)), "no picture was shown");
        // The copy still reads as it did when it was made.
        let Entry::Reply(reply) = &saved.entries[1] else { panic!("a reply") };
        assert!(matches!(&reply.blocks[0], Block::Tool(call) if call.output.as_deref() != Some("late")));
        assert!(Entry::Summary { text: "a".into() }.is(&Entry::Summary { text: "a".into() }));
        assert!(!Entry::from(Reply::default()).is(&Entry::from(Reply::default())), "equal, but not the same");
    }
}
