//! Skills: a folder with a `SKILL.md` (instructions, and a line on when to
//! use them) and whatever files those instructions need. An agent is shown
//! each skill's name and description, and reads the rest only when a task
//! calls for it.
//!
//! The [`Library`], `~/.atroxabot/library/`, is shared by every agent. It is
//! laid out as a Claude Code plugin, so an agent on a Claude account gets it
//! with `--plugin-dir` for the session, whichever account it runs on and
//! without the app touching that account's folder. An agent on an API gets
//! the [`TOOL`] tool, which reads one. Skills come from repositories laid
//! out as Claude Code's plugin marketplaces are, or from any folder of
//! skills ([`Source`], [`Catalog`]).

mod archive;
mod fetch;
mod scan;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use fetch::Source;
pub use scan::{Catalog, Elsewhere, Group};

use crate::config::{Config, write_private};

/// The name Claude Code knows the library by: its skills are
/// `library:<name>` there, and `<name>` as well.
const PLUGIN: &str = "library";
/// The library's record of where its skills came from, in its folder.
const RECORD: &str = "library.json";
/// The repository offered before the user adds any: Anthropic's own skills.
const ANTHROPIC_SKILLS: &str = "anthropics/skills";
/// Largest `SKILL.md` read.
const MAX_SKILL_FILE: u64 = 1 << 20;
/// Most files, and bytes, a skill may have to be installed.
const MAX_FILES: usize = 5_000;
const MAX_BYTES: u64 = 200 << 20;
/// Longest description kept, in characters (the Agent Skills limit).
const MAX_DESCRIPTION: usize = 1_024;

/// The tool an agent on an API loads a skill with.
pub const TOOL: &str = "skill";

/// A skill: in the library, or offered by a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skill {
    /// Its name, which is also its folder's name in the library.
    pub name: String,
    /// What it does and when to use it, on one line.
    pub description: String,
    /// Its folder.
    pub dir: PathBuf,
    /// Where it is in the repository it is offered by, with `/`.
    pub path: String,
    /// A fingerprint of its files: for one in the library, as installed.
    pub digest: String,
}

/// Where an installed skill came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The repository (or folder) it was installed from.
    pub source: Source,
    /// Where it is in there.
    pub path: String,
    /// The fingerprint of its files when it was installed.
    pub digest: String,
}

/// What the library keeps besides the skills: the repositories the user
/// looks in, and where each skill came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// The repositories, in the order they were added.
    pub repositories: Vec<Source>,
    /// Where each installed skill came from, by name.
    pub installed: BTreeMap<String, Origin>,
}

impl Default for Record {
    fn default() -> Self {
        let anthropic =
            Source::GitHub { repo: ANTHROPIC_SKILLS.to_owned(), reference: String::new(), folder: String::new() };
        Self { repositories: vec![anthropic], installed: BTreeMap::new() }
    }
}

impl Record {
    /// The newer version `catalog` (fetched from `source`) offers of the
    /// installed skill `name`, if it has one.
    #[must_use]
    pub fn update<'a>(&self, name: &str, source: &Source, catalog: &'a Catalog) -> Option<&'a Skill> {
        let origin = self.installed.get(name).filter(|origin| origin.source == *source)?;
        catalog.find(&origin.path).filter(|offered| offered.digest != origin.digest)
    }
}

