//! What a repository offers. A Claude Code plugin marketplace
//! (`.claude-plugin/marketplace.json`) is read the way Claude Code reads
//! it: its plugins' skills, grouped by plugin, and the plugins it lists that
//! live in other repositories. Anything else is searched for `SKILL.md`
//! files.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Skill, Source, read_skill};

/// Most plugins read from one marketplace.
const MAX_PLUGINS: usize = 2_000;
/// Most skills one repository offers.
const MAX_SKILLS: usize = 1_000;
/// How deep the search for `SKILL.md` goes, and how many folders it opens.
const MAX_DEPTH: usize = 6;
const MAX_FOLDERS: usize = 20_000;
/// Largest manifest read.
const MAX_JSON: u64 = 8 << 20;
/// Folders that never hold skills.
const SKIPPED: [&str; 6] = [".git", "node_modules", "target", ".venv", "__pycache__", ".next"];

/// What a repository offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Its name: the marketplace's, or the repository's.
    pub name: String,
    /// Its skills, by the plugin they come in (one unnamed group when it has no plugins).
    pub groups: Vec<Group>,
    /// Plugins it lists that live in other repositories.
    pub elsewhere: Vec<Elsewhere>,
}

impl Catalog {
    /// Every skill it offers.
    pub fn skills(&self) -> impl Iterator<Item = &Skill> {
        self.groups.iter().flat_map(|group| &group.skills)
    }

    /// The skill at `path` in the repository.
    #[must_use]
    pub fn find(&self, path: &str) -> Option<&Skill> {
        self.skills().find(|skill| skill.path == path)
    }
}

/// Skills that come together: a plugin's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Group {
    /// The plugin's name; empty for skills that come in none.
    pub name: String,
    /// What the plugin is for.
    pub description: String,
    /// Its skills.
    pub skills: Vec<Skill>,
}

/// A plugin a marketplace lists that lives in another repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elsewhere {
    /// Its name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Where it is.
    pub source: Source,
}

/// What the folder `root` offers; `name` names it when nothing in it does.
pub(super) fn catalog(root: &Path, name: &str) -> Catalog {
    if let Some(market) = json(&root.join(".claude-plugin").join("marketplace.json")) {
        let catalog = marketplace(root, &market, name);
        if !catalog.groups.is_empty() || !catalog.elsewhere.is_empty() {
            return catalog;
        }
    }
    if let Some(manifest) = json(&root.join(".claude-plugin").join("plugin.json")) {
        let skills = plugin_skills(root, root, &paths(manifest.get("skills")), false, &mut HashSet::new());
        if !skills.is_empty() {
            let group = Group { name: text(&manifest, "name"), description: text(&manifest, "description"), skills };
            return Catalog { name: name.to_owned(), groups: vec![group], elsewhere: Vec::new() };
        }
    }
    let skills: Vec<Skill> = search(root).iter().filter_map(|dir| read_skill(dir, root, true)).collect();
    let groups = if skills.is_empty() { Vec::new() } else { vec![Group { skills, ..Group::default() }] };
    Catalog { name: name.to_owned(), groups, elsewhere: Vec::new() }
}

