//! A git worktree of its own for a session, and the cleaning up after it.
//!
//! A session that asks for one works on a branch of its own
//! (`atroxabot/<what it is about>-<id>`) checked out in a folder under
//! `~/.atroxabot/worktrees/`, so several sessions change one repository
//! without touching each other or the user's own checkout.
//!
//! Worktrees pile up unless something takes them away, so the app does:
//!
//! * a worktree is **lazy**: it is made when a message needs it and made
//!   again (from its branch, or from where that started) if it was taken
//!   away since, always in the same folder;
//! * a **clean** worktree (nothing uncommitted) holds nothing git does not
//!   have, so it may go: at once when its work has **landed** (its commits
//!   are in the branch it came from, merged or squashed), and once its
//!   session has been **idle** for as long as the settings allow. Its branch
//!   goes with it when nothing on it is unmerged, and stays otherwise;
//! * a worktree with **uncommitted changes** is never taken away by itself;
//! * deleting a session takes its worktree with it;
//! * what is left without a session (after a crash, say) is an **orphan**:
//!   removed when clean, listed for the user when not;
//! * a branch of the app's that no worktree and no session uses is deleted
//!   once it is merged.
//!
//! A worktree holds what git checks out, which is not all a project needs
//! to run: a local `.env`, say, is nobody's commit. A repository names such
//! files in a `.worktreeinclude` at its top (one pattern a line, written as
//! in a `.gitignore`), and every worktree made of it gets a copy of those of
//! them that **git ignores**: copied, never linked, so a session cannot
//! change the user's own; never over a file the checkout has; and only
//! ignored ones, so a copy is no change of the session's and goes with the
//! worktree like anything else git ignores. The app runs no command of the
//! project's for this: installing what a build needs is the agent's, asked
//! about like any other command.
//!
//! Nothing here touches a worktree that is in use: whoever works in one
//! holds a [`Lease`] on it. Everything goes through the user's own `git`
//! and blocks; run it off the UI thread.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{Config, create_private_dir};
use crate::git::git;
use crate::session::{Session, unix_now};

/// What the app's own branches start with.
pub const BRANCH_PREFIX: &str = "atroxabot/";
/// Where a branch of the app's keeps the branch it came from, in the repository's configuration.
const BASE_KEY: &str = "atroxabot-base";
/// How long the worktree of a session being deleted is waited for, while what worked in it ends.
const LET_GO: Duration = Duration::from_secs(20);
/// Longest name a worktree's folder takes from its repository's.
const NAME_LIMIT: usize = 24;
/// Longest part of a branch's name taken from the session's title.
const SLUG_LIMIT: usize = 32;
/// The file at a repository's top that names what its worktrees get beside what git checks out.
const INCLUDE: &str = ".worktreeinclude";
/// Most files, and most bytes, copied into a worktree for it: it is for a
/// handful of local settings, not for what a build makes.
const CARRY_FILES: usize = 2_000;
const CARRY_BYTES: u64 = 256 * 1024 * 1024;

/// A session's worktree: where it is and what it is of.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Worktree {
    /// The repository's own folder, where git is asked about it.
    pub repo: String,
    /// The worktree's folder. Empty while it is only wanted: the session's
    /// first message makes it.
    pub path: String,
    /// The branch made for it.
    pub branch: String,
    /// The branch it came from, which its work is merged back into; a
    /// commit when the repository was on none.
    pub base: String,
    /// The commit it started at: with another checked out, it has work of its own.
    pub start: String,
}

impl Worktree {
    /// Whether it was made already (it may have been taken away since).
    #[must_use]
    pub fn made(&self) -> bool {
        !self.path.is_empty()
    }

    /// The repository's name: its folder's.
    #[must_use]
    pub fn repo_name(&self) -> &str {
        let trimmed = self.repo.trim_end_matches(['/', '\\']);
        trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
    }
}

/// How a worktree stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// Whether its folder is there.
    pub exists: bool,
    /// The branch checked out in it (its own, unless the agent switched);
    /// empty on no branch, or when the folder is gone.
    pub branch: String,
    /// Files changed or added and not committed.
    pub changes: u32,
    /// Commits of its own that the branch it came from does not have.
    pub unmerged: u32,
    /// It has had commits of its own and all of them are in the branch it
    /// came from now, merged or squashed.
    pub landed: bool,
}

impl State {
    /// Whether taking the folder away loses nothing: all it holds is in git.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.changes == 0
    }
}

/// A hold on a worktree while something works in it (a turn, an agent's
/// process): nothing takes the folder away until every hold is dropped.
#[derive(Debug)]
pub struct Lease {
    path: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut held = leases();
        if let Some(at) = held.iter().position(|path| *path == self.path) {
            held.swap_remove(at);
        }
    }
}

/// The folder of every worktree something works in, once per hold on it.
static LEASES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Held while a worktree is made or taken away, so the two never cross.
static GATE: Mutex<()> = Mutex::new(());

fn leases() -> MutexGuard<'static, Vec<String>> {
    LEASES.lock().unwrap_or_else(PoisonError::into_inner)
}

fn gate() -> MutexGuard<'static, ()> {
    GATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether something holds the worktree in `path`.
fn leased(path: &str) -> bool {
    leases().iter().any(|held| held == path)
}

/// A session's worktree, ready to work in.
#[derive(Debug)]
pub struct Prepared {
    /// The worktree as it is now: new, or with a new start if it was made again.
    pub worktree: Worktree,
    /// The folder in it the session works in.
    pub workdir: String,
    /// The hold on it; keep it for as long as something works there.
    pub lease: Lease,
    /// What the user should know of the files its repository's
    /// `.worktreeinclude` names, when not all of them were copied into it.
    pub note: Option<String>,
}

/// The folder all worktrees are kept in: `~/.atroxabot/worktrees`.
///
/// # Errors
/// The platform reports no home directory.
pub fn root() -> Result<PathBuf, String> {
    Config::dir().map(|dir| dir.join("worktrees")).map_err(|e| e.to_string())
}

/// Gets the worktree `session` asked for ready and holds it: made on the
/// session's first message, made again if it was taken away since, left
/// alone if it is there.
///
/// # Errors
/// The session asked for none, its folder is in no repository (or one
/// without a commit), or git refused; the sentence says which.
pub fn prepare(session: &Session) -> Result<Prepared, String> {
    prepare_in(&root()?, session)
}

/// [`prepare`] with the worktrees kept in `root`.
fn prepare_in(root: &Path, session: &Session) -> Result<Prepared, String> {
    let wanted = session.worktree.as_ref().ok_or("This session has no worktree.")?;
    let _gate = gate();
    let (worktree, workdir, note) = if wanted.made() {
        let (worktree, note) = ensure(wanted)?;
        // The folder in it the session worked in, if the branch still has it.
        let workdir =
            Some(session.workdir.clone()).filter(|dir| Path::new(dir).is_dir()).unwrap_or(worktree.path.clone());
        (worktree, workdir, note)
    } else {
        create(root, &session.folder(), &session.id, &session.title)?
    };
    leases().push(worktree.path.clone());
    let lease = Lease { path: worktree.path.clone() };
    Ok(Prepared { worktree, workdir, lease, note })
}

