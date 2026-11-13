//! The tools an agent has when it thinks through an API (the Claude Agent
//! brings its own): reading, writing and editing files (and looking at
//! pictures), listing and searching folders, running commands (in the
//! background too, see [`Jobs`]) and fetching pages; and, when the library
//! has skills, loading one ([`crate::skills::TOOL`]). Commands run in bash
//! where there is one (Git for Windows' on Windows), which models write
//! best, and in PowerShell on a Windows without it.
//!
//! Each call is a typed action, so the app can judge it before it runs
//! ([`risk`]) and show what it did. Outputs are bounded: a long file is read
//! in pages and a long command output is cut in the middle, with a note
//! saying so, which keeps one careless call from flooding the context. They
//! are also made to save the model a step: `search` takes a regular
//! expression, leaves out what Git ignores and can name the files alone,
//! `edit` makes several changes at once and says which lines they are now,
//! and a command's output that was cut is kept whole in a file to search.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::attach;
use crate::config::Config;
use crate::pattern::Pattern;
use crate::responses::Picture;
use crate::skills::{self, Skill};

/// Most lines one `read` returns.
const READ_LINES: usize = 1_500;
/// Most characters any tool returns.
const OUTPUT_CHARS: usize = 30_000;
/// Largest file `read`, `edit` and `search` open, in bytes.
const MAX_FILE: u64 = 4 << 20;
/// Most entries `list` returns and most matches `search` returns.
const MAX_ENTRIES: usize = 400;
/// Most files and folders `list` and `search` look at before giving up.
const MAX_VISITS: usize = 50_000;
/// How deep `list` and `search` descend.
const MAX_DEPTH: usize = 24;
/// Longest line `read` returns whole, in characters.
const LINE_CHARS: usize = 2_000;
/// How much of a line `search` looks at and shows: bytes of it, then characters.
const SEARCH_LINE: (usize, usize) = (10_000, 240);
/// Most lines `search` shows before and after a match.
const MAX_CONTEXT: usize = 10;
/// How long one `search` may take.
const SEARCH_TIME: Duration = Duration::from_secs(20);
/// Most places an `edit` names in its answer.
const MAX_PLACES: usize = 12;
/// How much of a cut command output is its start; the rest is its end,
/// where a build or a test run says what went wrong.
const SHELL_HEAD: usize = OUTPUT_CHARS / 4;
/// Most command outputs kept whole on disk at a time.
const KEPT_OUTPUTS: usize = 20;
/// What the folder of this process's kept outputs is named by, in the temporary folder.
const OUTPUTS: &str = "atroxabot-output-";
/// How long another process's kept outputs are left alone.
const OUTPUTS_AGE: Duration = Duration::from_secs(24 * 3600);
/// How long a command may run unless it asks for longer, and the most it may ask for.
const SHELL_TIMEOUT: (u64, u64) = (120, 900);
/// Most a background command's output file keeps, in bytes; what it prints
/// past that is let go.
const JOB_OUTPUT: u64 = 32 << 20;
/// Most commands one session keeps running in the background.
const MAX_JOBS: usize = 8;
/// How long `shell` watches a background command before it answers: one
/// that fails at once says so.
const JOB_START: Duration = Duration::from_secs(2);
/// Longest `job` waits for a command to end, in seconds.
const JOB_WAIT: u64 = 600;
/// Largest page `fetch` reads, in bytes.
const MAX_PAGE: u64 = 2 << 20;
/// Folders that are never worth listing or searching.
const SKIPPED: [&str; 6] = [".git", "node_modules", "target", ".venv", "__pycache__", ".next"];

/// Where an agent works and what it may touch.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// The folder relative paths start from.
    pub dir: PathBuf,
    /// The agent's notes folder, which it may always read and write.
    pub memory: Option<PathBuf>,
    /// The folder of the installed skills, which it may read.
    pub library: Option<PathBuf>,
    /// The skills it has, as they were when its session began.
    pub skills: Arc<Vec<Skill>>,
    /// Whether the model cannot look at pictures: `read` then says so
    /// rather than show one.
    pub blind: bool,
    /// The commands its session runs in the background.
    pub jobs: Arc<Jobs>,
    /// The files it read or wrote, as they were on disk then: what tells
    /// `edit` and `write` that one was changed since by something else.
    seen: Arc<Mutex<HashMap<PathBuf, Stamp>>>,
    /// The picture the latest `read` made ready for the model, and the
    /// file it is of: [`picture`] takes it, so one that had to be scaled
    /// down is only scaled once.
    shown: Arc<Mutex<Option<(PathBuf, Picture)>>>,
}

/// What tells one state of a file on disk from another: when it was last
/// written, and how long it is.
type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    fs::metadata(path).ok().map(|meta| (meta.modified().ok(), meta.len()))
}

impl Workspace {
    /// The workspace in `dir` with notes in `memory`, both as the real paths
    /// (links followed) that tool arguments are compared with.
    #[must_use]
    pub fn new(dir: &Path, memory: Option<&Path>) -> Self {
        let real = |path: &Path| resolve(&path.to_string_lossy(), &Self::default());
        Self { dir: real(dir), memory: memory.map(real), ..Self::default() }
    }

    /// Remembers `path` as it is on disk now.
    fn saw(&self, path: &Path) {
        if let (Some(stamp), Ok(mut seen)) = (stamp(path), self.seen.lock()) {
            seen.insert(path.to_owned(), stamp);
        }
    }

    /// Whether `path` is not as it was when the agent last read or wrote it.
    fn changed(&self, path: &Path) -> bool {
        let known = self.seen.lock().ok().and_then(|seen| seen.get(path).copied());
        known.is_some_and(|known| stamp(path) != Some(known))
    }

    /// The same workspace with the installed `skills`, whose folders are in `library`.
    #[must_use]
    pub fn with_skills(self, library: &Path, skills: Arc<Vec<Skill>>) -> Self {
        let library = Some(resolve(&library.to_string_lossy(), &Self::default()));
        Self { library, skills, ..self }
    }

    /// The same workspace running background commands among `jobs`: its
    /// session's, which outlive a turn.
    #[must_use]
    pub fn with_jobs(self, jobs: Arc<Jobs>) -> Self {
        Self { jobs, ..self }
    }
}

/// How much a tool call can change, which decides whether it is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Only looks, inside the workspace.
    Safe,
    /// Changes files inside the workspace.
    Edit,
    /// Runs a command, reaches the network, or touches files elsewhere.
    Ask,
    /// Never allowed: the app's own data, which holds API keys.
    Forbidden,
}

/// The tools' names, descriptions and argument schemas, in a fixed order
/// (a stable tool list keeps the provider's prompt cache valid).
#[must_use]
pub fn definitions() -> Vec<(&'static str, &'static str, Value)> {
    let object = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false });
    let text = |description: &str| json!({ "type": "string", "description": description });
    let number = |description: &str| json!({ "type": "integer", "description": description });
    vec![
        (
            "read",
            "Read a text file. Returns numbered lines. Long files come in pages: pass `offset` (first line, from 1) and `limit` to read on, or to read only the part you need. A picture (PNG, JPEG, GIF, WebP) is shown to you instead.",
            object(
                json!({ "path": text("File path, absolute or relative to your folder."), "offset": number("First line to return, from 1."), "limit": number("Most lines to return.") }),
                &["path"],
            ),
        ),
        (
            "write",
            "Create a file or replace its whole contents. Creates missing folders. Prefer `edit` to change part of a file.",
            object(
                json!({ "path": text("File path."), "content": text("The file's full new contents.") }),
                &["path", "content"],
            ),
        ),
        (
            "edit",
            "Replace text in a file. `old` must match the file exactly and, unless `all` is true, exactly once: include enough surrounding lines to make it unique. To change several places of one file, pass them as `edits`: they are made in order, and none is kept unless all fit. The answer names the lines that changed; a call that did not fail worked, so do not read the file again to check.",
            object(
                json!({
                    "path": text("File path."),
                    "old": text("Exact text to replace."),
                    "new": text("Text to put in its place."),
                    "all": { "type": "boolean", "description": "Replace every occurrence." },
                    "edits": {
                        "type": "array",
                        "description": "Several replacements, instead of `old` and `new`.",
                        "items": object(
                            json!({ "old": text("Exact text to replace."), "new": text("Text to put in its place."), "all": { "type": "boolean", "description": "Replace every occurrence." } }),
                            &["old", "new"],
                        ),
                    },
                }),
                &["path"],
            ),
        ),
        (
            "list",
            "List a folder. With `recursive`, lists the files below it, leaving out what `.gitignore` excludes; `match` keeps names that fit a pattern with `*` wildcards, such as `*.rs`.",
            object(
                json!({ "path": text("Folder path; your folder when omitted."), "recursive": { "type": "boolean", "description": "Descend into subfolders." }, "match": text("Name pattern with `*` wildcards.") }),
                &[],
            ),
        ),
        (
            "search",
            "Find the lines a regular expression matches in the files below a folder, leaving out what `.gitignore` excludes. Letters match whatever their case unless the pattern has a capital. Returns each file's name and under it `line: text`. With `files`, returns only the files' names and how many lines match in each: start with that when you do not know where to look.",
            object(
                json!({
                    "pattern": text("Regular expression, matched within a line: `fn \\w+\\(`, `TODO|FIXME`, `^use `."),
                    "path": text("Folder or file to search; your folder when omitted."),
                    "match": text("Only search files whose name fits this `*` pattern, such as `*.rs`."),
                    "files": { "type": "boolean", "description": "Only name the files that have a match." },
                    "context": number("Lines to show before and after each match; none by default."),
                }),
                &["pattern"],
            ),
        ),
        (
            "shell",
            shell_description(),
            object(
                json!({
                    "command": text("The command line."),
                    "timeout": number("Seconds before it is stopped; 120 by default, 900 at most."),
                    "background": { "type": "boolean", "description": "Keep it running after the call returns, as a job." },
                }),
                &["command"],
            ),
        ),
        (
            "job",
            "Look at the commands you started in the background. Without `id`, lists them. With `id`, says how that one is doing and shows what it printed since you last looked; `wait` first waits up to that many seconds for it to end, and `stop` ends it, with whatever it started.",
            object(
                json!({
                    "id": number("The job's number, as `shell` named it."),
                    "wait": number("Seconds to wait for it to end first; 600 at most."),
                    "stop": { "type": "boolean", "description": "End it." },
                }),
                &[],
            ),
        ),
        (
            "fetch",
            "Fetch a web page or API response over HTTP(S) and return its text (HTML is reduced to its text).",
            object(json!({ "url": text("The http:// or https:// address.") }), &["url"]),
        ),
    ]
}

