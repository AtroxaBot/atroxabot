//! The tools an agent has when it thinks through an API (the Claude Agent
//! brings its own): reading, writing and editing files, listing and
//! searching folders, running commands and fetching pages; and, when the
//! library has skills, loading one ([`crate::skills::TOOL`]).
//!
//! Each call is a typed action, so the app can judge it before it runs
//! ([`risk`]) and show what it did. Outputs are bounded: a long file is read
//! in pages and a long command output is cut in the middle, with a note
//! saying so, which keeps one careless call from flooding the context.

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Config;
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
/// How long a command may run unless it asks for longer, and the most it may ask for.
const SHELL_TIMEOUT: (u64, u64) = (120, 900);
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
}

impl Workspace {
    /// The workspace in `dir` with notes in `memory`, both as the real paths
    /// (links followed) that tool arguments are compared with.
    #[must_use]
    pub fn new(dir: &Path, memory: Option<&Path>) -> Self {
        let real = |path: &Path| resolve(&path.to_string_lossy(), &Self::default());
        Self { dir: real(dir), memory: memory.map(real), library: None, skills: Arc::default() }
    }

    /// The same workspace with the installed `skills`, whose folders are in `library`.
    #[must_use]
    pub fn with_skills(self, library: &Path, skills: Arc<Vec<Skill>>) -> Self {
        let library = Some(resolve(&library.to_string_lossy(), &Self::default()));
        Self { library, skills, ..self }
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
            "Read a text file. Returns numbered lines. Long files come in pages: pass `offset` (first line, from 1) and `limit` to read on.",
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
            "Replace text in a file. `old` must match the file exactly and, unless `all` is true, exactly once: include enough surrounding lines to make it unique.",
            object(
                json!({ "path": text("File path."), "old": text("Exact text to replace."), "new": text("Text to put in its place."), "all": { "type": "boolean", "description": "Replace every occurrence." } }),
                &["path", "old", "new"],
            ),
        ),
        (
            "list",
            "List a folder. With `recursive`, lists everything below it; `match` keeps names that fit a pattern with `*` wildcards, such as `*.rs`.",
            object(
                json!({ "path": text("Folder path; your folder when omitted."), "recursive": { "type": "boolean", "description": "Descend into subfolders." }, "match": text("Name pattern with `*` wildcards.") }),
                &[],
            ),
        ),
        (
            "search",
            "Find lines containing some text in the files below a folder (case-insensitive, plain text, not a regular expression). Returns `path:line: text`.",
            object(
                json!({ "text": text("Text to find."), "path": text("Folder or file to search; your folder when omitted."), "match": text("Only search files whose name fits this `*` pattern.") }),
                &["text"],
            ),
        ),
        (
            "shell",
            if cfg!(windows) {
                "Run a PowerShell command in your folder and return its output and exit code. Each call is a fresh shell."
            } else {
                "Run a shell command (`sh -c`) in your folder and return its output and exit code. Each call is a fresh shell."
            },
            object(
                json!({ "command": text("The command line."), "timeout": number("Seconds before it is stopped; 120 by default, 900 at most.") }),
                &["command"],
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
    match name {
        skills::TOOL => Risk::Safe,
        // A skill's files are read, never changed, by an agent.
        "read" | "list" | "search" if library => Risk::Safe,
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
        "read" => read(&resolve(need("path")?, workspace), count("offset"), count("limit")),
        "write" => write(&resolve(need("path")?, workspace), need("content")?),
        "edit" => edit(&resolve(need("path")?, workspace), need("old")?, need("new")?, flag("all")),
        "list" => list(&path("path"), flag("recursive"), text("match")),
        "search" => search(&path("path"), need("text")?, text("match")),
        "shell" => shell(need("command")?, input.get("timeout").and_then(Value::as_u64), &workspace.dir, cancel),
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

fn read(path: &Path, offset: Option<usize>, limit: Option<usize>) -> Result<String, String> {
    let text = load(path)?;
    let first = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(READ_LINES).clamp(1, READ_LINES);
    let total = text.lines().count();
    let mut out = String::new();
    let mut last = first.saturating_sub(1);
    for (number, line) in text.lines().enumerate().skip(first - 1).take(limit) {
        if out.len() > OUTPUT_CHARS {
            break;
        }
        let _ = writeln!(out, "{:>5}  {line}", number + 1);
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

fn write(path: &Path, content: &str) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!("Wrote {} ({} lines).", path.display(), content.lines().count()))
}

fn edit(path: &Path, old: &str, new: &str, all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("`old` is empty; use `write` to create a file".to_owned());
    }
    let text = load(path)?;
    let found = text.matches(old).count();
    match found {
        0 => return Err("`old` was not found in the file; read it again and copy the text exactly".to_owned()),
        1 => {}
        _ if all => {}
        n => return Err(format!("`old` matches {n} places; add surrounding lines to make it unique, or set `all`")),
    }
    let replaced = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    fs::write(path, replaced).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!("Edited {} ({found} replacement{}).", path.display(), if found == 1 { "" } else { "s" }))
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

fn list(root: &Path, recursive: bool, pattern: Option<&str>) -> Result<String, String> {
    let mut out = String::new();
    let mut shown = 0usize;
    let mut more = false;
    walk(root, recursive, true, &mut |path, is_dir| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if pattern.is_some_and(|pattern| !fits(pattern, name)) {
            return true;
        }
        if shown == MAX_ENTRIES {
            more = true;
            return false;
        }
        shown += 1;
        let relative = path.strip_prefix(root).unwrap_or(path);
        let _ = writeln!(out, "{}{}", relative.display(), if is_dir { "/" } else { "" });
        true
    })?;
    if shown == 0 {
        return Ok("(nothing found)".to_owned());
    }
    if more {
        let _ = write!(out, "\n[only the first {MAX_ENTRIES} are shown; narrow it with `match` or a deeper `path`]");
    }
    Ok(out)
}

fn search(root: &Path, needle: &str, pattern: Option<&str>) -> Result<String, String> {
    if needle.is_empty() {
        return Err("`text` is empty".to_owned());
    }
    let needle = needle.to_lowercase();
    let mut out = String::new();
    let mut found = 0usize;
    let mut more = false;
    let mut look = |path: &Path| {
        let Ok(text) = load(path) else { return true };
        for (number, line) in text.lines().enumerate() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            if found == MAX_ENTRIES || out.len() > OUTPUT_CHARS {
                more = true;
                return false;
            }
            found += 1;
            let shown: String = line.trim().chars().take(240).collect();
            let relative = path.strip_prefix(root).unwrap_or(path);
            let _ = writeln!(out, "{}:{}: {shown}", relative.display(), number + 1);
        }
        true
    };
    if root.is_file() {
        look(root);
    } else {
        walk(root, true, false, &mut |path, _| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            pattern.is_some_and(|pattern| !fits(pattern, name)) || look(path)
        })?;
    }
    if found == 0 {
        return Ok("(no matches)".to_owned());
    }
    if more {
        out.push_str("\n[more matches were left out; narrow the search]");
    }
    Ok(out)
}

