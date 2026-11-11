//! Where skills come from ([`Source`]) and getting them onto this computer:
//! a GitHub repository as GitHub's own `.tar.gz` (no Git needed), any other
//! repository with Git, a folder as it is.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::archive;

/// A place skills come from: a repository, or one of its folders, or a
/// folder on this computer. Written as Claude Code's `plugin marketplace
/// add` takes them ([`Source::parse`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// A repository on GitHub, `owner/name`.
    GitHub {
        /// `owner/name`.
        repo: String,
        /// A branch, tag or commit; the default branch when empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reference: String,
        /// The folder in it, with `/`; the whole repository when empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        folder: String,
    },
    /// Any Git repository, by its address.
    Git {
        /// Its `https://`, `ssh://` or `user@host:path` address.
        url: String,
        /// A branch, tag or commit; the default branch when empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reference: String,
        /// The folder in it, with `/`; the whole repository when empty.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        folder: String,
    },
    /// A folder on this computer, read where it is.
    Folder {
        /// Its absolute path.
        path: String,
    },
}

impl Source {
    /// Reads what the user typed: `owner/name` (with `#ref` or `@ref`), a
    /// GitHub page (`…/tree/<ref>/<folder>` too), any Git address, or a
    /// folder.
    ///
    /// # Errors
    /// What is wrong with it, as a sentence.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("Type where the skills are.".to_owned());
        }
        if let Some(path) = local_path(text) {
            return Ok(Self::Folder { path });
        }
        if let Some(rest) = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")) {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = host.to_ascii_lowercase();
            if host == "github.com" || host == "www.github.com" {
                return github_page(path).ok_or_else(|| "That is not a GitHub repository's address.".to_owned());
            }
            if path.to_ascii_lowercase().ends_with(".json") {
                return Err("Add the repository the marketplace file is in, not the file.".to_owned());
            }
            return git(text, "", "");
        }
        if text.starts_with("ssh://") || text.starts_with("git@") || is_scp_address(text) {
            return git(text, "", "");
        }
        let (repo, reference) = text.split_once(['#', '@']).unwrap_or((text, ""));
        if is_repo(repo) && (reference.is_empty() || is_reference(reference)) {
            return Ok(Self::GitHub { repo: repo.to_owned(), reference: reference.to_owned(), folder: String::new() });
        }
        Err("Use owner/repository, a Git address, or a folder on this computer.".to_owned())
    }

    /// The source of a plugin entry in a marketplace that lives in another
    /// repository: `github`, `url` and `git-subdir` sources (a pinned
    /// commit wins over a branch). `None` for any other kind (npm, archives,
    /// commands), which carry no skills this app reads.
    pub(super) fn from_entry(entry: &Value) -> Option<Self> {
        let field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default();
        let sha = field("sha");
        let reference = if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) { sha } else { field("ref") };
        let reference = if is_reference(reference) { reference } else { "" };
        let folder = clean_folder(field("path"))?;
        let at = |source: Self| match source {
            Self::GitHub { repo, .. } => Self::GitHub { repo, reference: reference.to_owned(), folder: folder.clone() },
            Self::Git { url, .. } => Self::Git { url, reference: reference.to_owned(), folder: folder.clone() },
            Self::Folder { .. } => source,
        };
        match field("source") {
            "github" => is_repo(field("repo")).then(|| {
                at(Self::GitHub { repo: field("repo").to_owned(), reference: String::new(), folder: String::new() })
            }),
            "url" | "git-subdir" => match Self::parse(field("url")).ok()? {
                Self::Folder { .. } => None,
                source => Some(at(source)),
            },
            _ => None,
        }
    }

    /// A name for it: the repository's, or the folder's.
    #[must_use]
    pub fn name(&self) -> String {
        let last =
            |text: &str| text.trim_end_matches(['/', '\\']).rsplit(['/', '\\', ':']).next().unwrap_or(text).to_owned();
        match self {
            Self::GitHub { folder, .. } | Self::Git { folder, .. } if !folder.is_empty() => last(folder),
            Self::GitHub { repo, .. } => last(repo),
            Self::Git { url, .. } => last(url.trim_end_matches(".git")),
            Self::Folder { path } => last(path),
        }
    }

    /// The download (or clone) this source shares with every other folder of its repository, as a file name.
    fn cache_key(&self) -> String {
        let (kind, place, reference) = match self {
            Self::GitHub { repo, reference, .. } => ("github", repo.as_str(), reference.as_str()),
            Self::Git { url, reference, .. } => ("git", url.as_str(), reference.as_str()),
            Self::Folder { path } => ("folder", path.as_str(), ""),
        };
        format!("{:016x}", super::fnv(format!("{kind}\0{place}\0{reference}").as_bytes()))
    }
}