/// How risky `name` with `input` is in `workspace`. Unknown tools and
/// unreadable arguments are [`Risk::Ask`]: the call fails anyway, visibly.
#[must_use]
pub fn risk(name: &str, input: &Value, workspace: &Workspace) -> Risk {
    let path = input.get("path").and_then(Value::as_str);
    let path = path.map_or_else(|| workspace.dir.clone(), |path| resolve(path, workspace));
    let notes = workspace.memory.as_deref().is_some_and(|memory| path.starts_with(memory));
    let library = workspace.library.as_deref().is_some_and(|library| path.starts_with(library));
    // The sessions' worktrees are kept in the data folder, and are checkouts like any other.
    let private =
        !notes && Config::dir().is_ok_and(|dir| path.starts_with(&dir) && !path.starts_with(dir.join("worktrees")));
    let inside = notes || path.starts_with(&workspace.dir);
    // What its own commands printed, kept whole for it to look through.
    let kept = kept_outputs().is_some_and(|outputs| path.starts_with(outputs));
    match name {
        // Only ever the session's own background commands.
        skills::TOOL | "job" => Risk::Safe,
        // A skill's files are read, never changed, by an agent.
        "read" | "list" | "search" if library || kept => Risk::Safe,
        "read" | "list" | "search" | "write" | "edit" if private => Risk::Forbidden,
        "read" | "list" | "search" if inside => Risk::Safe,
        "write" | "edit" if notes => Risk::Safe,
        "write" | "edit" if inside => Risk::Edit,
        _ => Risk::Ask,
    }
}

/// A short line saying what a call does, for the transcript and the
/// approval prompt: the path, command or address it is about.
#[must_use]
pub fn summary(input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    let text = ["command", "path", "file_path", "url", "pattern", "text", "query", "subject", "description", "prompt"]
        .iter()
        .find_map(|key| field(key));
    let line = text.unwrap_or_default().lines().next().unwrap_or_default();
    line.chars().take(160).collect()
}

/// Runs a tool and returns what the model reads, or `Err` with the reason
/// it failed (also for the model: it can try something else). Blocks until
/// done; `cancel` stops a running command.
///
/// # Errors
/// The tool's own failure, as a sentence.
pub fn run(name: &str, input: &Value, workspace: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let need = |key: &str| text(key).ok_or_else(|| format!("missing `{key}`"));
    let path = |key: &str| text(key).map_or_else(|| workspace.dir.clone(), |path| resolve(path, workspace));
    let flag = |key: &str| input.get(key).and_then(Value::as_bool).unwrap_or(false);
    let count = |key: &str| input.get(key).and_then(Value::as_u64).map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    match name {
        "read" => read(&resolve(need("path")?, workspace), count("offset"), count("limit"), workspace),
        "write" => write(&resolve(need("path")?, workspace), need("content")?, workspace),
        "edit" => edit(&resolve(need("path")?, workspace), &changes(input)?, workspace),
        "list" => list(&path("path"), flag("recursive"), text("match")),
        "search" => {
            search(&path("path"), need("pattern")?, text("match"), flag("files"), count("context").unwrap_or(0))
        }
        "shell" if flag("background") => start_job(need("command")?, workspace, cancel),
        "shell" => shell(need("command")?, input.get("timeout").and_then(Value::as_u64), &workspace.dir, cancel),
        "job" => job(
            input.get("id").and_then(Value::as_u64),
            input.get("wait").and_then(Value::as_u64),
            flag("stop"),
            workspace,
            cancel,
        ),
        "fetch" => fetch(need("url")?),
        skills::TOOL => skills::load(&workspace.skills, input),
        other => Err(format!("there is no tool named `{other}`")),
    }
}

/// `path` as an absolute path without `.` and `..`, relative ones starting
/// from the workspace. Symbolic links are followed where the path exists, so
/// a link out of the workspace does not look like it is inside.
fn resolve(path: &str, workspace: &Workspace) -> PathBuf {
    let path = Path::new(path.trim());
    let joined = if path.is_absolute() { path.to_owned() } else { workspace.dir.join(path) };
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    // The deepest existing ancestor decides where the path really is.
    let mut existing = clean.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = fs::canonicalize(existing) {
            let mut real = strip_verbatim(real);
            real.extend(rest.iter().rev());
            return real;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return clean,
        }
    }
}

/// Windows' canonical paths start with `\\?\`; without it they compare with
/// ordinary ones.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(plain) if !plain.starts_with("UNC") => PathBuf::from(plain),
        _ => path,
    }
}