/// A marketplace's plugins: those in the repository by their skills, the others by where they are.
fn marketplace(root: &Path, market: &Value, name: &str) -> Catalog {
    let plugin_root = market.pointer("/metadata/pluginRoot").and_then(Value::as_str);
    let mut catalog = Catalog {
        name: Some(text(market, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| name.to_owned()),
        ..Catalog::default()
    };
    let mut seen = HashSet::new();
    let plugins = market.get("plugins").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for entry in plugins.iter().take(MAX_PLUGINS) {
        let (name, description) = (text(entry, "name"), text(entry, "description"));
        if name.is_empty() {
            continue;
        }
        match entry.get("source") {
            Some(Value::String(source)) => {
                let Some(dir) = plugin_folder(root, source, plugin_root) else { continue };
                let mut declared = paths(entry.get("skills"));
                if let Some(manifest) = json(&dir.join(".claude-plugin").join("plugin.json")) {
                    declared.extend(paths(manifest.get("skills")));
                }
                // A plugin at the marketplace's root that names its skills has only those.
                let only_declared = dir == root && entry.get("skills").is_some();
                let skills = plugin_skills(root, &dir, &declared, only_declared, &mut seen);
                if !skills.is_empty() {
                    catalog.groups.push(Group { name, description, skills });
                }
            }
            Some(source) => {
                if let Some(source) = Source::from_entry(source) {
                    catalog.elsewhere.push(Elsewhere { name, description, source });
                }
            }
            None => {}
        }
    }
    catalog
}

/// The folder a plugin entry's relative `source` names: `./x` from the
/// marketplace's root, a bare name under its `pluginRoot`.
fn plugin_folder(root: &Path, source: &str, plugin_root: Option<&str>) -> Option<PathBuf> {
    let source = source.trim();
    if source == "." || source == "./" {
        return Some(root.to_owned());
    }
    match source.strip_prefix("./") {
        Some(relative) => join(root, relative),
        None => join(&join(root, plugin_root?)?, source),
    }
}

/// The skills of the plugin in `dir`: those its manifest and entry name
/// (a skill's folder, or a folder of them), then (unless `only_declared`)
/// those in its `skills/`, else the plugin itself when it is one skill.
/// Skills already in `seen` are left out.
fn plugin_skills(
    root: &Path,
    dir: &Path,
    declared: &[String],
    only_declared: bool,
    seen: &mut HashSet<PathBuf>,
) -> Vec<Skill> {
    let mut found = Vec::new();
    for relative in declared {
        let Some(path) = join(dir, relative) else { continue };
        if is_skill(&path) {
            found.push(path);
        } else {
            found.extend(child_skills(&path));
        }
    }
    if !only_declared {
        found.extend(child_skills(&dir.join("skills")));
    }
    if found.is_empty() && is_skill(dir) {
        found.push(dir.to_owned());
    }
    found
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .take(MAX_SKILLS)
        .filter_map(|path| read_skill(&path, root, true))
        .collect()
}

/// The folders below `root` that hold a skill, shallowest first, not
/// looking inside a skill or a link.
fn search(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.to_owned(), 0)]);
    let mut opened = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        if is_skill(&dir) {
            found.push(dir);
            if found.len() >= MAX_SKILLS {
                break;
            }
            continue;
        }
        opened += 1;
        if depth >= MAX_DEPTH || opened > MAX_FOLDERS {
            continue;
        }
        queue.extend(folders(&dir).into_iter().map(|child| (child, depth + 1)));
    }
    found
}

/// The folders in `dir` (not links), by name, without those that never hold skills.
fn folders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !SKIPPED.iter().any(|skipped| entry.file_name() == *skipped))
        .map(|entry| entry.path())
        .collect();
    folders.sort();
    folders
}

/// The skill folders directly in `dir`.
fn child_skills(dir: &Path) -> Vec<PathBuf> {
    folders(dir).into_iter().filter(|child| is_skill(child)).collect()
}

/// Whether `dir` is a skill: a real folder (not a link) with a `SKILL.md` file in it.
pub(super) fn is_skill(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
        && fs::symlink_metadata(dir.join("SKILL.md")).is_ok_and(|meta| meta.is_file())
}

/// `base` joined with the relative `path` (`/` or `\`), refusing anything
/// that would leave `base` or pass through a link.
fn join(base: &Path, path: &str) -> Option<PathBuf> {
    let mut joined = base.to_owned();
    for part in path.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".") {
        if !super::archive::portable(part) {
            return None;
        }
        joined.push(part);
        if fs::symlink_metadata(&joined).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return None;
        }
    }
    Some(joined)
}

/// A `skills` field: one path or a list of them.
fn paths(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).map(str::to_owned).take(MAX_SKILLS).collect()
        }
        _ => Vec::new(),
    }
}