impl fmt::Display for Source {
    /// How it is shown: `owner/name@ref/folder`, an address, or a path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (place, reference, folder) = match self {
            Self::GitHub { repo, reference, folder } => (repo, reference, folder),
            Self::Git { url, reference, folder } => (url, reference, folder),
            Self::Folder { path } => return f.write_str(path),
        };
        f.write_str(place)?;
        if !reference.is_empty() {
            // A commit is long; its first seven characters are how people write it.
            let short = if reference.len() == 40 { &reference[..7] } else { reference };
            write!(f, "@{short}")?;
        }
        if !folder.is_empty() {
            write!(f, " · {folder}")?;
        }
        Ok(())
    }
}

/// A folder path (absolute, `~`, `./` or a drive), made absolute; `None` for anything else.
fn local_path(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")).or((text == "~").then_some("")) {
        let home = std::env::home_dir()?;
        return Some(home.join(rest).display().to_string());
    }
    if drive || text.starts_with('/') || text.starts_with("\\\\") {
        return Some(text.to_owned());
    }
    if text.starts_with("./") || text.starts_with("../") || text.starts_with(".\\") || text.starts_with("..\\") {
        return std::env::current_dir().ok().map(|dir| dir.join(text).display().to_string());
    }
    None
}

/// `owner/name` with characters GitHub allows.
fn is_repo(text: &str) -> bool {
    let Some((owner, name)) = text.split_once('/') else { return false };
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part != "."
            && part != ".."
            && part.len() <= 100
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    part(owner) && part(name)
}

/// A branch, tag or commit name Git would take, which no command line reads as an option.
fn is_reference(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 200
        && !text.starts_with(['-', '/'])
        && !text.contains("..")
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// `user@host:path`, the address form `scp` uses.
fn is_scp_address(text: &str) -> bool {
    let Some((user_host, path)) = text.split_once(':') else { return false };
    user_host.contains('@') && !user_host.contains('/') && !path.is_empty() && !path.starts_with("//")
}

/// A folder inside a repository, with `/`: `None` when it would leave it.
fn clean_folder(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".").collect();
    parts.iter().all(|part| archive::portable(part)).then(|| parts.join("/"))
}

/// A GitHub page's path, `owner/name[.git][/tree/<ref>[/<folder>]]`, as a source.
fn github_page(path: &str) -> Option<Source> {
    let mut parts = path.split(['?', '#']).next().unwrap_or_default().split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?.trim_end_matches(".git");
    let repo = format!("{owner}/{name}");
    if !is_repo(&repo) {
        return None;
    }
    if !matches!(parts.next(), Some("tree" | "blob")) {
        return Some(Source::GitHub { repo, reference: String::new(), folder: String::new() });
    }
    let reference = parts.next().filter(|reference| is_reference(reference)).unwrap_or_default().to_owned();
    let folder = clean_folder(&parts.collect::<Vec<_>>().join("/"))?;
    Some(Source::GitHub { repo, reference, folder })
}

/// A Git source at `url`, if the address is one Git can be trusted with.
fn git(url: &str, reference: &str, folder: &str) -> Result<Source, String> {
    // No option-looking addresses, and none of Git's transports that run programs (`ext::`).
    if url.starts_with('-') || url.contains("::") || url.chars().any(char::is_whitespace) {
        return Err("That is not a Git address.".to_owned());
    }
    Ok(Source::Git { url: url.to_owned(), reference: reference.to_owned(), folder: folder.to_owned() })
}