/// A file's text, refusing ones too large or not text.
fn load(path: &Path) -> Result<String, String> {
    let size = fs::metadata(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?.len();
    if size > MAX_FILE {
        return Err(format!(
            "{} is {size} bytes, too large to open here; use `shell` to look at parts of it",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))
}

fn read(path: &Path, offset: Option<usize>, limit: Option<usize>, workspace: &Workspace) -> Result<String, String> {
    if let Some(kind) = attach::media_type(&path.to_string_lossy()) {
        let size = fs::metadata(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?.len();
        if workspace.blind {
            return Err(format!("{} is a picture, and this model cannot look at pictures", path.display()));
        }
        let (bytes, media_type, scaled) = shown(path, kind)?;
        workspace.saw(path);
        if let Ok(mut ready) = workspace.shown.lock() {
            let picture = Picture { media_type: media_type.to_owned(), data: atroxabot_mcp::base64(&bytes) };
            *ready = Some((path.to_owned(), picture));
        }
        let kind = kind.trim_start_matches("image/").to_uppercase();
        let note = if scaled { " It is shown to you scaled down." } else { "" };
        return Ok(format!("{}: a {kind} picture of {} KB.{note}", path.display(), size.div_ceil(1000)));
    }
    let text = load(path)?;
    workspace.saw(path);
    let first = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(READ_LINES).clamp(1, READ_LINES);
    let total = text.lines().count();
    let mut out = String::new();
    let mut last = first.saturating_sub(1);
    for (number, line) in text.lines().enumerate().skip(first - 1).take(limit) {
        if out.len() > OUTPUT_CHARS {
            break;
        }
        let _ = writeln!(out, "{:>5}  {}", number + 1, clipped(line, LINE_CHARS));
        last = number + 1;
    }
    if total == 0 {
        return Ok("(the file is empty)".to_owned());
    }
    if last < total {
        let _ = write!(out, "\n[lines {first}-{last} of {total}; read on with offset {}]", last + 1);
    }
    Ok(out)
}

/// The largest picture file read to be shown, scaled down if need be.
const MAX_PICTURE: u64 = 64 << 20;

/// The picture at `path` (of the kind `media_type`, by its name) as a
/// model is shown it: its bytes and their kind, and whether they are a
/// copy scaled down from one too large ([`crate::picture::shrink`]).
fn shown(path: &Path, media_type: &'static str) -> Result<(Vec<u8>, &'static str, bool), String> {
    let size = fs::metadata(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?.len();
    let too_large = || {
        format!(
            "{} is a picture of {} KB, larger than the {} KB a model is shown, and it could not be scaled down",
            path.display(),
            size / 1000,
            attach::MAX_IMAGE / 1000
        )
    };
    if size > MAX_PICTURE {
        return Err(too_large());
    }
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    match crate::picture::shrink(&bytes) {
        Some((copy, kind)) => Ok((copy, kind, true)),
        None if bytes.len() as u64 <= attach::MAX_IMAGE => Ok((bytes, media_type, false)),
        None => Err(too_large()),
    }
}

/// The picture a `read` with `input` shows the model, when it reads one it
/// may look at; what [`run`] said of it is the result's text.
#[must_use]
pub fn picture(input: &Value, workspace: &Workspace) -> Option<Picture> {
    let path = resolve(input.get("path").and_then(Value::as_str)?, workspace);
    let media_type = attach::media_type(&path.to_string_lossy())?;
    if workspace.blind {
        return None;
    }
    // What the `read` that came before made ready, when it is of this file.
    let ready = workspace.shown.lock().ok().and_then(|mut ready| ready.take());
    if let Some((_, picture)) = ready.filter(|(of, _)| *of == path) {
        return Some(picture);
    }
    let (bytes, media_type, _) = shown(&path, media_type).ok()?;
    Some(Picture { media_type: media_type.to_owned(), data: atroxabot_mcp::base64(&bytes) })
}

/// `line`, or its first `max` characters and a note of how many more there
/// are: one line of a minified file is no page to read.
fn clipped(line: &str, max: usize) -> Cow<'_, str> {
    match line.char_indices().nth(max) {
        None => Cow::Borrowed(line),
        Some((at, _)) => Cow::Owned(format!("{}… [{} more characters]", &line[..at], line[at..].chars().count())),
    }
}

fn write(path: &Path, content: &str, workspace: &Workspace) -> Result<String, String> {
    if path.exists() && workspace.changed(path) {
        return Err(format!(
            "{} was changed on disk since you read it; read it again before you replace it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    workspace.saw(path);
    Ok(format!("Wrote {} ({} lines).", path.display(), content.lines().count()))
}

/// One replacement of an `edit` call.
#[derive(Debug, Clone, Copy)]
struct Change<'a> {
    old: &'a str,
    new: &'a str,
    all: bool,
}

/// The replacements an `edit` call asks for: its own `old` and `new`, then
/// those of its `edits`.
fn changes(input: &Value) -> Result<Vec<Change<'_>>, String> {
    fn one(value: &Value) -> Result<Change<'_>, String> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing `{key}`"));
        Ok(Change {
            old: text("old")?,
            new: text("new")?,
            all: value.get("all").and_then(Value::as_bool).unwrap_or(false),
        })
    }
    let edits = input.get("edits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let own = (input.get("old").is_some() || edits.is_empty()).then_some(input);
    own.into_iter().chain(edits).map(one).collect()
}

fn edit(path: &Path, changes: &[Change<'_>], workspace: &Workspace) -> Result<String, String> {
    let stale = workspace.changed(path);
    let mut text = load(path)?;
    // Where each replacement's new text is in the file as it will be: its first byte, and its length.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let (which, kept) = if changes.len() == 1 {
            (String::new(), "")
        } else {
            (format!(" of edit {}", index + 1), "; nothing was changed")
        };
        if change.old.is_empty() {
            return Err(format!("`old`{which} is empty; use `write` to create a file{kept}"));
        }
        let mut found: Vec<usize> = text.match_indices(change.old).map(|(at, _)| at).collect();
        match found.len() {
            0 => {
                let why = if stale { ", which was changed on disk since you read it" } else { "" };
                return Err(format!(
                    "`old`{which} was not found in the file{why}; read it again and copy the text exactly{kept}"
                ));
            }
            1 => {}
            _ if change.all => {}
            n => {
                return Err(format!(
                    "`old`{which} matches {n} places; add surrounding lines to make it unique, or set `all`{kept}"
                ));
            }
        }
        found.truncate(if change.all { usize::MAX } else { 1 });
        // What comes after a replacement moves by as much as it grew or shrank.
        let grown = change.new.len().saturating_sub(change.old.len());
        let shrunk = change.old.len().saturating_sub(change.new.len());
        for span in &mut spans {
            let before = found.iter().filter(|at| **at + change.old.len() <= span.0).count();
            span.0 = (span.0 + before * grown).saturating_sub(before * shrunk);
        }
        let mut replaced = String::with_capacity(text.len() + found.len() * grown);
        let mut rest = 0;
        for at in found {
            replaced.push_str(&text[rest..at]);
            spans.push((replaced.len(), change.new.len()));
            replaced.push_str(change.new);
            rest = at + change.old.len();
        }
        replaced.push_str(&text[rest..]);
        text = replaced;
    }
    if spans.is_empty() {
        return Err("nothing to replace: give `old` and `new`, or `edits`".to_owned());
    }
    fs::write(path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    workspace.saw(path);
    let count = spans.len();
    let mut out = format!(
        "Edited {} ({count} replacement{}), now at {}.",
        path.display(),
        if count == 1 { "" } else { "s" },
        lines_of(&text, spans)
    );
    if stale {
        out.push_str(" The file was changed on disk since you read it: the rest of it may not be as you saw it.");
    }
    Ok(out)
}

/// The lines the `spans` of `text` (first byte and length) are on, as
/// `line 4` or `lines 4-9, 20`.
fn lines_of(text: &str, mut spans: Vec<(usize, usize)>) -> String {
    spans.sort_unstable();
    let bytes = text.as_bytes();
    let breaks = |from: usize, to: usize| {
        let part = bytes.get(from.min(bytes.len())..to.min(bytes.len())).unwrap_or_default();
        part.iter().map(|byte| usize::from(*byte == b'\n')).sum::<usize>()
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let (mut line, mut at) = (1, 0);
    for &(start, length) in spans.iter().take(MAX_PLACES) {
        line += breaks(at, start);
        at = at.max(start);
        // A new text that ends its last line does not reach into the next.
        let ends_line = length > 0 && bytes.get(start + length - 1) == Some(&b'\n');
        let last = line + breaks(start, start + length).saturating_sub(usize::from(ends_line));
        if ranges.last() != Some(&(line, last)) {
            ranges.push((line, last));
        }
    }
    let many = ranges.len() > 1 || ranges.iter().any(|(first, last)| first != last);
    let mut out = String::from(if many { "lines " } else { "line " });
    for (index, (first, last)) in ranges.iter().enumerate() {
        let _ = write!(out, "{}{first}", if index == 0 { "" } else { ", " });
        if last != first {
            let _ = write!(out, "-{last}");
        }
    }
    if spans.len() > MAX_PLACES {
        out.push_str(" and further on");
    }
    out
}

/// Whether `name` fits `pattern`, where `*` stands for any run of characters
/// (case-insensitive).
fn fits(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.to_lowercase(), name.to_lowercase());
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else { return true };
    let Some(mut rest) = name.strip_prefix(first) else { return false };
    let mut last = first;
    let mut starred = false;
    for part in parts {
        starred = true;
        last = part;
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    // Without a trailing `*`, the name must end where the pattern does.
    if !starred {
        return rest.is_empty();
    }
    last.is_empty() || name.ends_with(last)
}

/// Calls `visit` with every file below `root` (and folders, when `folders`),
/// skipping the folders nobody means; stops when `visit` returns `false`.
fn walk(root: &Path, recursive: bool, folders: bool, visit: &mut dyn FnMut(&Path, bool) -> bool) -> Result<(), String> {
    let mut stack = vec![(root.to_owned(), 0usize)];
    let mut visits = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if depth == 0 => return Err(format!("cannot list {}: {e}", dir.display())),
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            visits += 1;
            if visits > MAX_VISITS {
                return Ok(());
            }
            let path = entry.path();
            // A link is listed, never followed: no loops, no way out.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir {
                let skipped = entry.file_name().to_str().is_some_and(|name| SKIPPED.contains(&name));
                if folders && !visit(&path, true) {
                    return Ok(());
                }
                if recursive && !skipped && depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if !visit(&path, false) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The files below `root` that Git does not ignore, when `root` is in a
/// repository and has any: what `.gitignore` excludes (builds, packages)
/// is rarely what is looked for, and drowns what is.
fn unignored(root: &Path) -> Option<Vec<PathBuf>> {
    let listed = crate::git::git(root, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).ok()?;
    let files: Vec<PathBuf> = listed
        .split(|byte| *byte == 0)
        .filter_map(|name| std::str::from_utf8(name).ok())
        .filter(|name| !name.is_empty())
        .take(MAX_VISITS)
        .map(|name| name.split('/').fold(root.to_owned(), |path, part| path.join(part)))
        .collect();
    (!files.is_empty()).then_some(files)
}

fn list(root: &Path, recursive: bool, pattern: Option<&str>) -> Result<String, String> {
    let mut out = String::new();
    let mut shown = 0usize;
    let mut more = false;
    let wanted = |path: &Path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        pattern.is_none_or(|pattern| fits(pattern, name))
    };
    let mut show = |path: &Path, is_dir: bool| {
        if shown == MAX_ENTRIES {
            more = true;
            return false;
        }
        shown += 1;
        let relative = path.strip_prefix(root).unwrap_or(path);
        let _ = writeln!(out, "{}{}", relative.display(), if is_dir { "/" } else { "" });
        true
    };
    match recursive.then_some(root).and_then(unignored) {
        // Git also names the files it tracks that are gone from the folder.
        Some(files) => {
            for file in files.iter().filter(|file| wanted(file) && file.is_file()) {
                if !show(file, false) {
                    break;
                }
            }
        }
        None => walk(root, recursive, true, &mut |path, is_dir| !wanted(path) || show(path, is_dir))?,
    }
    if shown == 0 {
        return Ok("(nothing found)".to_owned());
    }
    if more {
        let _ = write!(out, "\n[only the first {MAX_ENTRIES} are shown; narrow it with `match` or a deeper `path`]");
    }
    Ok(out)
}

fn search(root: &Path, pattern: &str, glob: Option<&str>, names: bool, context: usize) -> Result<String, String> {
    const MORE: &str = "more matches were left out; narrow the search";
    if pattern.is_empty() {
        return Err("`pattern` is empty".to_owned());
    }
    let mut out = String::new();
    let mut matcher = Pattern::new(pattern).unwrap_or_else(|why| {
        let _ = writeln!(
            out,
            "[`pattern` is not a regular expression this tool reads ({why}): it was looked for as plain text]"
        );
        Pattern::plain(pattern)
    });
    let context = context.min(MAX_CONTEXT);
    let deadline = Instant::now() + SEARCH_TIME;
    let mut found = 0usize;
    let mut stopped = "";
    let mut look = |path: &Path| {
        // A link may lead anywhere: only what is really below the folder is read.
        if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
            return true;
        }
        let Ok(text) = load(path) else { return true };
        let lines: Vec<&str> = text.lines().collect();
        let mut hits = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            if at % 64 == 0 && Instant::now() > deadline {
                stopped = "the search took too long and stopped here; narrow it";
                return false;
            }
            // The start of a line is enough of one that goes on and on.
            let mut end = line.len().min(SEARCH_LINE.0);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            if matcher.is_match(&line[..end]) {
                hits.push(at);
            }
        }
        if hits.is_empty() {
            return true;
        }
        let full = |found: usize, out: &str| found >= MAX_ENTRIES || out.len() > OUTPUT_CHARS;
        if full(found, &out) {
            stopped = MORE;
            return false;
        }
        let name = if path == root {
            path.file_name().map_or(path, Path::new)
        } else {
            path.strip_prefix(root).unwrap_or(path)
        };
        if names {
            found += 1;
            let _ = writeln!(out, "{}: {}", name.display(), hits.len());
            return true;
        }
        let _ = writeln!(out, "{}", name.display());
        // The first line not shown yet.
        let mut next = 0;
        for &hit in &hits {
            if full(found, &out) {
                stopped = MORE;
                return false;
            }
            found += 1;
            let from = hit.saturating_sub(context).max(next);
            let to = (hit + context).min(lines.len().saturating_sub(1));
            if context > 0 && next > 0 && from > next {
                out.push_str("  --\n");
            }
            for (at, line) in lines.iter().enumerate().take(to + 1).skip(from) {
                let mark = if hits.binary_search(&at).is_ok() { ':' } else { '-' };
                // Indentation only says something next to other lines.
                let line = if context == 0 { line.trim() } else { line.trim_end() };
                let shown: String = line.chars().take(SEARCH_LINE.1).collect();
                let _ = writeln!(out, "  {}{mark} {shown}", at + 1);
            }
            next = next.max(to + 1);
        }
        true
    };
    if root.is_file() {
        look(root);
    } else {
        let mut visit = |path: &Path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            glob.is_some_and(|glob| !fits(glob, name)) || look(path)
        };
        match unignored(root) {
            Some(files) => {
                let _ = files.iter().try_for_each(|file| visit(file).then_some(()));
            }
            None => walk(root, true, false, &mut |path, _| visit(path))?,
        }
    }
    if found == 0 && stopped.is_empty() {
        out.push_str("(no matches)");
    } else if !stopped.is_empty() {
        let _ = write!(out, "\n[{stopped}]");
    }
    Ok(out)
}

/// `text` cut in the middle to at most [`OUTPUT_CHARS`], saying so: the
/// start and the end of an output matter most.
fn bounded(text: &str) -> String {
    match halves(text, OUTPUT_CHARS / 2) {
        None => text.to_owned(),
        Some((head, tail, left_out)) => format!("{head}\n\n[{left_out} characters left out here]\n\n{tail}"),
    }
}

/// The first `head` characters of a `text` longer than [`OUTPUT_CHARS`]
/// and as many of its last as still fit, with the count of those between.
fn halves(text: &str, head: usize) -> Option<(&str, &str, usize)> {
    let count = text.chars().count();
    if count <= OUTPUT_CHARS {
        return None;
    }
    let tail = OUTPUT_CHARS - head;
    let (end, _) = text.char_indices().nth(head)?;
    let (start, _) = text.char_indices().rev().nth(tail.checked_sub(1)?)?;
    Some((&text[..end], &text[start..], count - head - tail))
}

/// What a command printed, for the model: when it is long, its start and,
/// mostly, its end, each cut at a line, and the file that has all of it.
fn shell_output(text: &str) -> String {
    let Some((head, tail, _)) = halves(text, SHELL_HEAD) else { return text.to_owned() };
    let head = head.rfind('\n').map_or(head, |at| &head[..at]);
    let tail = tail.find('\n').map_or(tail, |at| &tail[at + 1..]);
    let left_out = text.chars().count() - head.chars().count() - tail.chars().count();
    let whole = keep(text).map_or_else(String::new, |path| {
        format!("; the whole output is in {}, which `search` and `read` can look through", path.display())
    });
    format!("{head}\n\n[{left_out} characters left out here{whole}]\n\n{tail}")
}

/// This process's folder of kept outputs, once it was made.
static KEPT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The folder of kept outputs, when there is one yet.
fn kept_outputs() -> Option<&'static Path> {
    KEPT.get().and_then(Option::as_deref)
}

/// Keeps `output` whole in a file of this process's own folder in the
/// temporary folder, and returns where; the oldest of more than
/// [`KEPT_OUTPUTS`] goes. The folder is made new and closed to other
/// users, so nothing in it is anyone else's, and the ones earlier runs of
/// the app left are removed.
fn keep(output: &str) -> Option<PathBuf> {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = kept_dir()?;
    let number = COUNT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("output-{number}.txt"));
    fs::write(&path, output).ok()?;
    if let Some(old) = number.checked_sub(KEPT_OUTPUTS) {
        let _ = fs::remove_file(dir.join(format!("output-{old}.txt")));
    }
    Some(path)
}