/// The names git printed one after the other with `-z`.
fn names(listing: &[u8]) -> impl Iterator<Item = &str> {
    listing.split(|byte| *byte == 0).filter_map(|name| std::str::from_utf8(name).ok()).filter(|name| !name.is_empty())
}

/// Copies into the new worktree at `to` the files of the checkout `from`
/// that its `.worktreeinclude` names and git ignores (see the module's
/// comment). Returns what the user should know when not all of them went:
/// `None` when they did, or when the repository names none.
fn carry(from: &Path, to: &Path) -> Option<String> {
    let list = from.join(INCLUDE);
    if !list.is_file() {
        return None;
    }
    // What is not git's and matches the list, as git itself reads such patterns.
    let patterns = format!("--exclude-from={}", list.display());
    let listed = match git(from, &["ls-files", "-z", "--others", "--ignored", &patterns]) {
        Ok(listed) => listed,
        Err(problem) => return Some(format!("{INCLUDE} could not be read: {problem}")),
    };
    let listed: Vec<&str> = names(&listed).collect();
    // Only what git ignores: anything else would be a change of the session's in its worktree.
    let mut ignored: HashSet<String> = HashSet::new();
    for some in listed.chunks(100) {
        // Each name is one file's, whatever characters it holds.
        let mut args =
            vec!["--literal-pathspecs", "ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--"];
        args.extend(some);
        if let Ok(found) = git(from, &args) {
            ignored.extend(names(&found).map(str::to_owned));
        }
    }
    let (mut files, mut bytes, mut failed) = (0, 0, 0);
    for name in listed.into_iter().filter(|name| ignored.contains(*name)) {
        let inside = Path::new(name);
        // A name is a place inside the checkout, and so inside the worktree.
        if !inside.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
            continue;
        }
        let (source, target) = (from.join(inside), to.join(inside));
        // Files only (a link is not followed), and never over what the checkout put there.
        let Ok(about) = fs::symlink_metadata(&source) else { continue };
        if !about.is_file() || fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        if files == CARRY_FILES || bytes + about.len() > CARRY_BYTES {
            return Some(format!(
                "{INCLUDE} names more than is copied into a worktree ({CARRY_FILES} files, {} MB): the rest was \
                 left out.",
                CARRY_BYTES / (1024 * 1024)
            ));
        }
        let made = target.parent().map_or(Ok(()), fs::create_dir_all);
        match made.and_then(|()| fs::copy(&source, &target)) {
            Ok(_) => (files, bytes) = (files + 1, bytes + about.len()),
            Err(_) => failed += 1,
        }
    }
    (failed > 0).then(|| {
        let what = if failed == 1 { "1 file".to_owned() } else { format!("{failed} files") };
        format!("{what} that {INCLUDE} names could not be copied into the worktree.")
    })
}