/// A text field of `value`, on one line.
fn text(value: &Value, key: &str) -> String {
    super::one_line(value.get(key).and_then(Value::as_str).unwrap_or_default())
}

/// A JSON file, when there is one that parses.
fn json(path: &Path) -> Option<Value> {
    if fs::symlink_metadata(path).ok()?.len() > MAX_JSON {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atroxabot-scan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn skill(root: &Path, dir: &str, name: &str) {
        put(root, &format!("{dir}/SKILL.md"), &format!("---\nname: {name}\ndescription: Does {name}.\n---\nBody"));
    }

    fn names(group: &Group) -> Vec<&str> {
        group.skills.iter().map(|skill| skill.name.as_str()).collect()
    }

    #[test]
    fn reads_a_marketplace_as_claude_code_does() {
        let root = temp("market");
        let market = serde_json::json!({
            "name": "team-skills",
            "metadata": { "pluginRoot": "./plugins" },
            "plugins": [
                // Anthropic's layout: plugins at the root that name their skills.
                { "name": "documents", "description": "Office files", "source": "./", "strict": false, "skills": ["./skills/pdf", "./skills/docx"] },
                { "name": "tools", "source": "tools" },
                { "name": "single", "source": "./single" },
                { "name": "mcp-only", "source": "./mcp" },
                { "name": "far", "description": "Elsewhere", "source": { "source": "github", "repo": "a/far" } },
                { "name": "npm", "source": { "source": "npm", "package": "x" } },
                { "name": "escape", "source": "./../outside" },
            ]
        });
        put(&root, ".claude-plugin/marketplace.json", &market.to_string());
        skill(&root, "skills/pdf", "pdf");
        skill(&root, "skills/docx", "docx");
        skill(&root, "skills/unlisted", "unlisted");
        skill(&root, "plugins/tools/skills/lint", "lint");
        skill(&root, "plugins/tools/extra/fmt", "fmt");
        put(&root, "plugins/tools/.claude-plugin/plugin.json", r#"{"name":"tools","skills":"./extra"}"#);
        skill(&root, "single", "single");
        put(&root, "mcp/.mcp.json", "{}");

        let catalog = catalog(&root, "fallback");
        assert_eq!(catalog.name, "team-skills");
        let groups: Vec<(&str, Vec<&str>)> = catalog.groups.iter().map(|g| (g.name.as_str(), names(g))).collect();
        assert_eq!(
            groups,
            [("documents", vec!["pdf", "docx"]), ("tools", vec!["fmt", "lint"]), ("single", vec!["single"])]
        );
        assert_eq!(catalog.find("skills/pdf").map(|skill| skill.description.as_str()), Some("Does pdf."));
        assert_eq!(catalog.elsewhere.len(), 1);
        assert_eq!(catalog.elsewhere[0].name, "far");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn searches_a_plain_repository() {
        let root = temp("plain");
        skill(&root, "a/one", "one");
        skill(&root, "b/c/two", "two");
        skill(&root, "a/one/nested", "nested");
        skill(&root, "node_modules/x", "hidden");
        put(&root, "README.md", "# skills");
        let catalog = catalog(&root, "mine");
        assert_eq!(catalog.name, "mine");
        assert_eq!(names(&catalog.groups[0]), ["one", "two"], "not inside a skill, not in node_modules");
        assert_eq!(catalog.groups[0].skills[1].path, "b/c/two");

        let empty = temp("empty");
        assert!(super::catalog(&empty, "none").groups.is_empty());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn a_repository_that_is_one_skill() {
        let root = temp("one");
        skill(&root, ".", "solo");
        let catalog = catalog(&root, "solo-repo");
        assert_eq!(names(&catalog.groups[0]), ["solo"]);
        assert_eq!(catalog.groups[0].skills[0].path, "");
        fs::remove_dir_all(root).unwrap();
    }
}