/// This process's folder of kept outputs, made the first time it is
/// needed, new and closed to other users, so nothing in it is anyone
/// else's; the ones earlier runs of the app left are removed then.
fn kept_dir() -> Option<&'static Path> {
    KEPT.get_or_init(|| {
        let temp = std::env::temp_dir();
        sweep_outputs(&temp);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.subsec_nanos());
        let dir = temp.join(format!("{OUTPUTS}{}-{nanos}", std::process::id()));
        // Fails on a folder that is there already: one somebody else made is never used.
        make_private(&dir).ok()?;
        fs::canonicalize(&dir).ok().map(strip_verbatim)
    })
    .as_deref()
}

/// Makes the folder `dir`, which must not be there yet, for this user alone.
#[cfg(unix)]
fn make_private(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Makes the folder `dir`, which must not be there yet: the temporary
/// folder it is in is this user's alone.
#[cfg(not(unix))]
fn make_private(dir: &Path) -> std::io::Result<()> {
    fs::create_dir(dir)
}

/// Removes the folders of kept outputs in `temp` that nothing wrote to for
/// a day: what earlier runs of the app left behind.
fn sweep_outputs(temp: &Path) {
    let Ok(entries) = fs::read_dir(temp) else { return };
    for entry in entries.flatten() {
        let ours = entry.file_name().to_str().is_some_and(|name| name.starts_with(OUTPUTS));
        // A link is not followed: only a real folder is one of these.
        let old = entry.metadata().is_ok_and(|meta| {
            meta.is_dir() && meta.modified().ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age > OUTPUTS_AGE)
        });
        if ours && old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The shell commands run in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellKind {
    /// bash: Git for Windows' on Windows, the system's elsewhere.
    Bash(PathBuf),
    /// PowerShell 7 (`pwsh`) or, failing it, Windows PowerShell.
    PowerShell(PathBuf),
    /// The POSIX shell, where there is no bash.
    Sh,
}

/// The shell of this computer, found once.
fn shell_kind() -> &'static ShellKind {
    static KIND: OnceLock<ShellKind> = OnceLock::new();
    KIND.get_or_init(|| {
        if cfg!(windows) {
            if let Some(bash) = git_bash() {
                return ShellKind::Bash(bash);
            }
            return ShellKind::PowerShell(on_path("pwsh.exe").unwrap_or_else(|| PathBuf::from("powershell.exe")));
        }
        let bash = Path::new("/bin/bash");
        if bash.is_file() {
            return ShellKind::Bash(bash.to_owned());
        }
        on_path("bash").map_or(ShellKind::Sh, ShellKind::Bash)
    })
}

/// Git for Windows' `bin\bash.exe`, found from the `git` on the `PATH` or
/// where it installs. Never `System32\bash.exe`, which starts WSL.
fn git_bash() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    // `…\Git\cmd\git.exe`, `…\Git\bin\git.exe` or `…\Git\mingw64\bin\git.exe`.
    if let Some(git) = on_path("git.exe") {
        roots.extend(git.ancestors().skip(1).take(3).map(Path::to_path_buf));
    }
    for (variable, below) in [("ProgramFiles", ""), ("ProgramW6432", ""), ("LOCALAPPDATA", "Programs")] {
        if let Some(dir) = std::env::var_os(variable) {
            roots.push(PathBuf::from(dir).join(below).join("Git"));
        }
    }
    roots.iter().map(|root| root.join("bin").join("bash.exe")).find(|bash| bash.is_file())
}