/// What git prints for `args` in `folder`, trimmed; `None` when it fails.
fn out(folder: &Path, args: &[&str]) -> Option<String> {
    git(folder, args).ok().map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// A path git printed, the way the platform writes it.
fn native(path: &str) -> PathBuf {
    if cfg!(windows) { PathBuf::from(path.replace('/', "\\")) } else { PathBuf::from(path) }
}

/// The folder to ask git about the repository `folder` is in: its main
/// checkout (or the repository itself when it is bare), which outlives any
/// of its worktrees.
fn repository(folder: &Path) -> Option<PathBuf> {
    let common = out(folder, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = native(&common);
    let common = if common.is_absolute() { common } else { folder.join(common) };
    let main = common.file_name().is_some_and(|name| name == ".git").then(|| common.parent().map(Path::to_owned));
    Some(main.flatten().unwrap_or(common))
}

/// What a branch for a session called `title` is named after: its first
/// words, in lower-case letters and digits with dashes between.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for word in title.split(|c: char| !c.is_ascii_alphanumeric()).filter(|word| !word.is_empty()) {
        if slug.len() + word.len() + 1 > SLUG_LIMIT {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() { "session".to_owned() } else { slug }
}

/// A repository's name as part of a folder's: its letters, digits, dots,
/// dashes and underscores, a handful of them.
fn folder_part(name: &str) -> String {
    let kept: String =
        name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(NAME_LIMIT).collect();
    let kept = kept.trim_matches('.').to_owned();
    if kept.is_empty() { "repo".to_owned() } else { kept }
}

/// Whether `repo` has a local branch called `branch`.
fn has_branch(repo: &Path, branch: &str) -> bool {
    git(repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

/// Makes the worktree of the session `id` (titled `title`) for the
/// repository `folder` is in, under `root`. Returns it, the folder in it
/// that stands for `folder`, and what [`carry`] has to say.
fn create(root: &Path, folder: &Path, id: &str, title: &str) -> Result<(Worktree, String, Option<String>), String> {
    if !crate::session::valid_id(id) {
        return Err("This session cannot have a worktree.".to_owned());
    }
    let top = out(folder, &["rev-parse", "--show-toplevel"])
        .filter(|top| !top.is_empty())
        .ok_or_else(|| format!("{} is not in a git repository.", folder.display()))?;
    let repo = repository(folder).ok_or("The repository could not be found.")?;
    let start = out(folder, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .filter(|start| !start.is_empty())
        .ok_or("This repository has no commit yet: commit something first, then a worktree can be made.")?;
    // The branch it is on; a commit when it is on none.
    let base = out(folder, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| start.clone());
    // Where in the repository the session works: the same place in the worktree.
    let inside = out(folder, &["rev-parse", "--show-prefix"]).unwrap_or_default();

    let short: String = id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let mut branch = format!("{BRANCH_PREFIX}{}-{short}", slug(title));
    if has_branch(&repo, &branch) {
        branch = format!("{BRANCH_PREFIX}{}-{id}", slug(title));
    }
    let name = native(&top).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let path = root.join(format!("{}-{id}", folder_part(&name)));
    if path.exists() {
        return Err(format!("{} is in the way of the worktree.", path.display()));
    }
    create_private_dir(root).map_err(|e| e.to_string())?;
    let place = path.display().to_string();
    git(&repo, &["worktree", "add", "-b", &branch, "--", &place, &start])?;
    remember_base(&repo, &branch, &base);
    // From the checkout the session was started in: its local files are the ones meant.
    let note = carry(&native(&top), &path);
    let workdir = Some(path.join(native(inside.trim_end_matches('/')))).filter(|dir| dir.is_dir());
    let workdir = workdir.map_or_else(|| place.clone(), |dir| dir.display().to_string());
    let worktree = Worktree { repo: repo.display().to_string(), path: place, branch, base, start };
    Ok((worktree, workdir, note))
}

/// Notes on `branch`, in the repository's own configuration, which branch
/// it came from. Git forgets it when the branch is deleted.
fn remember_base(repo: &Path, branch: &str, base: &str) {
    // Without it a branch that outlives its session is compared with the repository's main branch.
    let _ = git(repo, &["config", &format!("branch.{branch}.{BASE_KEY}"), base]);
}

/// Whether `path` is a worktree's folder: it has git's pointer to its repository.
fn is_worktree(path: &Path) -> bool {
    path.join(".git").is_file() && out(path, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// Has the folder of `worktree` be there: left alone when it is, made again
/// from its branch (or, when that is gone, from where it came from) when it
/// was taken away. Returns the worktree as it is now, and what [`carry`]
/// has to say when it was made again.
fn ensure(worktree: &Worktree) -> Result<(Worktree, Option<String>), String> {
    let (repo, path) = (Path::new(&worktree.repo), Path::new(&worktree.path));
    if is_worktree(path) {
        return Ok((worktree.clone(), None));
    }
    if !repo.is_dir() {
        return Err(format!("The repository {} is no longer there.", worktree.repo));
    }
    if path.exists() {
        // What is left of one that was half taken away; anything else is not ours to delete.
        fs::remove_dir(path).map_err(|_| format!("{} is in the way of the worktree.", worktree.path))?;
    }
    // Git still lists a worktree whose folder is gone until it is told to look.
    let _ = git(repo, &["worktree", "prune"]);
    if let Some(parent) = path.parent() {
        create_private_dir(parent).map_err(|e| e.to_string())?;
    }
    let mut made = worktree.clone();
    if has_branch(repo, &worktree.branch) {
        git(repo, &["worktree", "add", "--", &worktree.path, &worktree.branch])?;
    } else {
        // Its work landed and the branch went with it: a new one, from where the old one came.
        let resolves = |name: &str| {
            !name.is_empty() && git(repo, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")]).is_ok()
        };
        let from = [worktree.base.as_str(), worktree.start.as_str(), "HEAD"]
            .into_iter()
            .find(|name| resolves(name))
            .ok_or("The repository has no commit to start the worktree from.")?;
        git(repo, &["worktree", "add", "-b", &worktree.branch, "--", &worktree.path, from])?;
        remember_base(repo, &worktree.branch, &worktree.base);
        made.start = out(path, &["rev-parse", "HEAD"]).unwrap_or_default();
    }
    // What git ignores went with the folder: the repository's local files are copied in again.
    let note = carry(repo, path);
    Ok((made, note))
}

/// What `tip` is compared with to see whether its work is merged: the
/// branch it came from and what that tracks, as far as they exist.
fn targets(repo: &Path, base: &str) -> Vec<String> {
    let resolves = |name: &str| git(repo, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")]).is_ok();
    let mut targets = Vec::new();
    if !base.is_empty() && resolves(base) {
        targets.push(base.to_owned());
        let upstream = format!("{base}@{{upstream}}");
        if resolves(&upstream) {
            targets.push(upstream);
        }
    }
    targets
}

/// How many commits `tip` has that none of `targets` has (the fewest
/// against any of them), and whether all it changed is in one of them:
/// merged, or squashed into a commit of its own there.
fn unmerged(folder: &Path, tip: &str, targets: &[String]) -> (u32, bool) {
    let mut fewest: Option<u32> = None;
    for target in targets {
        let count = out(folder, &["rev-list", "--count", &format!("{target}..{tip}")]).and_then(|n| n.parse().ok());
        let Some(count) = count else { continue };
        if count == 0 {
            return (0, true);
        }
        fewest = Some(fewest.map_or(count, |fewest: u32| fewest.min(count)));
    }
    let Some(fewest) = fewest else { return (0, false) };
    (fewest, targets.iter().any(|target| squashed(folder, tip, target)))
}

/// Whether everything `tip` changed since it left `target` is in `target`
/// as one commit: what a squash merge leaves. Checked the way `git cherry`
/// compares patches, on one commit that holds all of the branch's changes.
fn squashed(folder: &Path, tip: &str, target: &str) -> bool {
    let Some(fork) = out(folder, &["merge-base", target, tip]).filter(|fork| !fork.is_empty()) else { return false };
    let tree = |commit: &str| out(folder, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let (Some(ours), Some(theirs)) = (tree(tip), tree(&fork)) else { return false };
    if ours == theirs {
        // It changed nothing in the end.
        return true;
    }
    // A commit nothing points at, which git clears away in time. It needs an author even where none is set up.
    let whole = out(
        folder,
        &[
            "-c",
            "user.name=AtroxaBot",
            "-c",
            "user.email=atroxabot@localhost",
            "commit-tree",
            &ours,
            "-p",
            &fork,
            "-m",
            "squash",
        ],
    );
    let Some(whole) = whole.filter(|whole| !whole.is_empty()) else { return false };
    out(folder, &["cherry", target, &whole]).is_some_and(|line| line.starts_with('-'))
}

/// How `worktree` stands now.
#[must_use]
pub fn state(worktree: &Worktree) -> State {
    let (repo, path) = (Path::new(&worktree.repo), Path::new(&worktree.path));
    let exists = worktree.made() && is_worktree(path);
    if !exists {
        // Gone: what its branch still holds is all there is to say.
        let on_branch = repo.is_dir() && has_branch(repo, &worktree.branch);
        let unmerged = if on_branch { unmerged(repo, &worktree.branch, &targets(repo, &worktree.base)).0 } else { 0 };
        return State { unmerged, ..State::default() };
    }
    let changes = out(path, &["status", "--porcelain", "--untracked-files=normal"])
        .map_or(0, |listing| listing.lines().filter(|line| !line.trim().is_empty()).count());
    let branch = out(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
    let head = out(path, &["rev-parse", "HEAD"]).unwrap_or_default();
    let (unmerged, merged) = unmerged(path, "HEAD", &targets(path, &worktree.base));
    State {
        exists,
        branch,
        changes: u32::try_from(changes).unwrap_or(u32::MAX),
        unmerged: if merged { 0 } else { unmerged },
        landed: merged && !head.is_empty() && head != worktree.start,
    }
}

/// Deletes the branch `branch` of `repo` when it is one of the app's, no
/// worktree has it checked out, and nothing on it is unmerged into `base`
/// (or, when that is not known, into the branch the repository is on).
/// Returns whether the branch is still there.
fn drop_branch(repo: &Path, branch: &str, base: &str) -> bool {
    if !branch.starts_with(BRANCH_PREFIX) || !has_branch(repo, branch) {
        return false;
    }
    let noted = out(repo, &["config", "--get", &format!("branch.{branch}.{BASE_KEY}")]).unwrap_or_default();
    let base = [base, noted.as_str()].into_iter().find(|base| !base.is_empty()).map(str::to_owned);
    let base = base.or_else(|| out(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])).unwrap_or_default();
    // With nothing to compare it with, what it holds cannot be called merged.
    let done = base != branch && unmerged(repo, branch, &targets(repo, &base)).1;
    if !done {
        return true;
    }
    // Refused for a branch some worktree has checked out: then it stays.
    git(repo, &["branch", "-D", "--", branch]).is_err()
}

/// Deletes the folder `path` and all in it, read-only files too. Links are
/// removed, never followed.
fn delete_folder(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    // Windows refuses to delete a read-only file: git's own, and what build tools leave.
    let mut folders = vec![path.to_owned()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                folders.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                let mut permissions = meta.permissions();
                if permissions.readonly() {
                    #[expect(clippy::permissions_set_readonly_false, reason = "the file is deleted right after")]
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(entry.path(), permissions);
                }
            }
        }
    }
    fs::remove_dir_all(path)
}

/// Takes the worktree away: its folder, git's record of it, and its branch
/// when nothing on it is unmerged. With uncommitted changes it is refused
/// unless `discard`. Returns whether its branch is still there.
///
/// # Errors
/// Something works in it, it has uncommitted changes (and `discard` is not
/// set), or its folder could not be deleted (a program still uses it).
pub fn remove(worktree: &Worktree, discard: bool) -> Result<bool, String> {
    let _gate = gate();
    remove_held(worktree, discard)
}

/// [`remove`], for a caller that holds the gate.
fn remove_held(worktree: &Worktree, discard: bool) -> Result<bool, String> {
    if !worktree.made() {
        return Ok(false);
    }
    if leased(&worktree.path) {
        return Err("A session is still working in it.".to_owned());
    }
    let (repo, path) = (Path::new(&worktree.repo), Path::new(&worktree.path));
    if !discard && is_worktree(path) {
        let changes = state(worktree).changes;
        if changes > 0 {
            return Err(format!("It has {changes} uncommitted change{}.", if changes == 1 { "" } else { "s" }));
        }
    }
    let empty = || fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    if !discard && path.exists() && !is_worktree(path) && !empty() {
        return Err("Git can no longer say whether it holds changes.".to_owned());
    }
    let known = repo.is_dir();
    if known && path.exists() {
        // Forced: what is in it was checked above, and git would refuse over a submodule alone.
        let _ = git(repo, &["worktree", "remove", "--force", "--", &worktree.path]);
    }
    delete_folder(path).map_err(|e| format!("{} could not be deleted: {e}", worktree.path))?;
    if !known {
        return Ok(false);
    }
    let _ = git(repo, &["worktree", "prune"]);
    Ok(drop_branch(repo, &worktree.branch, &worktree.base))
}

/// A session's worktree, for [`sweep`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    /// The session's id.
    pub session: String,
    /// Its worktree.
    pub worktree: Worktree,
    /// When the session last changed, in Unix seconds.
    pub updated: u64,
}

/// A worktree's folder no session uses any more, kept because taking it
/// away would lose something.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Orphan {
    /// The folder.
    pub path: String,
    /// The repository it is of; empty when that cannot be told any more.
    pub repo: String,
    /// The branch checked out in it.
    pub branch: String,
    /// Its uncommitted changes; 0 when git can no longer say (its repository is gone).
    pub changes: u32,
}

/// What a [`sweep`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    /// How each session's worktree stands now, by session.
    pub states: Vec<(String, State)>,
    /// The sessions whose worktree was taken away.
    pub removed: Vec<String>,
    /// Folders left without a session that were kept.
    pub orphans: Vec<Orphan>,
    /// The repositories that still hold a worktree or a branch of the app's.
    pub repos: Vec<String>,
    /// What could not be taken away, and why: a line each.
    pub problems: Vec<String>,
}

/// Looks at every worktree and takes away what is no longer needed (see the
/// module's rules): `known` are the sessions' worktrees, `idle` how many
/// seconds a session may go unused before its clean worktree goes (`None`
/// to keep them), `repos` the repositories the app made worktrees in before.
#[must_use]
pub fn sweep(known: &[Known], idle: Option<u64>, repos: &[String]) -> Swept {
    match root() {
        Ok(root) => sweep_in(&root, known, idle, repos),
        Err(problem) => Swept { problems: vec![problem], ..Swept::default() },
    }
}

/// [`sweep`] with the worktrees kept in `root`.
fn sweep_in(root: &Path, known: &[Known], idle: Option<u64>, repos: &[String]) -> Swept {
    let mut swept = Swept::default();
    let now = unix_now();
    for entry in known.iter().filter(|entry| entry.worktree.made()) {
        let worktree = &entry.worktree;
        // Held from the look to the removal: nothing starts working in it in between.
        let _gate = gate();
        let mut stands = state(worktree);
        let unused = idle.is_some_and(|idle| now.saturating_sub(entry.updated) >= idle);
        let goes = stands.exists && stands.clean() && (stands.landed || unused) && !leased(&worktree.path);
        if goes {
            match remove_held(worktree, false) {
                Ok(_) => {
                    swept.removed.push(entry.session.clone());
                    stands = state(worktree);
                }
                Err(problem) => swept.problems.push(format!("{}: {problem}", worktree.path)),
            }
        } else if !stands.exists && Path::new(&worktree.repo).is_dir() {
            // Taken away earlier with its branch kept: the branch goes once it is merged.
            drop_branch(Path::new(&worktree.repo), &worktree.branch, &worktree.base);
        }
        swept.states.push((entry.session.clone(), stands));
    }
    let used: HashSet<&str> = known.iter().map(|entry| entry.worktree.path.as_str()).collect();
    let mut seen: Vec<String> = Vec::new();
    for folder in fs::read_dir(root).into_iter().flatten().flatten().map(|entry| entry.path()) {
        if !folder.is_dir() || used.contains(folder.display().to_string().as_str()) {
            continue;
        }
        let _gate = gate();
        match orphan(&folder) {
            Ok(repo) => seen.extend(repo),
            Err(kept) => {
                seen.push(kept.repo.clone());
                swept.orphans.push(kept);
            }
        }
    }
    // The repositories: stale records and merged branches of the app's go.
    let mut all: Vec<&str> = repos.iter().map(String::as_str).collect();
    all.extend(known.iter().filter(|entry| entry.worktree.made()).map(|entry| entry.worktree.repo.as_str()));
    all.extend(seen.iter().map(String::as_str));
    all.sort_unstable();
    all.dedup();
    let in_use: HashSet<&str> = known.iter().map(|entry| entry.worktree.branch.as_str()).collect();
    for repo in all.into_iter().filter(|repo| !repo.is_empty() && Path::new(repo).is_dir()) {
        let _gate = gate();
        if tidy(Path::new(repo), root, &in_use) {
            swept.repos.push(repo.to_owned());
        }
    }
    swept
}

/// Deals with a folder under the worktrees that no session uses: removed
/// (with its branch, when that is merged) if it is clean or empty, and
/// reported back as kept otherwise. `Ok` carries the repository it was of.
fn orphan(folder: &Path) -> Result<Option<String>, Orphan> {
    let path = folder.display().to_string();
    if leased(&path) {
        // A session that is being made right now: its record follows.
        return Ok(None);
    }
    if !is_worktree(folder) {
        // Nothing in it, or what git no longer knows: only an empty one is safe to drop.
        return match fs::remove_dir(folder) {
            Ok(()) => Ok(None),
            Err(_) => Err(Orphan { path, ..Orphan::default() }),
        };
    }
    let repo = repository(folder).map(|repo| repo.display().to_string()).unwrap_or_default();
    let branch = out(folder, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
    let worktree = Worktree { repo: repo.clone(), path: path.clone(), branch: branch.clone(), ..Worktree::default() };
    let changes = state(&worktree).changes;
    if changes > 0 || repo.is_empty() {
        return Err(Orphan { path, repo, branch, changes });
    }
    match remove_held(&worktree, false) {
        Ok(_) => Ok(Some(repo)),
        Err(_) => Err(Orphan { path, repo, branch, changes }),
    }
}

/// Clears what is stale in `repo`: records of worktrees whose folder is
/// gone, and branches of the app's that nothing uses (not `in_use` by a
/// session, not checked out) and that are merged. Returns whether the
/// repository still holds a worktree under `root` or a branch of the app's.
fn tidy(repo: &Path, root: &Path, in_use: &HashSet<&str>) -> bool {
    let _ = git(repo, &["worktree", "prune"]);
    let listing = out(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let root = root.display().to_string().replace('\\', "/").to_lowercase();
    let ours = listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|path| path.replace('\\', "/").to_lowercase().starts_with(&root));
    let pattern = format!("refs/heads/{BRANCH_PREFIX}");
    let branches = out(repo, &["for-each-ref", "--format=%(refname:short)", &pattern]).unwrap_or_default();
    let mut left = false;
    for branch in branches.lines().filter(|branch| !branch.is_empty()) {
        left |= in_use.contains(branch) || drop_branch(repo, branch, "");
    }
    ours || left
}

/// Takes away a folder under the worktrees that no session uses, whatever
/// is in it: what the user asks for in the settings.
///
/// # Errors
/// The folder is not one of the worktrees', something works in it, or it
/// could not be deleted.
pub fn remove_orphan(path: &str) -> Result<(), String> {
    remove_orphan_in(&root()?, path)
}

/// [`remove_orphan`] with the worktrees kept in `root`.
fn remove_orphan_in(root: &Path, path: &str) -> Result<(), String> {
    let folder = Path::new(path);
    if folder.parent() != Some(root) {
        return Err(format!("{path} is not a worktree of the app's."));
    }
    let _gate = gate();
    let repo = if is_worktree(folder) { repository(folder) } else { None };
    let branch = out(folder, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
    let worktree = Worktree {
        repo: repo.map(|repo| repo.display().to_string()).unwrap_or_default(),
        path: path.to_owned(),
        branch,
        ..Worktree::default()
    };
    remove_held(&worktree, true).map(drop)
}

/// How many bytes the files under `path` take. Links are not followed.
#[must_use]
pub fn size(path: &Path) -> u64 {
    let mut total = 0_u64;
    let mut folders = vec![path.to_owned()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                folders.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata().map_or(0, |meta| meta.len()));
            }
        }
    }
    total
}

/// Something the app asks about worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Look at all of them and take away what is no longer needed.
    Sweep {
        /// The sessions' worktrees.
        known: Vec<Known>,
        /// Seconds a session may go unused before its clean worktree goes; `None` to keep them.
        idle: Option<u64>,
        /// The repositories the app made worktrees in before.
        repos: Vec<String>,
    },
    /// Take a session's worktree away, as the session is deleted.
    Remove {
        /// The session's id.
        session: String,
        /// Its worktree.
        worktree: Worktree,
        /// Whether uncommitted changes go with it.
        discard: bool,
    },
    /// Take away a folder no session uses, whatever is in it.
    RemoveOrphan(String),
    /// Say how one session's worktree stands.
    State {
        /// The session's id.
        session: String,
        /// Its worktree.
        worktree: Worktree,
    },
    /// Measure these folders.
    Measure(Vec<String>),
}

/// What came of a [`Job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// A sweep is done.
    Swept(Swept),
    /// A session's worktree was taken away, or why it could not be.
    Removed {
        /// The session's id.
        session: String,
        /// Its worktree.
        worktree: Worktree,
        /// Why it is still there, if it is.
        problem: Option<String>,
    },
    /// A folder no session uses was taken away, or why it could not be.
    OrphanRemoved {
        /// The folder.
        path: String,
        /// Why it is still there, if it is.
        problem: Option<String>,
    },
    /// How a session's worktree stands.
    State {
        /// The session's id.
        session: String,
        /// How it stands.
        state: State,
    },
    /// How many bytes each folder takes.
    Sizes(Vec<(String, u64)>),
}

/// Does `job`.
#[must_use]
pub fn run(job: Job) -> Report {
    match job {
        Job::Sweep { known, idle, repos } => Report::Swept(sweep(&known, idle, &repos)),
        Job::Remove { session, worktree, discard } => {
            // Its agent's process is being ended as the session is deleted: that takes a moment.
            let patience = Instant::now() + LET_GO;
            while leased(&worktree.path) && Instant::now() < patience {
                std::thread::sleep(Duration::from_millis(100));
            }
            let problem = remove(&worktree, discard).err();
            Report::Removed { session, worktree, problem }
        }
        Job::RemoveOrphan(path) => {
            let problem = remove_orphan(&path).err();
            Report::OrphanRemoved { path, problem }
        }
        Job::State { session, worktree } => Report::State { state: state(&worktree), session },
        Job::Measure(paths) => {
            let measure = |path: String| {
                let bytes = size(Path::new(&path));
                (path, bytes)
            };
            Report::Sizes(paths.into_iter().map(measure).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit on `main` in a temporary folder, and a
    /// folder for its worktrees; `None` without git on this machine.
    fn repository_with_a_commit(name: &str) -> Option<(PathBuf, PathBuf)> {
        let dir = std::env::temp_dir().join(format!("atroxabot-wt-{name}-{}", crate::new_id()));
        let (repo, root) = (dir.join("project"), dir.join("worktrees"));
        fs::create_dir_all(repo.join("src")).expect("temp dir");
        if git(&repo, &["init", "--initial-branch=main"]).is_err() {
            let _ = fs::remove_dir_all(&dir);
            return None;
        }
        for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
            git(&repo, &["config", key, value]).expect("config");
        }
        fs::write(repo.join("a.txt"), "one\n").expect("write");
        fs::write(repo.join("src/lib.rs"), "fn main() {}\n").expect("write");
        fs::write(repo.join(".gitignore"), "target/\n").expect("write");
        commit(&repo, "first");
        // As git names it, links followed, so paths compare.
        let repo = repository(&repo).expect("a repository");
        Some((repo, root))
    }

    fn commit(folder: &Path, message: &str) {
        git(folder, &["add", "--all"]).expect("add");
        git(folder, &["commit", "--message", message]).expect("commit");
    }

    fn asking(repo: &Path, title: &str) -> Session {
        Session {
            workdir: repo.display().to_string(),
            title: title.to_owned(),
            worktree: Some(Worktree::default()),
            ..Session::new("agent")
        }
    }

    /// The session as the app keeps it once its worktree is ready.
    fn made(root: &Path, session: &mut Session) -> Lease {
        let prepared = prepare_in(root, session).expect("a worktree");
        session.worktree = Some(prepared.worktree);
        session.workdir = prepared.workdir;
        prepared.lease
    }

    fn known(session: &Session, updated: u64) -> Vec<Known> {
        vec![Known { session: session.id.clone(), worktree: session.worktree.clone().expect("a worktree"), updated }]
    }

    fn done(dir: &Path) {
        // The worktrees' parent holds the repository too.
        let _ = delete_folder(dir.parent().expect("a parent"));
    }

    #[test]
    fn names_are_safe_and_short() {
        assert_eq!(slug("Fix the parser's bug, please!"), "fix-the-parser-s-bug-please");
        assert_eq!(slug("  ---  "), "session");
        assert_eq!(slug("Ünïcödé only ✨"), "n-c-d-only");
        assert!(slug(&"word ".repeat(40)).len() <= SLUG_LIMIT);
        assert_eq!(folder_part("my repo (copy).git"), "myrepocopy.git");
        assert_eq!(folder_part("../.."), "repo");
        assert!(folder_part(&"x".repeat(200)).len() <= NAME_LIMIT);
        let worktree = Worktree { repo: "C:\\code\\parser\\".into(), ..Worktree::default() };
        assert_eq!(worktree.repo_name(), "parser");
        assert!(!worktree.made());
    }

    #[test]
    fn a_worktree_is_made_for_a_session_where_it_worked() {
        let Some((repo, root)) = repository_with_a_commit("make") else { return };
        // A session working in a folder of the repository works in the same folder of its worktree.
        let mut session = asking(&repo.join("src"), "Fix the parser");
        let lease = made(&root, &mut session);
        let worktree = session.worktree.clone().expect("a worktree");
        assert!(worktree.made() && Path::new(&worktree.path).starts_with(&root));
        assert_eq!(worktree.repo, repo.display().to_string());
        assert_eq!(worktree.base, "main");
        assert!(worktree.branch.starts_with("atroxabot/fix-the-parser-"), "{}", worktree.branch);
        assert_eq!(Path::new(&session.workdir), Path::new(&worktree.path).join("src"));
        assert!(Path::new(&session.workdir).join("lib.rs").is_file());
        let stands = state(&worktree);
        assert_eq!(
            stands,
            State { exists: true, branch: worktree.branch.clone(), changes: 0, unmerged: 0, landed: false }
        );
        // The user's own checkout is where it was.
        assert_eq!(out(&repo, &["symbolic-ref", "--short", "HEAD"]).as_deref(), Some("main"));

        // A second message finds it there; the session's own id names it.
        let again = prepare_in(&root, &session).expect("still there");
        assert_eq!((&again.worktree, again.workdir.as_str()), (&worktree, session.workdir.as_str()));
        drop(again);

        // While something works in it, it stays, however idle its session.
        let swept = sweep_in(&root, &known(&session, 0), Some(0), &[]);
        assert!(swept.removed.is_empty() && Path::new(&worktree.path).is_dir(), "{swept:?}");
        assert_eq!(remove(&worktree, true), Err("A session is still working in it.".to_owned()));
        drop(lease);
        done(&root);
    }

    #[test]
    fn a_worktree_gets_the_ignored_files_its_repository_names() {
        let Some((repo, root)) = repository_with_a_commit("carry") else { return };
        fs::write(repo.join(".gitignore"), "target/\n.env\nsecrets/\n*.local\n").expect("write");
        // As in a .gitignore: comments, folders, wildcards. It also names what is not for copying.
        let named = "# local settings\n.env\nsecrets/\n*.local\nnotes.txt\na.txt\nmissing.txt\n";
        fs::write(repo.join(INCLUDE), named).expect("write");
        commit(&repo, "ignore the local files");
        fs::create_dir_all(repo.join("secrets/deep")).expect("dir");
        fs::create_dir_all(repo.join("target")).expect("dir");
        fs::write(repo.join(".env"), "KEY=1\n").expect("write");
        fs::write(repo.join("secrets/deep/key.pem"), "pem").expect("write");
        fs::write(repo.join("src/db.local"), "local").expect("write");
        // Not ignored (it would be a change of the session's), not named, and git's own.
        fs::write(repo.join("notes.txt"), "mine").expect("write");
        fs::write(repo.join("target/out.bin"), "built").expect("write");
        fs::write(repo.join("a.txt"), "changed, not committed\n").expect("write");

        let mut session = asking(&repo, "Use the key");
        let prepared = prepare_in(&root, &session).expect("a worktree");
        assert_eq!(prepared.note, None);
        let tree = PathBuf::from(&prepared.worktree.path);
        let read = |name: &str| fs::read_to_string(tree.join(name)).ok();
        assert_eq!(read(".env").as_deref(), Some("KEY=1\n"));
        assert_eq!(read("secrets/deep/key.pem").as_deref(), Some("pem"));
        assert_eq!(read("src/db.local").as_deref(), Some("local"));
        assert_eq!((read("notes.txt"), read("target/out.bin")), (None, None));
        // What git checked out is not written over (git may write its line ends the platform's way).
        assert_eq!(read("a.txt").as_deref().map(str::trim_end), Some("one"));
        // The copies are no changes: the worktree is as clean as it was made.
        assert!(state(&prepared.worktree).clean());
        // They are copies: the session changing its own leaves the user's alone.
        fs::write(tree.join(".env"), "KEY=2\n").expect("write");
        assert_eq!(fs::read_to_string(repo.join(".env")).ok().as_deref(), Some("KEY=1\n"));

        // Taken away and made again, it gets them again, as they are now.
        session.worktree = Some(prepared.worktree.clone());
        session.workdir.clone_from(&prepared.workdir);
        drop(prepared);
        let worktree = session.worktree.clone().expect("a worktree");
        assert_eq!(remove(&worktree, false), Ok(false), "clean, with nothing of its own: its branch goes too");
        assert!(!tree.exists());
        fs::write(repo.join(".env"), "KEY=3\n").expect("write");
        let again = prepare_in(&root, &session).expect("made again");
        assert_eq!((read(".env").as_deref(), again.note.as_deref()), (Some("KEY=3\n"), None));
        drop(again);

        // A folder of that name is no list: nothing is copied, and nothing is wrong.
        fs::remove_file(repo.join(INCLUDE)).expect("remove");
        fs::create_dir_all(repo.join(INCLUDE)).expect("a folder in its place");
        assert_eq!(carry(&repo, &tree), None, "a folder is no list");
        done(&root);
    }

    #[test]
    fn names_git_prints_are_read_and_nothing_is_copied_outside() {
        assert_eq!(names(b"a.txt\0dir/b c.txt\0\0\xff\xfe\0").collect::<Vec<_>>(), ["a.txt", "dir/b c.txt"]);
        assert_eq!(names(b"").count(), 0);
        // No list, no copies, nothing to say: also where there is no repository at all.
        let dir = std::env::temp_dir().join(format!("atroxabot-wt-carry-none-{}", crate::new_id()));
        fs::create_dir_all(&dir).expect("temp dir");
        assert_eq!(carry(&dir, &dir.join("tree")), None);
        assert!(!dir.join("tree").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_outside_a_repository_gets_none() {
        let dir = std::env::temp_dir().join(format!("atroxabot-wt-none-{}", crate::new_id()));
        fs::create_dir_all(dir.join("plain")).expect("temp dir");
        // Inside somebody's repository (a temp folder under one) git answers instead: nothing to check.
        if git(&dir.join("plain"), &["rev-parse", "--git-dir"]).is_err() {
            let refused = prepare_in(&dir.join("worktrees"), &asking(&dir.join("plain"), "x"));
            assert!(refused.is_err_and(|why| why.contains("not in a git repository")));
            assert!(!dir.join("worktrees").exists(), "nothing is left behind");
        }
        // A repository without a commit has nothing to check out.
        if git(&dir.join("plain"), &["init"]).is_ok() {
            let refused = prepare_in(&dir.join("worktrees"), &asking(&dir.join("plain"), "x"));
            assert!(refused.is_err_and(|why| why.contains("no commit yet")));
        }
        let plain = Session { workdir: dir.display().to_string(), ..Session::new("agent") };
        assert!(prepare_in(&dir, &plain).is_err(), "a session that asked for none");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncommitted_work_is_never_taken_away_by_itself() {
        let Some((repo, root)) = repository_with_a_commit("dirty") else { return };
        let mut session = asking(&repo, "Try something");
        drop(made(&root, &mut session));
        let worktree = session.worktree.clone().expect("a worktree");
        let path = Path::new(&worktree.path);
        fs::write(path.join("new.txt"), "x").expect("write");
        fs::write(path.join("a.txt"), "changed\n").expect("write");
        assert_eq!(state(&worktree).changes, 2);
        // Idle for ever, and still there.
        let swept = sweep_in(&root, &known(&session, 0), Some(0), &[]);
        assert!(swept.removed.is_empty() && path.join("new.txt").is_file());
        assert_eq!(swept.states[0].1.changes, 2);
        assert_eq!(remove(&worktree, false), Err("It has 2 uncommitted changes.".to_owned()));
        // What git ignores (a build's output) is no work to keep.
        fs::remove_file(path.join("new.txt")).expect("remove");
        git(path, &["checkout", "--", "a.txt"]).expect("checkout");
        fs::create_dir_all(path.join("target")).expect("folder");
        fs::write(path.join("target/big.bin"), vec![0_u8; 4096]).expect("write");
        assert_eq!(state(&worktree).changes, 0);
        assert!(size(path) >= 4096);
        // Not idle long enough: it stays. Idle long enough: it goes, and its empty branch with it.
        let recent = sweep_in(&root, &known(&session, unix_now()), Some(3600), &[]);
        assert!(recent.removed.is_empty() && path.is_dir());
        let kept = sweep_in(&root, &known(&session, 0), None, &[]);
        assert!(kept.removed.is_empty() && path.is_dir(), "never, when the settings say so");
        let swept = sweep_in(&root, &known(&session, 0), Some(3600), &[]);
        assert_eq!(swept.removed, [session.id.clone()]);
        assert!(!path.exists() && !has_branch(Path::new(&worktree.repo), &worktree.branch));
        assert!(swept.repos.is_empty(), "nothing of the app's is left in the repository: {swept:?}");
        done(&root);
    }

    #[test]
    fn unmerged_work_keeps_its_branch_and_comes_back() {
        let Some((repo, root)) = repository_with_a_commit("unmerged") else { return };
        let mut session = asking(&repo, "Add a feature");
        drop(made(&root, &mut session));
        let worktree = session.worktree.clone().expect("a worktree");
        let path = Path::new(&worktree.path);
        fs::write(path.join("feature.txt"), "new\n").expect("write");
        commit(path, "feature");
        let stands = state(&worktree);
        assert_eq!((stands.changes, stands.unmerged, stands.landed), (0, 1, false));
        // Clean and idle: the folder goes, the branch with the commit stays.
        let swept = sweep_in(&root, &known(&session, 0), Some(60), &[]);
        assert_eq!(swept.removed, [session.id.clone()]);
        assert!(!path.exists() && has_branch(&repo, &worktree.branch));
        assert_eq!(swept.states[0].1, State { unmerged: 1, ..State::default() });
        assert_eq!(swept.repos, std::slice::from_ref(&worktree.repo), "the repository still holds its branch");
        // The next message brings it back, in the same folder, with the commit.
        let lease = made(&root, &mut session);
        assert_eq!(session.worktree.as_ref(), Some(&worktree), "the same worktree, the same start");
        assert!(path.join("feature.txt").is_file());
        drop(lease);

        // Merged into main by the user: its work has landed, and it goes with its branch, at once.
        git(&repo, &["merge", "--no-ff", "--no-edit", &worktree.branch]).expect("merge");
        assert!(state(&worktree).landed);
        let swept = sweep_in(&root, &known(&session, unix_now()), None, &[]);
        assert_eq!(swept.removed, [session.id.clone()]);
        assert!(!path.exists() && !has_branch(&repo, &worktree.branch));
        assert!(swept.repos.is_empty());
        // A message after that starts over from main as it is now, on a branch of the same name.
        let lease = made(&root, &mut session);
        let again = session.worktree.clone().expect("a worktree");
        assert_eq!((again.path.as_str(), again.branch.as_str()), (worktree.path.as_str(), worktree.branch.as_str()));
        assert_ne!(again.start, worktree.start, "from where main is now");
        assert!(path.join("feature.txt").is_file() && !state(&again).landed);
        drop(lease);
        assert_eq!(remove(&again, false), Ok(false));
        done(&root);
    }

    #[test]
    fn squashed_and_fast_forwarded_work_counts_as_landed() {
        let Some((repo, root)) = repository_with_a_commit("squash") else { return };
        let mut session = asking(&repo, "Two commits");
        drop(made(&root, &mut session));
        let worktree = session.worktree.clone().expect("a worktree");
        let path = Path::new(&worktree.path);
        for (file, message) in [("one.txt", "one"), ("two.txt", "two")] {
            fs::write(path.join(file), "x\n").expect("write");
            commit(path, message);
        }
        assert_eq!(state(&worktree).unmerged, 2);
        // Main moves on, then takes the branch as one commit, the way a squashed pull request lands.
        fs::write(repo.join("other.txt"), "y\n").expect("write");
        commit(&repo, "elsewhere");
        git(&repo, &["merge", "--squash", &worktree.branch]).expect("squash");
        git(&repo, &["commit", "--message", "Two commits (squashed)"]).expect("commit");
        let stands = state(&worktree);
        assert_eq!((stands.unmerged, stands.landed), (0, true), "{stands:?}");
        let swept = sweep_in(&root, &known(&session, unix_now()), None, &[]);
        assert_eq!(swept.removed, [session.id.clone()]);
        assert!(!has_branch(&repo, &worktree.branch), "a squashed branch goes too");

        // Fast-forwarded: main simply is where the branch was.
        let mut next = asking(&repo, "Fast");
        drop(made(&root, &mut next));
        let fast = next.worktree.clone().expect("a worktree");
        fs::write(Path::new(&fast.path).join("fast.txt"), "z\n").expect("write");
        commit(Path::new(&fast.path), "fast");
        git(&repo, &["merge", "--ff-only", &fast.branch]).expect("fast-forward");
        assert!(state(&fast).landed);
        // A worktree that never had a commit of its own has not landed anything.
        let mut idle = asking(&repo, "Idle");
        drop(made(&root, &mut idle));
        assert!(!state(idle.worktree.as_ref().expect("a worktree")).landed);
        assert_eq!(remove(&fast, false), Ok(false));
        assert_eq!(remove(idle.worktree.as_ref().expect("a worktree"), false), Ok(false));
        done(&root);
    }

    #[test]
    fn deleting_a_session_takes_its_worktree_and_orphans_are_swept() {
        let Some((repo, root)) = repository_with_a_commit("orphans") else { return };
        // A session deleted with uncommitted changes: only when told to discard them.
        let mut first = asking(&repo, "First");
        drop(made(&root, &mut first));
        let worktree = first.worktree.clone().expect("a worktree");
        fs::write(Path::new(&worktree.path).join("wip.txt"), "x").expect("write");
        assert!(remove(&worktree, false).is_err());
        assert_eq!(remove(&worktree, true), Ok(false));
        assert!(!Path::new(&worktree.path).exists() && !has_branch(&repo, &worktree.branch));
        assert_eq!(remove(&worktree, true), Ok(false), "gone already: nothing to do");

        // Folders no session knows of: a clean one goes, one with changes is kept and listed.
        let (mut clean, mut dirty, mut ahead) =
            (asking(&repo, "Clean"), asking(&repo, "Dirty"), asking(&repo, "Ahead"));
        for session in [&mut clean, &mut dirty, &mut ahead] {
            drop(made(&root, session));
        }
        let path = |session: &Session| PathBuf::from(&session.worktree.as_ref().expect("a worktree").path);
        let branch = |session: &Session| session.worktree.as_ref().expect("a worktree").branch.clone();
        fs::write(path(&dirty).join("wip.txt"), "x").expect("write");
        fs::write(path(&ahead).join("more.txt"), "x").expect("write");
        commit(&path(&ahead), "more");
        fs::create_dir_all(root.join("empty")).expect("folder");
        fs::create_dir_all(root.join("stray")).expect("folder");
        fs::write(root.join("stray/notes.txt"), "x").expect("write");
        let swept = sweep_in(&root, &[], Some(0), &[]);
        assert!(!path(&clean).exists() && !has_branch(&repo, &branch(&clean)));
        assert!(!path(&ahead).exists(), "its commit is safe on its branch");
        assert!(has_branch(&repo, &branch(&ahead)));
        assert!(!root.join("empty").exists());
        let mut kept: Vec<(&str, u32)> = swept.orphans.iter().map(|o| (o.path.as_str(), o.changes)).collect();
        kept.sort_unstable();
        let (dirty_path, stray) = (path(&dirty).display().to_string(), root.join("stray").display().to_string());
        let mut expected = vec![(dirty_path.as_str(), 1), (stray.as_str(), 0)];
        expected.sort_unstable();
        assert_eq!(kept, expected);
        assert_eq!(swept.repos, [repo.display().to_string()]);

        // The branch that outlived its session goes once it is merged, found through the repository alone.
        git(&repo, &["merge", "--no-edit", &branch(&ahead)]).expect("merge");
        let repos = [repo.display().to_string()];
        let swept = sweep_in(&root, &[], None, &repos);
        assert!(!has_branch(&repo, &branch(&ahead)));
        assert_eq!(swept.repos, repos, "the dirty orphan is still of this repository");
        // A branch of the user's own is never touched, merged or not.
        git(&repo, &["branch", "mine"]).expect("branch");
        git(&repo, &["branch", "atroxabot-not-ours"]).expect("branch");
        let _ = sweep_in(&root, &[], None, &repos);
        assert!(has_branch(&repo, "mine") && has_branch(&repo, "atroxabot-not-ours"));

        // What is kept goes when the user says so, and only what is under the worktrees.
        assert!(remove_orphan_in(&root, &repo.display().to_string()).is_err(), "not a worktree of the app's");
        assert!(remove_orphan_in(&root, &root.join("stray/notes.txt").display().to_string()).is_err());
        remove_orphan_in(&root, &dirty_path).expect("removed");
        remove_orphan_in(&root, &stray).expect("removed");
        assert!(!path(&dirty).exists() && !root.join("stray").exists());
        assert!(!has_branch(&repo, &branch(&dirty)), "it never had a commit of its own");
        let swept = sweep_in(&root, &[], None, &repos);
        assert!(swept.orphans.is_empty() && swept.repos.is_empty(), "{swept:?}");
        assert!(repo.join("a.txt").is_file(), "the repository itself is untouched");
        assert_eq!(
            out(&repo, &["worktree", "list", "--porcelain"]).map(|list| list.matches("worktree ").count()),
            Some(1)
        );
        done(&root);
    }

    #[test]
    fn a_repository_that_is_gone_does_not_stop_the_cleaning() {
        let Some((repo, root)) = repository_with_a_commit("gone") else { return };
        let mut session = asking(&repo, "Left behind");
        drop(made(&root, &mut session));
        let worktree = session.worktree.clone().expect("a worktree");
        delete_folder(&repo).expect("the repository is deleted");
        let lost = prepare_in(&root, &session);
        assert!(lost.is_err_and(|why| why.contains("no longer there")), "nothing to work on without it");
        // Git can say nothing about the folder any more: it is kept, with its session or without.
        let swept = sweep_in(&root, &known(&session, 0), Some(0), &[repo.display().to_string()]);
        assert!(swept.removed.is_empty() && swept.orphans.is_empty() && swept.repos.is_empty());
        assert_eq!(swept.states[0].1, State::default());
        let swept = sweep_in(&root, &[], Some(0), &[repo.display().to_string()]);
        assert_eq!(swept.orphans.len(), 1);
        assert!(remove(&worktree, false).is_err(), "not without being told to discard what is in it");
        // Deleting its session, or the user saying so, takes the folder.
        assert_eq!(remove(&worktree, true), Ok(false));
        assert!(!Path::new(&worktree.path).exists());
        done(&root);
    }
}
