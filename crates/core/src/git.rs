//! What the app shows and does about a folder's git repository, through the
//! user's own `git`: the branch, what changed, a file's diff, committing,
//! pushing and switching branch. Every call blocks; run it off the UI thread.

use std::path::Path;
use std::process::{Command, Stdio};

/// How much of a diff is kept; the rest is cut off with a note.
const DIFF_LIMIT: usize = 200_000;
/// Most files a folder's listing holds.
const FILES_LIMIT: usize = 20_000;
/// Most folder entries looked at when a folder outside a repository is walked.
const WALK_LIMIT: usize = 200_000;

/// A folder's repository at a glance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// The branch checked out, or a short note when detached.
    pub branch: String,
    /// The branch it tracks, if any.
    pub upstream: String,
    /// Commits not pushed yet.
    pub ahead: u32,
    /// Commits not pulled yet.
    pub behind: u32,
    /// What changed since the last commit.
    pub files: Vec<GitFile>,
    /// The local branches.
    pub branches: Vec<String>,
}

/// One changed file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFile {
    /// Its path within the repository, with forward slashes.
    pub path: String,
    /// What happened to it.
    pub change: Change,
    /// Lines added (0 for a new or binary file).
    pub added: u32,
    /// Lines removed.
    pub removed: u32,
}

/// What happened to a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Change {
    /// Its contents changed.
    #[default]
    Modified,
    /// It is new to the repository (added or not tracked yet).
    Added,
    /// It was deleted.
    Deleted,
    /// It was renamed or copied.
    Renamed,
    /// A merge left it in conflict.
    Conflict,
}

impl Change {
    /// The letter git users know it by.
    #[must_use]
    pub fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Conflict => "U",
        }
    }
}

/// Runs `git` in `folder` and returns what it printed, or what it complained about.
pub(crate) fn git(folder: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(folder).stdin(Stdio::null());
    // Fail rather than wait for a password nobody can type.
    command.env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console flashes up over the app.
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.trim();
    Err(if said.is_empty() {
        format!("git {} failed", args.first().copied().unwrap_or_default())
    } else {
        said.to_owned()
    })
}

/// The repository `folder` is in, or `None` when it is in none (or git is
/// not installed).
#[must_use]
pub fn status(folder: &Path) -> Option<GitStatus> {
    let listing = git(folder, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]).ok()?;
    let mut status = parse_status(&String::from_utf8_lossy(&listing));
    // Fails in a repository without a commit yet; the counts then stay 0.
    if let Ok(counts) = git(folder, &["diff", "--numstat", "-z", "HEAD"]) {
        for (path, added, removed) in parse_numstat(&String::from_utf8_lossy(&counts)) {
            if let Some(file) = status.files.iter_mut().find(|file| file.path == path) {
                (file.added, file.removed) = (added, removed);
            }
        }
    }
    if let Ok(names) = git(folder, &["branch", "--format=%(refname:short)"]) {
        status.branches =
            String::from_utf8_lossy(&names).lines().map(str::to_owned).filter(|name| !name.is_empty()).collect();
    }
    // A repository without a commit yet has no branch to list: the one it will start on stands in.
    if status.branches.is_empty() && !status.branch.is_empty() {
        status.branches.push(status.branch.clone());
    }
    Some(status)
}

/// Reads `git status --porcelain=v2 --branch -z`.
fn parse_status(listing: &str) -> GitStatus {
    let mut status = GitStatus::default();
    let mut records = listing.split('\0');
    while let Some(record) = records.next() {
        let (kind, rest) = record.split_once(' ').unwrap_or((record, ""));
        // The path is the last of a fixed number of fields, so it may hold spaces.
        let path = |fields: usize| rest.splitn(fields, ' ').nth(fields - 1).map(str::to_owned);
        let state = rest.split(' ').next().unwrap_or_default();
        let file = match kind {
            "#" => {
                let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
                match key {
                    "branch.head" => value.clone_into(&mut status.branch),
                    "branch.upstream" => value.clone_into(&mut status.upstream),
                    "branch.ab" => {
                        let mut counts = value.split(' ').map(|n| n.get(1..).and_then(|n| n.parse().ok()).unwrap_or(0));
                        status.ahead = counts.next().unwrap_or(0);
                        status.behind = counts.next().unwrap_or(0);
                    }
                    _ => {}
                }
                None
            }
            "1" => path(8).map(|path| {
                (
                    path,
                    if state.contains('D') {
                        Change::Deleted
                    } else if state.contains('A') {
                        Change::Added
                    } else {
                        Change::Modified
                    },
                )
            }),
            "2" => {
                // The name it had before follows as a record of its own.
                records.next();
                path(9).map(|path| (path, Change::Renamed))
            }
            "u" => path(10).map(|path| (path, Change::Conflict)),
            "?" => Some((rest.to_owned(), Change::Added)),
            _ => None,
        };
        if let Some((path, change)) = file.filter(|(path, _)| !path.is_empty()) {
            status.files.push(GitFile { path, change, ..GitFile::default() });
        }
    }
    status
}