/// The file `name` in a folder of the `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|path| path.is_file())
}

/// What the model is told of `shell`: which shell it is, and how to write for it.
fn shell_description() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let (which, syntax) = match shell_kind() {
            ShellKind::Bash(_) if cfg!(windows) => (
                "Git Bash (`bash -c`) on Windows",
                " Write bash, with forward slashes in paths; Windows programs run too.",
            ),
            ShellKind::Bash(_) => ("bash (`bash -c`)", ""),
            ShellKind::PowerShell(path) if path.to_string_lossy().contains("pwsh") => ("PowerShell 7", ""),
            ShellKind::PowerShell(_) => {
                ("Windows PowerShell 5.1", " Write PowerShell: `;` between commands, as it has no `&&`.")
            }
            ShellKind::Sh => ("sh (`sh -c`)", ""),
        };
        format!(
            "Run a command with {which} in your folder and return its output and exit code.{syntax} Each call is a \
             fresh shell. Long output is cut in the middle and kept whole in a file the answer names: search that \
             file rather than run the command again. With `background`, the command keeps running after the call \
             returns (a server, a watcher): the answer names its job, which `job` checks on and stops."
        )
    })
}

/// The process that runs `line` in `dir`, its output piped: without a
/// console window on Windows, and leading a process group of its own on
/// Unix, so that whatever it starts can be ended with it ([`kill_tree`]).
fn command(line: &str, dir: &Path) -> Command {
    let mut process = match shell_kind() {
        ShellKind::Bash(bash) => {
            let mut process = Command::new(bash);
            process.args(["-c", line]);
            process
        }
        ShellKind::PowerShell(exe) => {
            let mut process = Command::new(exe);
            process.args(["-NoProfile", "-NonInteractive", "-Command", line]);
            process
        }
        ShellKind::Sh => {
            let mut process = Command::new("sh");
            process.args(["-c", line]);
            process
        }
    };
    process.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console flashes up over the app.
        process.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
    process
}

/// Ends `child` and whatever it started (a test runner's workers, a dev
/// server's watchers), which ending the shell alone would leave running.
fn kill_tree(child: &mut Child) {
    let quiet = |mut process: Command| {
        process.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            process.creation_flags(0x0800_0000);
        }
        let _ = process.status();
    };
    if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill.args(["/T", "/F", "/PID", &child.id().to_string()]);
        quiet(taskkill);
    } else {
        // The command leads its own process group (see `command`): the whole group goes.
        let mut kill = Command::new("kill");
        kill.args(["-KILL", "--", &format!("-{}", child.id())]);
        quiet(kill);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn shell(line: &str, timeout: Option<u64>, dir: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let timeout = Duration::from_secs(timeout.unwrap_or(SHELL_TIMEOUT.0).clamp(1, SHELL_TIMEOUT.1));
    let mut child = command(line, dir).spawn().map_err(|e| format!("cannot start the shell: {e}"))?;
    // Both pipes are drained on their own threads, so a chatty command never
    // blocks on a full pipe while this thread waits for it.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(pipe) = pipe {
                // Bounded: a command that prints forever cannot fill memory.
                let _ = pipe.take(8 << 20).read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>));
    let started = Instant::now();
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if cancel.load(Ordering::Relaxed) => break Err("stopped by the user"),
            Ok(None) if started.elapsed() > timeout => break Err("timed out"),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break Err("could not be waited for"),
        }
    };
    if ended.is_err() {
        kill_tree(&mut child);
    }
    let mut out = stdout.join().unwrap_or_default();
    let errors = stderr.join().unwrap_or_default();
    if !errors.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&errors);
    }
    let out = shell_output(out.trim_end());
    match ended {
        Ok(status) if status.success() => Ok(if out.is_empty() { "(no output)".to_owned() } else { out }),
        Ok(status) => {
            Err(format!("{out}\n[exit code {}]", status.code().map_or_else(|| "?".to_owned(), |c| c.to_string())))
        }
        Err(why) => Err(format!("{out}\n[the command {why} after {}s]", started.elapsed().as_secs())),
    }
}

/// The commands an agent left running in the background (`shell` with
/// `background`), for one session; `job` looks at them and stops them.
/// They are stopped, with whatever they started, when the last workspace
/// holding them goes (the app keeps one per session while it runs, and
/// drops it with the session or when it quits).
#[derive(Debug, Default)]
pub struct Jobs {
    list: Mutex<Vec<Job>>,
}

/// One command running (or that ran) in the background.
#[derive(Debug)]
struct Job {
    /// What `job` knows it by, from 1.
    number: usize,
    /// The command line.
    command: String,
    child: Child,
    /// The file its output goes to.
    output: PathBuf,
    /// How much of the output `job` showed already, in bytes.
    shown: u64,
    started: Instant,
    /// How it ended, once it did.
    ended: Option<String>,
}

impl Job {
    /// Notes how it ended, if it did since it was last looked at.
    fn check(&mut self) {
        if self.ended.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.ended =
                Some(status.code().map_or_else(|| "ended by a signal".to_owned(), |code| format!("exit code {code}")));
        }
    }

    /// How it is doing, in a few words.
    fn state(&self) -> String {
        match &self.ended {
            Some(how) => format!("ended, {how}"),
            None => format!("running for {}s", self.started.elapsed().as_secs()),
        }
    }

    /// What it printed since it was last looked at, cut like any command's output.
    fn news(&mut self) -> String {
        let Ok(mut file) = fs::File::open(&self.output) else { return String::new() };
        let mut bytes = Vec::new();
        let read = std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(self.shown))
            .and_then(|_| file.take(JOB_OUTPUT).read_to_end(&mut bytes));
        let Ok(read) = read else { return String::new() };
        self.shown += read as u64;
        let text = String::from_utf8_lossy(&bytes);
        let Some((head, tail, left_out)) = halves(text.trim_end(), SHELL_HEAD) else {
            return text.trim_end().to_owned();
        };
        format!(
            "{head}\n\n[{left_out} characters left out here; the whole output is in {}]\n\n{tail}",
            self.output.display()
        )
    }
}

impl Jobs {
    /// Stops every command still running, with whatever each started.
    pub fn stop_all(&self) {
        let mut list = self.list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in list.iter_mut() {
            job.check();
            if job.ended.is_none() {
                kill_tree(&mut job.child);
                job.ended = Some("stopped".to_owned());
            }
        }
    }