/// `text` cut in the middle to at most [`OUTPUT_CHARS`], saying so: the
/// start and the end of an output matter most.
fn bounded(text: &str) -> String {
    let count = text.chars().count();
    if count <= OUTPUT_CHARS {
        return text.to_owned();
    }
    let half = OUTPUT_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    format!("{head}\n\n[{} characters left out here]\n\n{tail}", count - 2 * half)
}

fn shell(command: &str, timeout: Option<u64>, dir: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let timeout = Duration::from_secs(timeout.unwrap_or(SHELL_TIMEOUT.0).clamp(1, SHELL_TIMEOUT.1));
    let mut process = if cfg!(windows) {
        let mut process = Command::new("powershell.exe");
        process.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        process
    } else {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console flashes up over the app.
        process.creation_flags(0x0800_0000);
    }
    let mut child = process
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start the shell: {e}"))?;
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
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut out = stdout.join().unwrap_or_default();
    let errors = stderr.join().unwrap_or_default();
    if !errors.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&errors);
    }
    let out = bounded(out.trim_end());
    match ended {
        Ok(status) if status.success() => Ok(if out.is_empty() { "(no output)".to_owned() } else { out }),
        Ok(status) => {
            Err(format!("{out}\n[exit code {}]", status.code().map_or_else(|| "?".to_owned(), |c| c.to_string())))
        }
        Err(why) => Err(format!("{out}\n[the command {why} after {}s]", started.elapsed().as_secs())),
    }
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
        let found = call("search", &json!({ "text": "HELLO" }), &ws).unwrap();
        assert!(found.contains("b.txt:1: Hello again") && !found.contains("a.rs"));
        assert_eq!(call("search", &json!({ "text": "zzz" }), &ws).unwrap(), "(no matches)");
        assert!(call("nope", &json!({}), &ws).is_err() && call("read", &json!({}), &ws).is_err());
        fs::remove_dir_all(&ws.dir).unwrap();
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
        let sleep = if cfg!(windows) { "Start-Sleep -Seconds 20" } else { "sleep 20" };
        let started = Instant::now();
        let slow = call("shell", &json!({ "command": sleep, "timeout": 1 }), &ws).unwrap_err();
        assert!(slow.contains("timed out") && started.elapsed() < Duration::from_secs(15));
        let stopped = run("shell", &json!({ "command": sleep }), &ws, &AtomicBool::new(true)).unwrap_err();
        assert!(stopped.contains("stopped by the user"));
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
        let page = plain("<html><style>p{}</style><p>One &amp; two</p><script>x<y</script><div>Three</div>");
        assert_eq!(page.trim(), "One & two\n\nThree");
        assert!(fetch("file:///etc/passwd").is_err());
    }
}