/// Reads `git diff --numstat -z`: each file's path, lines added and removed.
fn parse_numstat(listing: &str) -> Vec<(String, u32, u32)> {
    let mut counts = Vec::new();
    let mut records = listing.split('\0');
    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (fields.next(), fields.next(), fields.next()) else { continue };
        // A rename has no path here: the old and the new name follow.
        let path = if path.is_empty() { records.nth(1).unwrap_or_default() } else { path };
        // A binary file's counts are "-".
        counts.push((path.to_owned(), added.parse().unwrap_or(0), removed.parse().unwrap_or(0)));
    }
    counts
}

/// The changes of one file since the last commit, as a unified diff.
///
/// # Errors
/// What git complained about.
pub fn diff(folder: &Path, file: &GitFile) -> Result<String, String> {
    let tracked = git(folder, &["diff", "HEAD", "--", &file.path]).unwrap_or_default();
    let bytes = if tracked.is_empty() && file.change == Change::Added {
        // Not tracked yet (or no commit yet): the whole file is new. git
        // exits with 1 when the two differ, which here they always do.
        let mut command = Command::new("git");
        command.args(["diff", "--no-index", "--", "/dev/null", &file.path]).current_dir(folder).stdin(Stdio::null());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
        command.output().map_err(|e| format!("cannot run git: {e}"))?.stdout
    } else {
        tracked
    };
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > DIFF_LIMIT {
        let mut end = DIFF_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… the rest of this diff is too long to show");
    }
    Ok(text)
}

/// Checks out the local branch `branch`.
///
/// # Errors
/// What git complained about (changes that would be overwritten).
pub fn switch(folder: &Path, branch: &str) -> Result<(), String> {
    // No branch starts with a dash; git would read it as an option.
    if branch.starts_with('-') || branch.is_empty() {
        return Err(format!("\"{branch}\" is not a branch"));
    }
    git(folder, &["checkout", branch, "--"]).map(drop)
}

/// The files under `folder`, relative to it with forward slashes, for
/// naming one in a message: what git tracks or would track (ignored files
/// left out) or, outside a repository, what a walk finds short of hidden
/// and dependency folders. At most [`FILES_LIMIT`] of them.
#[must_use]
pub fn files(folder: &Path) -> Vec<String> {
    if let Ok(listing) = git(folder, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"]) {
        let listing = String::from_utf8_lossy(&listing);
        return listing.split('\0').filter(|path| !path.is_empty()).take(FILES_LIMIT).map(str::to_owned).collect();
    }
    let mut found = Vec::new();
    // ponytail: a walk that gives up after WALK_LIMIT entries and knows no ignore file; fine for a folder
    // that is not a repository, wrong for a huge one. Read `.ignore` files if that comes up.
    let mut left = WALK_LIMIT;
    let mut folders = vec![folder.to_path_buf()];
    while let Some(dir) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            left = left.saturating_sub(1);
            if left == 0 || found.len() >= FILES_LIMIT {
                found.sort();
                return found;
            }
            if name.starts_with('.') || matches!(&*name, "node_modules" | "target") {
                continue;
            }
            let path = entry.path();
            // A link is not followed, so the walk cannot go in circles.
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => folders.push(path),
                Ok(kind) if kind.is_file() => {
                    if let Ok(inside) = path.strip_prefix(folder) {
                        found.push(inside.to_string_lossy().replace('\\', "/"));
                    }
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

/// Something the app asks of a folder's repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Read the status again.
    Refresh,
    /// Read one file's diff.
    Diff(GitFile),
    /// Check out this branch.
    Switch(String),
}

/// What came of a [`Job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The repository as it is now, after the job.
    Status {
        /// `None` when the folder is in no repository.
        status: Option<GitStatus>,
        /// What git complained about, if the job failed.
        problem: Option<String>,
    },
    /// A file's diff.
    Diff {
        /// The file's path.
        path: String,
        /// The diff, or what git complained about.
        text: Result<String, String>,
    },
}