    /// How many commands are running now.
    pub fn running(&self) -> usize {
        let mut list = self.list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut running = 0;
        for job in list.iter_mut() {
            job.check();
            running += usize::from(job.ended.is_none());
        }
        running
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Starts `line` in the background, its output going to a file of the
/// kept outputs' folder, and answers once it has had [`JOB_START`] to fail.
fn start_job(line: &str, workspace: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    static FILES: AtomicUsize = AtomicUsize::new(0);
    if workspace.jobs.running() >= MAX_JOBS {
        return Err(format!("{MAX_JOBS} commands already run in the background: stop one with `job` first"));
    }
    let dir = kept_dir().ok_or("there is no folder to keep the command's output in")?;
    let output = dir.join(format!("job-{}.txt", FILES.fetch_add(1, Ordering::Relaxed)));
    let file = fs::File::create(&output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    let mut child = command(line, &workspace.dir).spawn().map_err(|e| format!("cannot start the shell: {e}"))?;
    // Both pipes go to the file, each on a thread of its own that ends when the command does.
    let file = Arc::new(Mutex::new(file));
    let written = Arc::new(AtomicU64::new(0));
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    ];
    for mut pipe in pipes.into_iter().flatten() {
        let (file, written) = (Arc::clone(&file), Arc::clone(&written));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            // What it prints past the limit is read and let go, so it never blocks on a full pipe.
            while let Ok(read) = pipe.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                let room = JOB_OUTPUT.saturating_sub(written.fetch_add(read as u64, Ordering::Relaxed));
                let kept = &chunk[..read.min(usize::try_from(room).unwrap_or(usize::MAX))];
                if !kept.is_empty() {
                    let _ = file.lock().map(|mut file| file.write_all(kept));
                }
            }
        });
    }
    let started = Instant::now();
    while started.elapsed() < JOB_START && !cancel.load(Ordering::Relaxed) {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut list = workspace.jobs.list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let number = list.len() + 1;
    let mut job = Job { number, command: line.to_owned(), child, output, shown: 0, started, ended: None };
    job.check();
    // Read after a moment more, so what it printed before it ended is in the file.
    if job.ended.is_some() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let news = job.news();
    let answer = match &job.ended {
        Some(how) => {
            format!("The command ended at once ({how}):\n\n{}", if news.is_empty() { "(no output)" } else { &news })
        }
        None => format!(
            "Started job {number}, running in the background: `{line}`. Its output goes to {}, which `read` and \
             `search` can look through; `job` with id {number} shows what is new, waits for it or stops it. It is \
             stopped when the session ends.{}",
            job.output.display(),
            if news.is_empty() { String::new() } else { format!("\n\nSo far it printed:\n\n{news}") }
        ),
    };
    let failed = job.ended.as_deref().is_some_and(|how| how != "exit code 0");
    list.push(job);
    if failed { Err(answer) } else { Ok(answer) }
}