/// Why getting a repository failed.
enum Failure {
    /// GitHub has no public repository there (it may be private).
    NotFound,
    /// Anything else, as a sentence.
    Other(String),
}

/// Gets `source` onto this computer, a repository into `cache` (replacing
/// what an earlier fetch left there), and returns the folder its skills
/// are in.
///
/// # Errors
/// What went wrong, as a sentence for the user.
pub(super) fn fetch(source: &Source, cache: &Path) -> Result<PathBuf, String> {
    let (root, folder) = match source {
        Source::Folder { path } => {
            let path = PathBuf::from(path);
            return if path.is_dir() { Ok(path) } else { Err(format!("There is no folder at {}.", path.display())) };
        }
        Source::GitHub { repo, reference, folder } => {
            let dest = cache.join(source.cache_key());
            let got = replace(&dest, |tmp| match download(repo, reference, tmp) {
                // A private repository: Git may have the user's sign-in for it.
                Err(Failure::NotFound) => clone_private(repo, reference, tmp),
                Err(Failure::Other(e)) => Err(e),
                Ok(()) => Ok(()),
            });
            (got.map(|()| dest)?, folder)
        }
        Source::Git { url, reference, folder } => {
            let dest = cache.join(source.cache_key());
            (replace(&dest, |tmp| clone(url, reference, tmp)).map(|()| dest)?, folder)
        }
    };
    let at = folder.split('/').filter(|part| !part.is_empty()).fold(root, |path, part| path.join(part));
    if at.is_dir() { Ok(at) } else { Err(format!("The repository has no folder {folder}.")) }
}

/// Fills a fresh folder with `fill`, then puts it at `dest` in place of what was there.
fn replace(dest: &Path, fill: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    static TEMPS: AtomicU64 = AtomicU64::new(0);
    let parent = dest.parent().ok_or("no folder for downloads")?;
    fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(".part-{}-{}", std::process::id(), TEMPS.fetch_add(1, Ordering::Relaxed)));
    let _ = super::remove_tree(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let filled = fill(&tmp).and_then(|()| {
        if dest.exists() {
            super::remove_tree(dest)?;
        }
        fs::rename(&tmp, dest).map_err(|e| format!("cannot keep the download: {e}"))
    });
    if filled.is_err() {
        let _ = super::remove_tree(&tmp);
    }
    filled
}