/// Does `job` in `folder` and reads the status again.
#[must_use]
pub fn run(folder: &Path, job: Job) -> Report {
    let result = match job {
        Job::Diff(file) => return Report::Diff { text: diff(folder, &file), path: file.path },
        Job::Refresh => Ok(()),
        Job::Switch(branch) => switch(folder, &branch),
    };
    Report::Status { status: status(folder), problem: result.err() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_listing_is_read() {
        let listing = "# branch.oid 1234\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
            1 .M N... 100644 100644 100644 aaa bbb src/a file.rs\0\
            1 A. N... 000000 100644 100644 aaa bbb new.rs\0\
            1 .D N... 100644 100644 000000 aaa bbb gone.rs\0\
            2 R. N... 100644 100644 100644 aaa bbb R100 to.rs\0from.rs\0\
            u UU N... 100644 100644 100644 100644 aaa bbb ccc both.rs\0\
            ? notes with space.txt\0";
        let status = parse_status(listing);
        assert_eq!(
            (status.branch.as_str(), status.upstream.as_str(), status.ahead, status.behind),
            ("main", "origin/main", 2, 1)
        );
        let files: Vec<(&str, Change)> = status.files.iter().map(|f| (f.path.as_str(), f.change)).collect();
        assert_eq!(
            files,
            [
                ("src/a file.rs", Change::Modified),
                ("new.rs", Change::Added),
                ("gone.rs", Change::Deleted),
                ("to.rs", Change::Renamed),
                ("both.rs", Change::Conflict),
                ("notes with space.txt", Change::Added),
            ]
        );
        // Garbage is skipped, not trusted.
        assert_eq!(parse_status("1\0\0x y z\x002 R.\0"), GitStatus::default());
    }

    #[test]
    fn line_counts_are_read() {
        let counts = parse_numstat("3\t1\tsrc/a.rs\0-\t-\tpic.png\x005\t0\t\0old.rs\0new.rs\x007\t7\tlast.rs\0junk\0");
        assert_eq!(
            counts,
            [
                ("src/a.rs".to_owned(), 3, 1),
                ("pic.png".to_owned(), 0, 0),
                ("new.rs".to_owned(), 5, 0),
                ("last.rs".to_owned(), 7, 7)
            ]
        );
    }

    #[test]
    fn a_folder_outside_a_repository_is_walked() {
        let dir = std::env::temp_dir().join(format!("atroxabot-walk-{}", crate::new_id()));
        for folder in ["src/deep", ".hidden", "node_modules/x"] {
            std::fs::create_dir_all(dir.join(folder)).expect("folder");
        }
        for file in ["b.txt", "src/deep/a.rs", ".hidden/secret", "node_modules/x/index.js"] {
            std::fs::write(dir.join(file), "x").expect("write");
        }
        // Inside somebody's repository (a temp folder under one) git answers instead: nothing to check.
        if git(&dir, &["rev-parse", "--git-dir"]).is_err() {
            assert_eq!(files(&dir), ["b.txt", "src/deep/a.rs"]);
        }
        assert!(files(&dir.join("no-such-folder")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against a real repository in a temporary folder; needs `git`.
    #[test]
    fn a_repository_is_read_and_switched() {
        let dir = std::env::temp_dir().join(format!("atroxabot-git-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        if git(&dir, &["init", "--initial-branch=main"]).is_err() {
            // No git on this machine: nothing to check.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
            git(&dir, &["config", key, value]).expect("config");
        }
        assert!(status(&std::env::temp_dir().join("atroxabot-no-such-folder")).is_none());
        // Committing is the agent's to do; here it only makes the history the rest reads.
        let commit = |message: &str| {
            git(&dir, &["add", "--all"]).and_then(|_| git(&dir, &["commit", "--message", message])).map(drop)
        };

        std::fs::write(dir.join("a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(dir.join(".gitignore"), "built/\n").expect("write");
        std::fs::create_dir_all(dir.join("built")).expect("folder");
        std::fs::write(dir.join("built/out.bin"), "x").expect("write");
        assert_eq!(files(&dir), [".gitignore", "a.txt"], "what git ignores is not listed");
        std::fs::remove_file(dir.join(".gitignore")).expect("remove");
        std::fs::remove_dir_all(dir.join("built")).expect("remove");
        let before = status(&dir).expect("a repository");
        assert_eq!((before.branch.as_str(), before.branches.as_slice()), ("main", ["main".to_owned()].as_slice()));
        assert_eq!(before.files, [GitFile { path: "a.txt".into(), change: Change::Added, added: 0, removed: 0 }]);
        assert!(diff(&dir, &before.files[0]).expect("diff").contains("+two"));

        commit("first").expect("commit");
        assert!(status(&dir).expect("status").files.is_empty());

        std::fs::write(dir.join("a.txt"), "one\nthree\nfour\n").expect("write");
        let changed = status(&dir).expect("status");
        assert_eq!(changed.files, [GitFile { path: "a.txt".into(), change: Change::Modified, added: 2, removed: 1 }]);
        assert!(diff(&dir, &changed.files[0]).expect("diff").contains("-two"));
        commit("second").expect("commit");

        git(&dir, &["branch", "side"]).expect("branch");
        switch(&dir, "side").expect("switch");
        let after = status(&dir).expect("status");
        assert_eq!((after.branch.as_str(), after.branches.len()), ("side", 2));
        assert!(switch(&dir, "--orphan").is_err(), "an option is not a branch");
        let report = run(&dir, Job::Switch("main".into()));
        assert!(matches!(&report, Report::Status { status: Some(s), problem: None } if s.branch == "main"));
        let refused = run(&dir, Job::Switch("no-such-branch".into()));
        assert!(matches!(&refused, Report::Status { status: Some(s), problem: Some(_) } if s.branch == "main"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