/// The skills every agent has, in `~/.atroxabot/library/`.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// The library in `~/.atroxabot/library/`.
    ///
    /// # Errors
    /// The platform reports no home folder.
    pub fn open() -> crate::Result<Self> {
        Ok(Self::at(Config::dir()?.join("library")))
    }

    /// The library in `root`.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Its folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder the installed skills are in, one folder each.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Where repositories are downloaded to, beside the library.
    fn cache(&self) -> PathBuf {
        self.root.with_file_name("cache").join("repositories")
    }

    /// The installed skills, by name.
    #[must_use]
    pub fn skills(&self) -> Vec<Skill> {
        let record = self.record();
        let dir = self.skills_dir();
        let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
        let mut skills: Vec<Skill> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| scan::is_skill(path))
            .filter_map(|path| read_skill(&path, &dir, false))
            .map(|mut skill| {
                // In the library, a skill is called what its folder is.
                skill.name = skill.dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                skill.digest =
                    record.installed.get(&skill.name).map(|origin| origin.digest.clone()).unwrap_or_default();
                skill
            })
            .collect();
        skills.sort_by_key(|skill| skill.name.to_lowercase());
        skills
    }

    /// The record of where skills came from (the default one when there is none yet).
    #[must_use]
    pub fn record(&self) -> Record {
        fs::read(self.root.join(RECORD)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    fn save(&self, record: &Record) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
        write_private(&self.root.join(RECORD), &bytes).map_err(|e| format!("Could not save the library: {e}"))
    }

    /// The folder to hand Claude Code as a plugin (`--plugin-dir`), when
    /// there is at least one skill in it.
    #[must_use]
    pub fn plugin_dir(&self) -> Option<PathBuf> {
        let dir = self.skills_dir();
        let any = fs::read_dir(&dir).ok()?.flatten().any(|entry| scan::is_skill(&entry.path()));
        (any && self.write_manifest().is_ok()).then(|| self.root.clone())
    }

    /// Writes the plugin manifest that makes the folder a Claude Code plugin, unless it is there already.
    fn write_manifest(&self) -> Result<(), String> {
        let path = self.root.join(".claude-plugin").join("plugin.json");
        let manifest =
            json!({ "name": PLUGIN, "description": "The skills installed in AtroxaBot, which every agent has." });
        let current = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if current.is_some_and(|current| current == manifest) {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        write_private(&path, &bytes).map_err(|e| format!("Could not write the library's manifest: {e}"))
    }

    /// What `source` offers, fetched now.
    ///
    /// # Errors
    /// It could not be reached or read, as a sentence for the user.
    pub fn fetch(&self, source: &Source) -> Result<Catalog, String> {
        let root = fetch::fetch(source, &self.cache())?;
        Ok(scan::catalog(&root, &source.name()))
    }

    /// Copies `skill` (offered by `source`) into the library, in place of
    /// one of the same name.
    ///
    /// # Errors
    /// Its name cannot be a folder's, it is too large, or it could not be copied.
    pub fn install(&self, skill: &Skill, source: &Source) -> Result<(), String> {
        static STAGED: AtomicU64 = AtomicU64::new(0);
        if !valid_name(&skill.name) {
            return Err(format!("{} cannot be a skill's name.", skill.name));
        }
        let staging = self.root.join(".staging");
        let tmp = staging.join(format!("{}-{}", std::process::id(), STAGED.fetch_add(1, Ordering::Relaxed)));
        let copied = copy_tree(&skill.dir, &tmp, &mut Budget::default()).and_then(|()| {
            let target = self.skills_dir().join(&skill.name);
            fs::create_dir_all(self.skills_dir()).map_err(|e| e.to_string())?;
            if target.exists() {
                remove_tree(&target)?;
            }
            fs::rename(&tmp, &target).map_err(|e| e.to_string())
        });
        let _ = remove_tree(&tmp);
        copied.map_err(|e| format!("Could not install {}: {e}", skill.name))?;
        self.write_manifest()?;
        let mut record = self.record();
        let origin = Origin { source: source.clone(), path: skill.path.clone(), digest: skill.digest.clone() };
        record.installed.insert(skill.name.clone(), origin);
        self.save(&record)
    }

    /// Takes the skill `name` out of the library.
    ///
    /// # Errors
    /// It could not be deleted.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("There is no skill {name}."));
        }
        let dir = self.skills_dir().join(name);
        if dir.exists() {
            remove_tree(&dir).map_err(|e| format!("Could not remove {name}: {e}"))?;
        }
        let mut record = self.record();
        if record.installed.remove(name).is_some() { self.save(&record) } else { Ok(()) }
    }

    /// Adds `source` to the repositories looked in (last), unless it is there.
    ///
    /// # Errors
    /// The record could not be saved.
    pub fn add_repository(&self, source: Source) -> Result<(), String> {
        let mut record = self.record();
        if record.repositories.contains(&source) {
            return Ok(());
        }
        record.repositories.push(source);
        self.save(&record)
    }

    /// Stops looking in `source`; its skills stay installed.
    ///
    /// # Errors
    /// The record could not be saved.
    pub fn remove_repository(&self, source: &Source) -> Result<(), String> {
        let mut record = self.record();
        record.repositories.retain(|kept| kept != source);
        self.save(&record)
    }
}