/// Downloads GitHub's `.tar.gz` of `repo` at `reference` into `dest`.
fn download(repo: &str, reference: &str, dest: &Path) -> Result<(), Failure> {
    let reference = if reference.is_empty() { "HEAD" } else { reference };
    let url = format!("https://codeload.github.com/{repo}/tar.gz/{reference}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("atroxabot/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent.get(&url).call().map_err(|e| Failure::Other(format!("Could not reach GitHub: {e}")))?;
    match response.status().as_u16() {
        200 => archive::unpack(response.body_mut().as_reader(), dest)
            .map_err(|e| Failure::Other(format!("Could not unpack {repo}: {e}"))),
        404 => Err(Failure::NotFound),
        status => Err(Failure::Other(format!("GitHub answered {status} for {repo}."))),
    }
}

/// Gets the GitHub repository `repo`, which is not public, with Git: over
/// HTTPS, where a credential helper may hold the user's sign-in, then over
/// SSH, where their key does.
fn clone_private(repo: &str, reference: &str, dest: &Path) -> Result<(), String> {
    let Err(https) = clone(&format!("https://github.com/{repo}.git"), reference, dest) else { return Ok(()) };
    if https.contains("cannot run git") {
        return Err(format!("GitHub has no public repository {repo}."));
    }
    // Reached, and something else is wrong (it has no such branch, say): SSH would say the same.
    if !no_access(&https) {
        return Err(https);
    }
    let Err(ssh) = clone(&format!("git@github.com:{repo}.git"), reference, dest) else { return Ok(()) };
    Err(if no_access(&ssh) {
        format!(
            "GitHub has no repository {repo} that is public or that Git on this computer may read (it tried HTTPS              and SSH)."
        )
    } else {
        ssh
    })
}

/// Whether Git's complaint says the repository is not there or not the
/// user's to read: GitHub does not tell the two apart.
fn no_access(error: &str) -> bool {
    [
        "could not read Username",
        "could not read Password",
        "terminal prompts disabled",
        "Authentication failed",
        "Permission denied",
        "Repository not found",
        "Host key verification failed",
    ]
    .iter()
    .any(|sign| error.contains(sign))
}

/// Gets the repository at `url`, at `reference` (its default branch when
/// empty), into the empty folder `dest` with Git: only that one commit.
fn clone(url: &str, reference: &str, dest: &Path) -> Result<(), String> {
    let wanted = if reference.is_empty() { "HEAD" } else { reference };
    crate::git::git(dest, &["init", "-q"]).map_err(|e| missing_git(&e))?;
    let mut args = vec!["-c", "protocol.ext.allow=never"];
    let ssh = batch_ssh(dest);
    if let Some(ssh) = &ssh {
        args.extend(["-c", ssh]);
    }
    args.extend(["fetch", "-q", "--depth", "1", "--", url, wanted]);
    crate::git::git(dest, &args).map_err(|e| format!("Git could not get {url}: {e}"))?;
    crate::git::git(dest, &["-c", "advice.detachedHead=false", "checkout", "-q", "FETCH_HEAD"])
        .map_err(|e| format!("Git could not check out {url}: {e}"))?;
    Ok(())
}

/// The `core.sshCommand` setting that makes SSH fail rather than ask for a
/// passphrase or about a host's key, in a terminal nobody may be looking
/// at: the user's own SSH command with `BatchMode`. `None` leaves Git's
/// choice alone, where the environment names the program or it is `plink`,
/// which has no such option.
fn batch_ssh(repository: &Path) -> Option<String> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some() {
        return None;
    }
    let own = crate::git::git(repository, &["config", "--get", "core.sshCommand"])
        .ok()
        .map(|said| String::from_utf8_lossy(&said).trim().to_owned())
        .filter(|command| !command.is_empty());
    batch_mode(own.as_deref().unwrap_or("ssh"))
}

/// `command` with `BatchMode`, as a `core.sshCommand` setting.
fn batch_mode(command: &str) -> Option<String> {
    (!command.to_lowercase().contains("plink")).then(|| format!("core.sshCommand={command} -o BatchMode=yes"))
}