/// `job`: the background commands, or one of them (`id`), waited for up to
/// `wait` seconds and stopped when `stop`.
fn job(
    id: Option<u64>,
    wait: Option<u64>,
    stop: bool,
    workspace: &Workspace,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let lock = || workspace.jobs.list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(id) = id else {
        let mut list = lock();
        if list.is_empty() {
            return Ok("No command was started in the background.".to_owned());
        }
        let lines: Vec<String> = list
            .iter_mut()
            .map(|job| {
                job.check();
                format!("job {}: {}, `{}`", job.number, job.state(), job.command)
            })
            .collect();
        return Ok(lines.join("\n"));
    };
    let find = |list: &mut Vec<Job>| list.iter().position(|job| job.number as u64 == id);
    if find(&mut lock()).is_none() {
        return Err(format!("there is no job {id}; `job` without an id lists them"));
    }
    let until = Instant::now() + Duration::from_secs(wait.unwrap_or(0).min(JOB_WAIT));
    while Instant::now() < until && !cancel.load(Ordering::Relaxed) {
        {
            let mut list = lock();
            let at = find(&mut list).unwrap_or_default();
            list[at].check();
            if list[at].ended.is_some() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut list = lock();
    let at = find(&mut list).unwrap_or_default();
    let job = &mut list[at];
    job.check();
    if stop && job.ended.is_none() {
        kill_tree(&mut job.child);
        job.ended = Some("stopped".to_owned());
    }
    let news = job.news();
    let news = if news.is_empty() { "(nothing new)".to_owned() } else { news };
    Ok(format!("Job {}, {}: `{}`\n\n{news}", job.number, job.state(), job.command))
}

fn fetch(url: &str) -> Result<String, String> {
    let url = url.trim();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("only http:// and https:// addresses can be fetched".to_owned());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("atroxabot/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|e| format!("cannot fetch {url}: {e}"))?;
    let status = response.status();
    let html = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.contains("html"));
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_PAGE)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {url}: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = bounded(if html { plain(&text) } else { text.into_owned() }.trim());
    if status.is_success() { Ok(text) } else { Err(format!("{text}\n[HTTP {}]", status.as_u16())) }
}

/// The readable text of an HTML page: tags, scripts and styles removed,
/// blank runs collapsed. Not a parser: good enough to read an article.
fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let lower: String = rest.chars().take(8).collect::<String>().to_lowercase();
        let skip_to =
            ["script", "style"].iter().find(|tag| lower[1..].starts_with(**tag)).map(|tag| format!("</{tag}"));
        let end = match &skip_to {
            Some(close) => find_ignoring_case(rest, close).and_then(|at| rest[at..].find('>').map(|gt| at + gt)),
            None => rest.find('>'),
        };
        match end {
            Some(end) => {
                // A tag that ends a block ends a line.
                if ["<p", "</p", "<br", "<div", "</div", "<li", "<h", "</h", "<tr"]
                    .iter()
                    .any(|tag| lower.starts_with(tag))
                {
                    out.push('\n');
                }
                rest = &rest[end + 1..];
            }
            None => rest = "",
        }
    }
    out.push_str(rest);
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut text = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        blank = if line.is_empty() { blank + 1 } else { 0 };
        if blank < 2 {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

/// Where `needle` (ASCII, lower-case) starts in `text`, whatever its case.
fn find_ignoring_case(text: &str, needle: &str) -> Option<usize> {
    let (text, needle) = (text.as_bytes(), needle.as_bytes());
    text.windows(needle.len()).position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("atroxabot-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let dir = strip_verbatim(fs::canonicalize(dir).unwrap());
        Workspace { memory: Some(dir.join("notes")), dir, ..Workspace::default() }
    }

    fn call(name: &str, input: &Value, workspace: &Workspace) -> Result<String, String> {
        run(name, input, workspace, &AtomicBool::new(false))
    }

    #[test]
    fn files_are_written_read_edited_listed_and_searched() {
        let ws = workspace("files");
        call("write", &json!({ "path": "src/a.rs", "content": "fn main() {\n    hello();\n}\n" }), &ws).unwrap();
        call("write", &json!({ "path": "src/b.txt", "content": "Hello again\n" }), &ws).unwrap();
        let read = call("read", &json!({ "path": "src/a.rs" }), &ws).unwrap();
        assert!(read.contains("    2      hello();"));
        let page = call("read", &json!({ "path": "src/a.rs", "offset": 2, "limit": 1 }), &ws).unwrap();
        assert!(page.contains("[lines 2-2 of 3; read on with offset 3]"));

        assert!(call("edit", &json!({ "path": "src/a.rs", "old": "nope", "new": "x" }), &ws).is_err());
        call("edit", &json!({ "path": "src/a.rs", "old": "hello()", "new": "bye()" }), &ws).unwrap();
        assert!(fs::read_to_string(ws.dir.join("src/a.rs")).unwrap().contains("bye();"));
        call("write", &json!({ "path": "src/c.rs", "content": "x x" }), &ws).unwrap();
        assert!(
            call("edit", &json!({ "path": "src/c.rs", "old": "x", "new": "y" }), &ws).unwrap_err().contains("2 places")
        );
        call("edit", &json!({ "path": "src/c.rs", "old": "x", "new": "y", "all": true }), &ws).unwrap();

        let listed = call("list", &json!({ "recursive": true, "match": "*.rs" }), &ws).unwrap();
        assert!(listed.contains("a.rs") && listed.contains("c.rs") && !listed.contains("b.txt"));
        let found = call("search", &json!({ "pattern": "hello again" }), &ws).unwrap();
        assert!(found.contains("b.txt\n  1: Hello again") && !found.contains("a.rs"), "{found}");
        assert_eq!(call("search", &json!({ "pattern": "zzz" }), &ws).unwrap(), "(no matches)");
        assert!(call("nope", &json!({}), &ws).is_err() && call("read", &json!({}), &ws).is_err());
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn search_reads_patterns_names_files_and_shows_context() {
        let ws = workspace("search");
        let code = "use std::fs;\n\nfn one() {}\n\n\n\nfn two() {\n    one();\n}\n";
        call("write", &json!({ "path": "src/a.rs", "content": code }), &ws).unwrap();
        call("write", &json!({ "path": "src/b.txt", "content": "one\nONE\n" }), &ws).unwrap();
        let search = |input: Value| call("search", &input, &ws).unwrap();

        let found = search(json!({ "pattern": r"fn \w+\(", "match": "*.rs" }));
        assert!(found.contains("a.rs\n  3: fn one() {}\n  7: fn two() {\n") && !found.contains("b.txt"), "{found}");
        let names = search(json!({ "pattern": "one", "files": true }));
        assert!(names.contains("a.rs: 2\n") && names.contains("b.txt: 2\n") && !names.contains("fn"), "{names}");
        // A capital in the pattern makes the case count.
        let exact = search(json!({ "pattern": "ONE" }));
        assert!(exact.contains("b.txt\n  2: ONE\n") && !exact.contains("1: one") && !exact.contains("a.rs"), "{exact}");
        let around = search(json!({ "pattern": "fn (one|two)", "context": 1 }));
        assert!(
            around.contains("  2- \n  3: fn one() {}\n  4- \n  --\n  6- \n  7: fn two() {\n  8-     one();\n"),
            "{around}"
        );
        // Lines that are close share what is between them, shown once.
        let close = search(json!({ "pattern": "^fn two|^}", "context": 1, "path": "src/a.rs" }));
        assert!(
            close.contains("a.rs\n  6- \n  7: fn two() {\n  8-     one();\n  9: }\n") && !close.contains("--"),
            "{close}"
        );
        // What is no regular expression is looked for as it is, and the answer says so.
        let plain = search(json!({ "pattern": "one(" }));
        assert!(
            plain.starts_with("[`pattern` is not a regular expression") && plain.contains("  8: one();"),
            "{plain}"
        );
        assert!(call("search", &json!({ "pattern": "" }), &ws).is_err());
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn what_git_ignores_is_left_out() {
        let ws = workspace("ignored");
        if crate::git::git(&ws.dir, &["init", "-q"]).is_err() {
            return;
        }
        call("write", &json!({ "path": ".gitignore", "content": "build/\n" }), &ws).unwrap();
        call("write", &json!({ "path": "build/out.txt", "content": "needle\n" }), &ws).unwrap();
        call("write", &json!({ "path": "src/kept.txt", "content": "needle\n" }), &ws).unwrap();
        let found = call("search", &json!({ "pattern": "needle" }), &ws).unwrap();
        assert!(found.contains("kept.txt\n  1: needle") && !found.contains("out.txt"), "{found}");
        let listed = call("list", &json!({ "recursive": true }), &ws).unwrap();
        assert!(
            listed.contains("kept.txt") && listed.contains(".gitignore") && !listed.contains("out.txt"),
            "{listed}"
        );
        // Asked for by name, an ignored folder is looked through like any other.
        let asked = call("search", &json!({ "pattern": "needle", "path": "build" }), &ws).unwrap();
        assert!(asked.contains("out.txt\n  1: needle"), "{asked}");
        assert!(call("list", &json!({ "path": "build", "recursive": true }), &ws).unwrap().contains("out.txt"));
        // A file Git still tracks but that is gone is not listed.
        crate::git::git(&ws.dir, &["add", "-A"]).unwrap();
        fs::remove_file(ws.dir.join("src").join("kept.txt")).unwrap();
        assert!(!call("list", &json!({ "recursive": true }), &ws).unwrap().contains("kept.txt"));
        assert_eq!(call("search", &json!({ "pattern": "needle" }), &ws).unwrap(), "(no matches)");
        let _ = crate::skills::remove_tree(&ws.dir);
    }

    #[test]
    fn edits_are_made_together_or_not_at_all() {
        let ws = workspace("edits");
        let file = ws.dir.join("a.txt");
        call("write", &json!({ "path": "a.txt", "content": "one\ntwo\nthree\nfour\nfive\n" }), &ws).unwrap();
        let two =
            json!({ "path": "a.txt", "edits": [{ "old": "two", "new": "2\n2b" }, { "old": "four", "new": "4" }] });
        let done = call("edit", &two, &ws).unwrap();
        assert!(done.ends_with("(2 replacements), now at lines 2-3, 5."), "{done}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "one\n2\n2b\nthree\n4\nfive\n");
        // One that does not fit keeps the others from being made.
        let half = json!({ "path": "a.txt", "edits": [{ "old": "one", "new": "1" }, { "old": "nope", "new": "x" }] });
        let failed = call("edit", &half, &ws).unwrap_err();
        assert!(failed.contains("of edit 2") && failed.contains("nothing was changed"), "{failed}");
        assert!(fs::read_to_string(&file).unwrap().starts_with("one\n"));
        // A later edit moves where an earlier one is; a removal is where it was.
        let moved = json!({ "path": "a.txt", "edits": [{ "old": "five", "new": "5" }, { "old": "one\n", "new": "" }] });
        assert!(call("edit", &moved, &ws).unwrap().ends_with("now at lines 1, 5."));
        assert_eq!(fs::read_to_string(&file).unwrap(), "2\n2b\nthree\n4\n5\n");
        let both =
            json!({ "path": "a.txt", "old": "three", "new": "3", "edits": [{ "old": "2", "new": "z", "all": true }] });
        assert!(call("edit", &both, &ws).unwrap().ends_with("(3 replacements), now at lines 1, 2, 3."));
        assert_eq!(fs::read_to_string(&file).unwrap(), "z\nzb\n3\n4\n5\n");
        assert!(
            call("edit", &json!({ "path": "a.txt", "old": "4", "new": "four" }), &ws).unwrap().ends_with("line 4.")
        );
        assert!(call("edit", &json!({ "path": "a.txt" }), &ws).unwrap_err().contains("missing `old`"));
        assert!(
            call("edit", &json!({ "path": "a.txt", "edits": [{ "old": "z" }] }), &ws).unwrap_err().contains("`new`")
        );

        // Changed by something else since it was read: said, and not written over whole.
        call("read", &json!({ "path": "a.txt" }), &ws).unwrap();
        fs::write(&file, "somebody else's much longer text\n").unwrap();
        let gone = call("edit", &json!({ "path": "a.txt", "old": "zb", "new": "x" }), &ws).unwrap_err();
        assert!(gone.contains("changed on disk since you read it"), "{gone}");
        assert!(
            call("write", &json!({ "path": "a.txt", "content": "mine" }), &ws).unwrap_err().contains("changed on disk")
        );
        let fits = call("edit", &json!({ "path": "a.txt", "old": "longer", "new": "shorter" }), &ws).unwrap();
        assert!(fits.contains("line 1.") && fits.contains("changed on disk"), "{fits}");
        // Its own edit is what it knows now.
        assert!(
            !call("edit", &json!({ "path": "a.txt", "old": "shorter", "new": "s" }), &ws).unwrap().contains("disk")
        );
        call("write", &json!({ "path": "a.txt", "content": "mine" }), &ws).unwrap();
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn long_output_keeps_its_end_and_a_file_of_it_all() {
        let ws = workspace("output");
        let long = "x".repeat(LINE_CHARS + 5);
        assert!(clipped(&long, LINE_CHARS).ends_with("… [5 more characters]") && clipped("short", 5) == "short");
        call("write", &json!({ "path": "min.js", "content": format!("{long}\nend\n") }), &ws).unwrap();
        let read = call("read", &json!({ "path": "min.js" }), &ws).unwrap();
        assert!(read.contains("[5 more characters]\n    2  end") && read.chars().count() < LINE_CHARS + 100);

        let mut output = String::new();
        for n in 0..6_000 {
            let _ = writeln!(output, "line {n}");
        }
        let output = output.trim_end();
        assert_eq!(shell_output("short"), "short");
        let cut = shell_output(output);
        assert!(
            cut.starts_with("line 0\n") && cut.ends_with("\nline 5999") && cut.chars().count() < OUTPUT_CHARS + 300
        );
        let note = cut.find("characters left out here").unwrap();
        assert!(note < OUTPUT_CHARS / 3, "most of what is kept is the end");
        assert!(
            cut[note..]
                .split_once(
                    "]

"
                )
                .is_some_and(|(_, tail)| tail.starts_with("line ")),
            "cut at a line"
        );
        // All of it is in a file the agent may read and search, and nothing else may be done there.
        let path = cut.split("the whole output is in ").nth(1).and_then(|rest| rest.split(", which").next()).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), output);
        assert_eq!(risk("read", &json!({ "path": path }), &ws), Risk::Safe);
        assert_eq!(risk("search", &json!({ "path": path, "pattern": "x" }), &ws), Risk::Safe);
        assert_eq!(risk("write", &json!({ "path": path }), &ws), Risk::Ask);
        let found = call("search", &json!({ "pattern": "^line 4321$", "path": path }), &ws).unwrap();
        assert!(found.contains("  4322: line 4321"), "{found}");
        // Only so many are kept.
        let first = PathBuf::from(path);
        for _ in 0..=KEPT_OUTPUTS {
            keep("more").unwrap();
        }
        assert!(!first.exists());
        fs::remove_dir_all(&ws.dir).unwrap();
        // A folder an earlier run left a day ago is removed; a fresh one is somebody's still.
        let temp = ws.dir.with_file_name(format!("atroxabot-sweep-{}", std::process::id()));
        let (old, fresh) = (temp.join(format!("{OUTPUTS}1-1")), temp.join(format!("{OUTPUTS}2-2")));
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(old.join("output-0.txt"), "x").unwrap();
        let long_ago = SystemTime::now() - OUTPUTS_AGE - Duration::from_secs(60);
        let set = fs::File::open(&old).and_then(|dir| dir.set_modified(long_ago)).is_ok();
        sweep_outputs(&temp);
        // Where a folder's time cannot be set, there is nothing to tell.
        assert!(!set || !old.exists(), "the old one went");
        assert!(fresh.exists());
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn risk_follows_where_a_call_reaches() {
        let ws = workspace("risk");
        assert_eq!(risk("read", &json!({ "path": "src/a.rs" }), &ws), Risk::Safe);
        assert_eq!(risk("list", &json!({}), &ws), Risk::Safe);
        assert_eq!(risk("write", &json!({ "path": "src/new.rs" }), &ws), Risk::Edit);
        assert_eq!(risk("write", &json!({ "path": "notes/notes.md" }), &ws), Risk::Safe, "its own notes");
        assert_eq!(risk("read", &json!({ "path": "../elsewhere.txt" }), &ws), Risk::Ask, "`..` does not hide it");
        assert_eq!(risk("edit", &json!({ "path": "src/../../x" }), &ws), Risk::Ask);
        assert_eq!(risk("shell", &json!({ "command": "ls" }), &ws), Risk::Ask);
        assert_eq!(risk("fetch", &json!({ "url": "https://example.com" }), &ws), Risk::Ask);
        if let Ok(private) = Config::dir() {
            let config = private.join("config.json");
            assert_eq!(risk("read", &json!({ "path": config.to_str().unwrap() }), &ws), Risk::Forbidden);
            // The skill library is read, never written; the rest stays off limits.
            let ws = ws.clone().with_skills(&private.join("library").join("skills"), Arc::default());
            let script = private.join("library").join("skills").join("pdf").join("fill.py");
            assert_eq!(risk("read", &json!({ "path": script.to_str().unwrap() }), &ws), Risk::Safe);
            assert_eq!(risk("write", &json!({ "path": script.to_str().unwrap() }), &ws), Risk::Forbidden);
            assert_eq!(risk("read", &json!({ "path": config.to_str().unwrap() }), &ws), Risk::Forbidden);
            let record = private.join("library").join("library.json");
            assert_eq!(risk("read", &json!({ "path": record.to_str().unwrap() }), &ws), Risk::Forbidden);
            assert_eq!(risk(skills::TOOL, &json!({ "name": "pdf" }), &ws), Risk::Safe);
            // A session's worktree is kept there too, and is a folder to work in like any other.
            let tree = private.join("worktrees").join("parser-1a2b");
            let inside = Workspace { dir: tree.join("src"), ..ws.clone() };
            let file = |name: &str| json!({ "path": tree.join(name).to_str().unwrap() });
            assert_eq!(risk("write", &file("src/a.rs"), &inside), Risk::Edit);
            assert_eq!(risk("read", &file("Cargo.toml"), &inside), Risk::Ask, "the rest of it: outside its folder");
            assert_eq!(risk("read", &json!({ "path": config.to_str().unwrap() }), &inside), Risk::Forbidden);
            let sneaky = tree.join("..").join("..").join("config.json");
            assert_eq!(risk("read", &json!({ "path": sneaky.to_str().unwrap() }), &inside), Risk::Forbidden);
        }
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn shell_runs_fails_and_times_out() {
        let ws = workspace("shell");
        assert!(call("shell", &json!({ "command": "echo hi" }), &ws).unwrap().contains("hi"));
        assert!(call("shell", &json!({ "command": "exit 3" }), &ws).unwrap_err().contains("[exit code 3]"));
        let sleep =
            if matches!(shell_kind(), ShellKind::PowerShell(_)) { "Start-Sleep -Seconds 20" } else { "sleep 20" };
        let started = Instant::now();
        let slow = call("shell", &json!({ "command": sleep, "timeout": 1 }), &ws).unwrap_err();
        assert!(slow.contains("timed out") && started.elapsed() < Duration::from_secs(15));
        let stopped = run("shell", &json!({ "command": sleep }), &ws, &AtomicBool::new(true)).unwrap_err();
        assert!(stopped.contains("stopped by the user"));
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn the_shell_is_bash_where_there_is_one() {
        let kind = shell_kind();
        if cfg!(windows) {
            if let ShellKind::Bash(bash) = kind {
                assert!(bash.ends_with("bin\\bash.exe") && !bash.to_string_lossy().to_lowercase().contains("system32"));
            }
        } else {
            assert!(matches!(kind, ShellKind::Bash(_) | ShellKind::Sh));
        }
        assert!(shell_description().contains("`background`"));
        let ws = workspace("bash");
        let both = if matches!(kind, ShellKind::PowerShell(_)) { "echo one; echo two" } else { "echo one && echo two" };
        assert_eq!(call("shell", &json!({ "command": both }), &ws).unwrap().lines().count(), 2);
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn background_commands_run_report_and_stop() {
        let ws = workspace("jobs");
        let powershell = matches!(shell_kind(), ShellKind::PowerShell(_));
        let (talk, quick) = if powershell {
            ("Write-Output started; Start-Sleep -Seconds 60", "Write-Output done")
        } else {
            ("echo started; sleep 60", "echo done")
        };
        let started = call("shell", &json!({ "command": talk, "background": true }), &ws).unwrap();
        assert!(started.contains("Started job 1") && started.contains("started"), "{started}");
        let listed = call("job", &json!({}), &ws).unwrap();
        assert!(listed.contains("job 1: running"), "{listed}");
        assert!(call("job", &json!({ "id": 1 }), &ws).unwrap().contains("(nothing new)"), "shown once");
        let at = Instant::now();
        let stopped = call("job", &json!({ "id": 1, "stop": true }), &ws).unwrap();
        assert!(stopped.contains("ended, stopped") && at.elapsed() < Duration::from_secs(10), "{stopped}");
        assert_eq!(ws.jobs.running(), 0);
        let ended = call("shell", &json!({ "command": quick, "background": true }), &ws).unwrap();
        assert!(ended.contains("ended at once") && ended.contains("done"), "{ended}");
        assert!(call("job", &json!({ "id": 9 }), &ws).unwrap_err().contains("no job 9"));
        assert_eq!(risk("job", &json!({ "id": 1 }), &ws), Risk::Safe);
        // The last workspace to go stops what still runs.
        call("shell", &json!({ "command": talk, "background": true }), &ws).unwrap();
        let jobs = Arc::clone(&ws.jobs);
        assert_eq!(jobs.running(), 1);
        jobs.stop_all();
        assert_eq!(jobs.running(), 0);
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn a_picture_is_shown_not_read() {
        let mut ws = workspace("picture");
        let png = ws.dir.join("shot.png");
        fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let input = json!({ "path": "shot.png" });
        let said = call("read", &input, &ws).unwrap();
        assert!(said.contains("PNG picture of 1 KB"), "{said}");
        let shown = picture(&input, &ws).unwrap();
        assert_eq!((shown.media_type.as_str(), shown.data.as_str()), ("image/png", "iVBORw=="));
        assert!(picture(&json!({ "path": "src" }), &ws).is_none());
        assert_eq!(picture(&input, &ws).map(|shown| shown.data), Some("iVBORw==".into()), "asked alone, it is read");
        // One with more pixels than a model is shown is scaled down, once, and the agent is told.
        let side = crate::picture::MAX_SIDE + 40;
        let wide = crate::picture::encode_jpeg(&vec![90; (side * 8 * 4) as usize], side, 8, 80);
        fs::write(ws.dir.join("wide.jpg"), wide).unwrap();
        let input = json!({ "path": "wide.jpg" });
        let said = call("read", &input, &ws).unwrap();
        assert!(said.contains("JPEG picture") && said.ends_with("scaled down."), "{said}");
        let shown = picture(&input, &ws).unwrap();
        assert_eq!(shown.media_type, "image/jpeg");
        assert!(ws.shown.lock().unwrap().is_none(), "taken");
        ws.blind = true;
        assert!(call("read", &input, &ws).unwrap_err().contains("cannot look at pictures"));
        assert!(picture(&input, &ws).is_none());
        fs::remove_dir_all(&ws.dir).unwrap();
    }

    #[test]
    fn patterns_summaries_bounds_and_pages() {
        assert!(fits("*.rs", "main.RS") && fits("a*c", "abc") && fits("*", "x") && fits("ab", "ab"));
        assert!(!fits("*.rs", "main.rsx") && !fits("a*c", "abd") && !fits("ab", "abc"));
        assert_eq!(summary(&json!({ "command": "ls -la\nsecond" })), "ls -la");
        assert_eq!(summary(&json!({ "path": "src/a.rs", "content": "..." })), "src/a.rs");
        assert_eq!(summary(&json!({})), "");
        let long = "é".repeat(OUTPUT_CHARS + 10);
        let cut = bounded(&long);
        assert!(cut.contains("[10 characters left out here]") && cut.chars().count() < long.chars().count() + 60);
        assert_eq!(bounded("short"), "short");
        assert_eq!(lines_of("a\nb\nc\n", vec![(4, 2), (0, 1)]), "lines 1, 3");
        assert_eq!(lines_of("a\nb\nc\n", vec![(2, 4)]), "lines 2-3");
        assert_eq!(lines_of("abc", vec![(9, 9)]), "line 1", "a span that is nowhere is no panic");
        let page = plain("<html><style>p{}</style><p>One &amp; two</p><script>x<y</script><div>Three</div>");
        assert_eq!(page.trim(), "One & two\n\nThree");
        assert!(fetch("file:///etc/passwd").is_err());
    }
}