/// Something to do with the library, off the interface's thread.
#[derive(Debug, Clone)]
pub enum Job {
    /// Read what is installed.
    Load,
    /// Fetch what a repository offers.
    Fetch(Source),
    /// Install a skill a repository offers.
    Install(Box<Skill>, Source),
    /// Remove an installed skill.
    Remove(String),
    /// Add a repository to look in.
    AddRepository(Source),
    /// Stop looking in a repository.
    RemoveRepository(Source),
}

/// What a [`Job`] came to.
#[derive(Debug, Clone)]
pub enum Report {
    /// The library as it is now, and what went wrong changing it.
    Library {
        /// The installed skills.
        skills: Vec<Skill>,
        /// Where they came from, and the repositories.
        record: Record,
        /// What went wrong, if anything.
        problem: Option<String>,
    },
    /// What a repository offers, or why it could not be read.
    Fetched {
        /// The repository.
        source: Source,
        /// What it offers.
        catalog: Result<Box<Catalog>, String>,
    },
}

/// Does `job` and says what came of it. Blocks: downloads and copies.
#[must_use]
pub fn run(job: Job) -> Report {
    // Changes to the record are made one at a time.
    static CHANGING: Mutex<()> = Mutex::new(());
    let library = match Library::open() {
        Ok(library) => library,
        Err(e) => {
            return Report::Library { skills: Vec::new(), record: Record::default(), problem: Some(e.to_string()) };
        }
    };
    let problem = match job {
        Job::Fetch(source) => return Report::Fetched { catalog: library.fetch(&source).map(Box::new), source },
        Job::Load => None,
        job => {
            let _changing = CHANGING.lock().unwrap_or_else(PoisonError::into_inner);
            match job {
                Job::Install(skill, source) => library.install(&skill, &source).err(),
                Job::Remove(name) => library.remove(&name).err(),
                Job::AddRepository(source) => library.add_repository(source).err(),
                Job::RemoveRepository(source) => library.remove_repository(&source).err(),
                Job::Load | Job::Fetch(_) => None,
            }
        }
    };
    Report::Library { skills: library.skills(), record: library.record(), problem }
}

/// The [`TOOL`] tool's description, listing `skills`, and its arguments;
/// `None` when there are no skills.
#[must_use]
pub fn tool(skills: &[Skill]) -> Option<(String, Value)> {
    /// The listing's size, in characters: past it, skills are listed by name only.
    const BUDGET: usize = 12_000;
    if skills.is_empty() {
        return None;
    }
    let mut description = "Load a skill: instructions for a kind of task, and the files they need. When a task fits \
        one of these skills, load it before starting and follow it. Its files are in the folder it names; read them \
        with `read`.\n\nSkills:"
        .to_owned();
    for skill in skills {
        let entry = if description.len() < BUDGET && !skill.description.is_empty() {
            format!("\n- {}: {}", skill.name, skill.description)
        } else {
            format!("\n- {}", skill.name)
        };
        description.push_str(&entry);
    }
    let parameters = json!({
        "type": "object",
        "properties": { "name": { "type": "string", "description": "The skill's name, as listed." } },
        "required": ["name"],
        "additionalProperties": false,
    });
    Some((description, parameters))
}

/// Runs the [`TOOL`] tool: the instructions of the skill `input` names,
/// with where its files are.
///
/// # Errors
/// There is no such skill, or it cannot be read.
pub fn load(skills: &[Skill], input: &Value) -> Result<String, String> {
    let wanted = input.get("name").and_then(Value::as_str).unwrap_or_default().trim();
    let wanted = wanted.strip_prefix(&format!("{PLUGIN}:")).unwrap_or(wanted);
    let Some(skill) = skills.iter().find(|skill| skill.name.eq_ignore_ascii_case(wanted)) else {
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        return Err(format!("There is no skill `{wanted}`. The skills are: {}.", names.join(", ")));
    };
    let text = read_limited(&skill.dir.join("SKILL.md"), MAX_SKILL_FILE)
        .ok_or_else(|| format!("The skill `{}` cannot be read.", skill.name))?;
    let mut out = format!("Skill `{}`, in `{}`.", skill.name, skill.dir.display());
    let files = files(&skill.dir);
    if files.len() > 1 {
        out.push_str(" Its files:");
        for file in files.iter().filter(|file| *file != "SKILL.md").take(60) {
            let _ = write!(out, "\n- {file}");
        }
    }
    let _ = write!(out, "\n\n{}", body(&text).trim());
    Ok(out)
}