/// Git's failure to start, as the user should read it.
fn missing_git(error: &str) -> String {
    if error.contains("cannot run git") {
        "cannot run git: this repository needs Git installed".to_owned()
    } else {
        error.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github(repo: &str, reference: &str, folder: &str) -> Source {
        Source::GitHub { repo: repo.into(), reference: reference.into(), folder: folder.into() }
    }

    #[test]
    fn parses_what_marketplace_add_takes() {
        assert_eq!(Source::parse(" anthropics/skills "), Ok(github("anthropics/skills", "", "")));
        assert_eq!(Source::parse("anthropics/skills#v2"), Ok(github("anthropics/skills", "v2", "")));
        assert_eq!(Source::parse("anthropics/skills@main"), Ok(github("anthropics/skills", "main", "")));
        assert_eq!(Source::parse("https://github.com/anthropics/skills.git"), Ok(github("anthropics/skills", "", "")));
        assert_eq!(
            Source::parse("https://github.com/anthropics/skills/tree/main/skills/pdf"),
            Ok(github("anthropics/skills", "main", "skills/pdf"))
        );
        assert!(matches!(Source::parse("https://gitlab.com/team/skills.git"), Ok(Source::Git { .. })));
        assert!(matches!(Source::parse("git@github.com:me/private.git"), Ok(Source::Git { .. })));
        assert!(matches!(Source::parse("C:\\skills"), Ok(Source::Folder { .. })));
        assert!(matches!(Source::parse("/home/me/skills"), Ok(Source::Folder { .. })));
        for bad in ["", "skills", "a/b/c", "-x/y", "owner/repo#--upload-pack=x", "https://github.com/x", "ext::sh -c x"]
        {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
        assert!(Source::parse("https://example.com/marketplace.json").is_err(), "only its repository can be read");
    }

    #[test]
    fn tells_no_access_from_other_failures() {
        let https = "fatal: could not read Username for 'https://github.com': terminal prompts disabled";
        let ssh = "git@github.com: Permission denied (publickey).
fatal: Could not read from remote repository.";
        assert!(no_access(https) && no_access(ssh) && no_access("ERROR: Repository not found."));
        assert!(!no_access("fatal: couldn't find remote ref v9"));
        assert!(!no_access("ssh: connect to host github.com port 22: Connection timed out"));
        assert_eq!(batch_mode("ssh").as_deref(), Some("core.sshCommand=ssh -o BatchMode=yes"));
        assert_eq!(
            batch_mode("C:/Windows/System32/OpenSSH/ssh.exe -i key").as_deref(),
            Some("core.sshCommand=C:/Windows/System32/OpenSSH/ssh.exe -i key -o BatchMode=yes")
        );
        assert_eq!(batch_mode("C:/PuTTY/plink.exe"), None);
    }

    /// Uses the network and the user's Git sign-in: name a private repository
    /// of theirs in `ATROXABOT_PRIVATE_REPO` (`owner/name`).
    #[test]
    #[ignore = "fetches a private repository with the user's Git"]
    fn fetches_a_private_repository() {
        let Ok(repo) = std::env::var("ATROXABOT_PRIVATE_REPO") else { return };
        let cache = std::env::temp_dir().join(format!("atroxabot-private-{}", std::process::id()));
        let got = fetch(&github(&repo, "", ""), &cache);
        let listed = got.as_ref().map(|root| fs::read_dir(root).map_or(0, Iterator::count));
        let _ = crate::skills::remove_tree(&cache);
        assert!(listed.is_ok_and(|files| files > 0), "{got:?}");
    }

    #[test]
    fn reads_marketplace_entries_that_live_elsewhere() {
        let entry = |value: Value| Source::from_entry(&value);
        let sha = "09bf1539d41f9ff355ba3eb5d05d4a75813423bb";
        assert_eq!(
            entry(serde_json::json!({ "source": "github", "repo": "a/b", "ref": "v1" })),
            Some(github("a/b", "v1", ""))
        );
        assert_eq!(
            entry(
                serde_json::json!({ "source": "url", "url": "https://github.com/a/b.git", "ref": "main", "sha": sha })
            ),
            Some(github("a/b", sha, "")),
            "the pinned commit wins"
        );
        assert_eq!(
            entry(serde_json::json!({ "source": "git-subdir", "url": "a/b", "path": "plugins/x" })),
            Some(github("a/b", "", "plugins/x"))
        );
        assert!(matches!(
            entry(serde_json::json!({ "source": "url", "url": "https://gitlab.com/a/b.git" })),
            Some(Source::Git { .. })
        ));
        for other in [
            serde_json::json!({ "source": "npm", "package": "x" }),
            serde_json::json!({ "source": "git-subdir", "url": "a/b", "path": "../up" }),
            serde_json::json!({ "source": "url", "url": "/etc" }),
        ] {
            assert_eq!(entry(other.clone()), None, "{other}");
        }
    }

    #[test]
    fn names_and_shows_sources() {
        let source = github("anthropics/skills", "09bf1539d41f9ff355ba3eb5d05d4a75813423bb", "skills/pdf");
        assert_eq!(source.to_string(), "anthropics/skills@09bf153 · skills/pdf");
        assert_eq!(source.name(), "pdf");
        assert_eq!(github("anthropics/skills", "", "").name(), "skills");
        assert_eq!(Source::parse("https://gitlab.com/team/tools.git").unwrap().name(), "tools");
        let json = serde_json::to_string(&github("a/b", "", "")).unwrap();
        assert_eq!(json, r#"{"kind":"github","repo":"a/b"}"#);
        assert_eq!(serde_json::from_str::<Source>(&json).unwrap(), github("a/b", "", ""));
    }
}