/// The skill in `dir`, offered by the repository (or library) folder
/// `root`, from its `SKILL.md`: `None` when it has no name a folder can
/// take. With `fingerprint`, its files are read for [`Skill::digest`].
pub(crate) fn read_skill(dir: &Path, root: &Path, fingerprint: bool) -> Option<Skill> {
    let text = read_limited(&dir.join("SKILL.md"), MAX_SKILL_FILE)?;
    let (name, description) = frontmatter(&text);
    let folder = dir.file_name().map(|name| name.to_string_lossy().into_owned());
    let name = name.filter(|name| valid_name(name)).or(folder.filter(|name| valid_name(name)))?;
    let path = dir.strip_prefix(root).ok()?;
    let path = path.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
    let digest = if fingerprint { digest(dir) } else { String::new() };
    Some(Skill { name, description: description.unwrap_or_default(), dir: dir.to_owned(), path, digest })
}

/// A name a skill's folder can take on every system: letters, digits, `-`,
/// `_` and `.`, starting with a letter or digit, at most 64 characters.
fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && archive::portable(name)
}

/// The `name` and `description` a `SKILL.md`'s front matter (the YAML
/// between its opening `---` lines) gives. Not a YAML parser: plain,
/// quoted, folded (`>`) and literal (`|`) values of top-level keys, which is
/// how skills write them.
fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let lines: Vec<&str> = lines.take_while(|line| !matches!(line.trim_end(), "---" | "...")).collect();
    let value = |key: &str| {
        let at = lines
            .iter()
            .position(|line| line.strip_prefix(key).is_some_and(|rest| rest.trim_start().starts_with(':')))?;
        let first = lines[at][key.len()..].trim_start()[1..].trim();
        // The lines that carry on the value: indented, or blank.
        let more = lines[at + 1..].iter().take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t']));
        let more: Vec<&str> = more.map(|line| line.trim()).collect();
        let value = if first.is_empty() || first.starts_with(['>', '|']) {
            more.join(" ")
        } else if let Some(quoted) = first.strip_prefix('"') {
            let whole = std::iter::once(quoted).chain(more).collect::<Vec<_>>().join(" ");
            let end = whole.char_indices().find(|&(at, c)| c == '"' && !whole[..at].ends_with('\\')).map(|(at, _)| at);
            whole[..end.unwrap_or(whole.len())].replace("\\\"", "\"").replace("\\n", " ").replace("\\\\", "\\")
        } else if let Some(quoted) = first.strip_prefix('\'') {
            let whole = std::iter::once(quoted).chain(more).collect::<Vec<_>>().join(" ").replace("''", "\u{0}");
            whole.split('\'').next().unwrap_or_default().replace('\u{0}', "'")
        } else {
            std::iter::once(first).chain(more).collect::<Vec<_>>().join(" ")
        };
        Some(one_line(&value)).filter(|value| !value.is_empty())
    };
    (value("name"), value("description"))
}

/// What follows a `SKILL.md`'s front matter.
fn body(text: &str) -> &str {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---") else { return text };
    let mut offset = 0;
    for line in rest.split_inclusive('\n').skip(1) {
        offset += line.len();
        if matches!(line.trim_end(), "---" | "...") {
            let start = rest.split_inclusive('\n').next().map_or(0, str::len);
            return &rest[start + offset..];
        }
    }
    text
}

/// `text` on one line, its runs of space made one, at most
/// [`MAX_DESCRIPTION`] characters.
pub(crate) fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(MAX_DESCRIPTION) {
        Some((at, _)) => format!("{}…", line[..at].trim_end()),
        None => line,
    }
}

/// A file's text, if it is no larger than `limit`.
fn read_limited(path: &Path, limit: u64) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// The 64-bit FNV-1a hash of `bytes`.
pub(crate) fn fnv(bytes: &[u8]) -> u64 {
    fnv_more(0xcbf2_9ce4_8422_2325, bytes)
}

fn fnv_more(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The files below `dir` (not links, not in `.git`), relative, with `/`, by name.
fn files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if out.len() >= MAX_FILES {
                return;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && name != ".git" {
                walk(&entry.path(), &format!("{prefix}{name}/"), out);
            } else if kind.is_file() {
                out.push(format!("{prefix}{name}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out
}

/// A fingerprint of the files in `dir`: their names and contents.
fn digest(dir: &Path) -> String {
    let mut hash = fnv(b"skill");
    let mut read = 0u64;
    for file in files(dir) {
        hash = fnv_more(hash, file.as_bytes());
        hash = fnv_more(hash, &[0]);
        let Ok(bytes) = fs::read(dir.join(&file)) else { continue };
        read += bytes.len() as u64;
        if read > MAX_BYTES {
            break;
        }
        hash = fnv_more(hash, &bytes);
    }
    format!("{hash:016x}")
}

/// What a copy may still take.
struct Budget {
    files: usize,
    bytes: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self { files: MAX_FILES, bytes: MAX_BYTES }
    }
}

/// Copies the folder `from` to `to`, files and folders only (no links, no `.git`).
fn copy_tree(from: &Path, to: &Path, budget: &mut Budget) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let target = to.join(entry.file_name());
        if kind.is_dir() && entry.file_name() != ".git" {
            copy_tree(&entry.path(), &target, budget)?;
        } else if kind.is_file() {
            let size = entry.metadata().map_err(|e| e.to_string())?.len();
            if budget.files == 0 || size > budget.bytes {
                return Err("it is too large".to_owned());
            }
            budget.files -= 1;
            budget.bytes -= size;
            fs::copy(entry.path(), &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Deletes a folder and everything in it, read-only files too (Git makes its objects read-only).
pub(crate) fn remove_tree(dir: &Path) -> Result<(), String> {
    fn writable(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                writable(&entry.path());
            } else if let Ok(meta) = entry.metadata() {
                let mut permissions = meta.permissions();
                if permissions.readonly() {
                    #[expect(clippy::permissions_set_readonly_false, reason = "only to delete the file")]
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(entry.path(), permissions);
                }
            }
        }
    }
    if fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return Ok(());
    }
    writable(dir);
    fs::remove_dir_all(dir).map_err(|e| format!("cannot delete {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atroxabot-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_front_matter_as_skills_write_it() {
        let parse = |text: &str| frontmatter(text);
        assert_eq!(
            parse("---\nname: pdf\ndescription: Read and fill PDFs.\nlicense: MIT\n---\n# PDF"),
            (Some("pdf".into()), Some("Read and fill PDFs.".into()))
        );
        assert_eq!(
            parse(
                "\u{feff}---\nname: \"quoted \\\"one\\\"\"\ndescription: >-\n  Folded over\n  two lines.\nother: x\n---"
            ),
            (Some("quoted \"one\"".into()), Some("Folded over two lines.".into()))
        );
        assert_eq!(parse("---\ndescription: 'It''s plain'\n---").1, Some("It's plain".into()));
        assert_eq!(parse("---\ndescription: |\n  Line one\n\n  line two\n---").1, Some("Line one line two".into()));
        assert_eq!(
            parse("---\ndescription: Starts here\n  and goes on\nname: x\n---").1,
            Some("Starts here and goes on".into())
        );
        assert_eq!(parse("---\nnamespace: no\ndescription_long: no\n---"), (None, None), "only whole keys");
        assert_eq!(parse("# No front matter\nname: x"), (None, None));
        assert_eq!(parse("---\ndescription: \"never closed\n---"), (None, Some("never closed".into())));
        assert_eq!(body("---\nname: x\n---\nThe body.\n"), "The body.\n");
        assert_eq!(body("No front matter."), "No front matter.");
    }

    #[test]
    fn names_must_suit_a_folder() {
        for good in ["pdf", "web-artifacts-builder", "v2.skill", "A_b"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-x", ".hidden", "a/b", "a:b", "con", "x".repeat(65).as_str(), "naïve"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(one_line("  a\n\tb  "), "a b");
        assert!(one_line(&"é".repeat(2_000)).chars().count() <= MAX_DESCRIPTION + 1);
    }

    #[test]
    fn installs_updates_and_removes() {
        let base = temp("library");
        let repo = base.join("repo");
        let skill_dir = repo.join("skills").join("pdf");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\nname: pdf\ndescription: PDFs.\n---\nUse scripts/fill.py.").unwrap();
        fs::write(skill_dir.join("scripts").join("fill.py"), "print(1)").unwrap();
        let library = Library::at(base.join("library"));
        assert_eq!(library.record(), Record::default(), "Anthropic's repository is offered from the start");
        assert_eq!(library.plugin_dir(), None, "an empty library is no plugin");

        let source = Source::Folder { path: repo.display().to_string() };
        let catalog = library.fetch(&source).unwrap();
        let offered = catalog.find("skills/pdf").unwrap().clone();
        library.install(&offered, &source).unwrap();
        let installed = library.skills();
        assert_eq!(installed.len(), 1);
        assert_eq!((installed[0].name.as_str(), installed[0].digest.as_str()), ("pdf", offered.digest.as_str()));
        assert!(library.root().join("skills/pdf/scripts/fill.py").is_file());
        assert_eq!(library.plugin_dir().as_deref(), Some(library.root()));
        let manifest = fs::read_to_string(library.root().join(".claude-plugin/plugin.json")).unwrap();
        assert!(manifest.contains("\"library\""));
        assert!(!library.root().join(".staging").read_dir().unwrap().any(|_| true), "nothing left staged");

        // A change in the repository is an update.
        assert_eq!(library.record().update("pdf", &source, &catalog), None);
        fs::write(skill_dir.join("scripts").join("fill.py"), "print(2)").unwrap();
        let newer = library.fetch(&source).unwrap();
        assert!(library.record().update("pdf", &source, &newer).is_some());

        // The API tool lists it and reads it.
        let (description, _) = tool(&installed).unwrap();
        assert!(description.contains("- pdf: PDFs."));
        let loaded = load(&installed, &json!({ "name": "library:pdf" })).unwrap();
        assert!(
            loaded.contains("Use scripts/fill.py.")
                && loaded.contains("- scripts/fill.py")
                && !loaded.contains("name: pdf")
        );
        assert!(load(&installed, &json!({ "name": "nope" })).unwrap_err().contains("pdf"));
        assert_eq!(tool(&[]), None);

        library.remove("pdf").unwrap();
        assert!(library.skills().is_empty() && library.record().installed.is_empty());
        assert!(library.remove("../repo").is_err() && repo.exists(), "never outside the library");
        fs::remove_dir_all(base).unwrap();
    }

    /// Downloads Anthropic's skills from GitHub, as an archive and with Git.
    #[test]
    #[ignore = "downloads from GitHub"]
    fn reads_anthropics_skills() {
        let base = temp("anthropic");
        let library = Library::at(base.join("library"));
        let source = Record::default().repositories[0].clone();
        let catalog = library.fetch(&source).unwrap();
        assert_eq!(catalog.name, "anthropic-agent-skills");
        assert!(catalog.groups.len() >= 2 && catalog.skills().count() >= 10, "{catalog:#?}");
        assert!(catalog.skills().all(|skill| !skill.description.is_empty() && !skill.digest.is_empty()));
        let cloned = Source::parse("https://github.com/anthropics/skills.git").unwrap();
        let by_git = library.fetch(&cloned).unwrap();
        assert_eq!(by_git.skills().count(), catalog.skills().count());
        // Claude Code's own marketplace: plugins in it, and many elsewhere.
        let official = library.fetch(&Source::parse("anthropics/claude-plugins-official").unwrap()).unwrap();
        assert!(official.groups.len() >= 5 && official.elsewhere.len() >= 100, "{official:#?}");
        let missing = library.fetch(&Source::parse("anthropics/no-such-repository-here").unwrap());
        assert!(missing.is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn repositories_are_added_once_and_removed() {
        let base = temp("repositories");
        let library = Library::at(base.join("library"));
        let mine = Source::parse("me/skills").unwrap();
        library.add_repository(mine.clone()).unwrap();
        library.add_repository(mine.clone()).unwrap();
        assert_eq!(library.record().repositories.len(), 2);
        library.remove_repository(&Record::default().repositories[0]).unwrap();
        assert_eq!(library.record().repositories, [mine]);
        fs::remove_dir_all(base).unwrap();
    }
}
