//! Updates: the newest release on GitHub, looked for and installed when the
//! user asks.
//!
//! The rules:
//!
//! * **Only when asked.** Nothing here runs by itself: [`run`] is for the
//!   button in the settings, and the app never looks for a release on its
//!   own, at start or on a timer.
//! * **A release is believed by its signature.** Every release carries a
//!   manifest (`update.manifest`): a first line with an Ed25519 signature in
//!   hex, then the JSON it signs, `{"version": "1.2.3", "files": {"<name>":
//!   {"size": 1, "sha256": "<hex>"}}}`. The signature is checked against the
//!   key in `packaging/update-key.pub`, compiled in; its other half signs
//!   the releases and is never in the repository.
//! * **Only forward.** A manifest that is not newer than the running
//!   version is no update, so an old signed release cannot be passed off as
//!   one.
//! * **Nothing unchecked is unpacked or run.** The download is the file the
//!   manifest names for this platform, and it must have that size and
//!   SHA-256 before it is touched.
//! * **The swap can be taken back.** Installing moves what the app runs
//!   from aside (`<name>.old`), moves the new version in, and moves the old
//!   one back when that fails. What is left beside it goes at the next
//!   start ([`sweep`]).
//!
//! What is swapped is the executable on Windows and Linux and the whole
//! `.app` bundle on macOS. Windows gets the bare executable; the other two
//! their release archive, unpacked with the system's `tar`.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use ring::{digest, signature};
use serde_json::Value;

/// The argument the app is started with after it updated itself: the copy
/// that made way may still be closing.
pub const AFTER_UPDATE: &str = "--after-update";
/// Where a person downloads the newest release, when the app cannot update itself.
pub const DOWNLOADS: &str = "https://github.com/Atroxabot/atroxabot/releases/latest";
/// The newest release's files.
const LATEST: &str = "https://github.com/Atroxabot/atroxabot/releases/latest/download";
/// Every release's files, under `v<version>/`.
const RELEASES: &str = "https://github.com/Atroxabot/atroxabot/releases/download";
/// The manifest's name among a release's files.
const MANIFEST: &str = "update.manifest";
/// The key releases are signed with: its public half, in hex.
const KEY: &str = include_str!("../../../packaging/update-key.pub");
/// A manifest is a few hundred bytes; more than this is not one.
const MANIFEST_LIMIT: u64 = 64 * 1024;
/// What is said of a download that is not what the manifest promised.
const MISMATCH: &str = "The download is not the file the release was signed with. Nothing was changed.";

/// A version of the app, as in `Cargo.toml` and a release's tag: three
/// numbers, and for a pre-release what follows the dash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    numbers: [u64; 3],
    pre: Option<String>,
}

impl Version {
    /// Reads `1.2.3`, `v1.2.3` or `1.2.3-rc.1`; build metadata (`+…`) is dropped.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        fn number(part: Option<&str>) -> Option<u64> {
            let part = part?;
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())).then(|| part.parse().ok())?
        }
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (numbers, pre) = match text.split_once('-') {
            Some((_, "")) => return None,
            Some((numbers, pre)) => (numbers, Some(pre.to_owned())),
            None => (text, None),
        };
        let mut parts = numbers.split('.');
        let numbers = [number(parts.next())?, number(parts.next())?, number(parts.next())?];
        parts.next().is_none().then_some(Self { numbers, pre })
    }

    /// The version that is running.
    ///
    /// # Panics
    /// Never: the workspace's version is checked to be one by a test.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the workspace's version is three numbers")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            // A pre-release comes before the release it leads to.
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(mine), Some(theirs)) => pre_order(mine, theirs),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Orders two pre-release tags part by part, as semantic versioning does:
/// numbers by value and before words, and the shorter of two alike first.
fn pre_order(mine: &str, theirs: &str) -> Ordering {
    let (mut mine, mut theirs) = (mine.split('.'), theirs.split('.'));
    loop {
        let order = match (mine.next(), theirs.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// A newer version, and its file for this platform as the signed manifest
/// describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The version.
    pub version: Version,
    name: String,
    size: u64,
    sha256: Vec<u8>,
}

/// A downloaded and checked version, ready to be moved in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    /// The version.
    pub version: Version,
    target: PathBuf,
    fresh: PathBuf,
}

/// Something to do about updates, off the interface's thread.
#[derive(Debug, Clone)]
pub enum Job {
    /// See whether a newer version is out.
    Check,
    /// Download a newer version and check it.
    Download(Release),
}

/// What a [`Job`] came to, or how far it is.
#[derive(Debug, Clone)]
pub enum Report {
    /// The newer version that is out, `None` when this is the newest, or
    /// why that could not be found out.
    Checked(Result<Option<Release>, String>),
    /// How much of a download has arrived.
    Progress {
        /// Bytes so far.
        done: u64,
        /// Bytes in all.
        size: u64,
    },
    /// The download is checked and ready, or why it is not.
    Downloaded(Result<Staged, String>),
}

/// Does `job`, telling `said` how a download gets on. Blocks: run it off
/// the interface's thread, and only because the user asked.
pub fn run(job: Job, said: &dyn Fn(Report)) -> Report {
    let source = Source::github();
    match job {
        Job::Check => Report::Checked(check(&source, &Version::current(), asset())),
        Job::Download(release) => {
            Report::Downloaded(target().and_then(|target| download(&source, &release, &target, said)))
        }
    }
}

/// Where releases come from, and the key they are believed by.
struct Source {
    /// The address of the newest release's files.
    latest: String,
    /// The address every release's files are under.
    releases: String,
    /// The public half of the signing key.
    key: Vec<u8>,
    /// Whether only HTTPS is spoken (always, outside the tests).
    secure: bool,
}

impl Source {
    fn github() -> Self {
        // An unreadable key verifies nothing, so every release is refused.
        Self { latest: LATEST.into(), releases: RELEASES.into(), key: unhex(KEY).unwrap_or_default(), secure: true }
    }

    fn agent(&self, limit: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(self.secure)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_global(Some(limit))
            .user_agent(concat!("atroxabot/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    }
}

/// The files of a release that the app updates itself from, one per
/// platform: Windows, Linux, macOS.
const ASSETS: [&str; 3] =
    ["atroxabot-windows-x86_64.exe", "atroxabot-linux-x86_64.tar.gz", "atroxabot-macos-arm64.tar.gz"];

/// The name of this platform's file in a release, if releases are built for it.
const fn asset() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some(ASSETS[0])
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(ASSETS[1])
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(ASSETS[2])
    } else {
        None
    }
}

/// Reads hex, surrounded by nothing but blanks.
fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |byte: u8| char::from(byte).to_digit(16).and_then(|value| u8::try_from(value).ok());
    text.chunks(2).map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?)).collect()
}

/// A release's manifest, its signature checked.
struct Manifest {
    version: Version,
    files: Value,
}

impl Manifest {
    /// Reads a manifest: the signature in hex on the first line, and after
    /// it the JSON that `key` must have signed, byte for byte.
    fn read(bytes: &[u8], key: &[u8]) -> Result<Self, String> {
        const UNSIGNED: &str = "The release is not signed with AtroxaBot's key, so it was not taken.";
        let end = bytes.iter().position(|&byte| byte == b'\n').ok_or(UNSIGNED)?;
        let (first, signed) = (&bytes[..end], &bytes[end + 1..]);
        let signed_with = std::str::from_utf8(first).ok().and_then(unhex).ok_or(UNSIGNED)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, key)
            .verify(signed, &signed_with)
            .map_err(|_| UNSIGNED)?;
        let unreadable = "The release does not say what it holds in a way this version understands.";
        let json: Value = serde_json::from_slice(signed).map_err(|_| unreadable)?;
        let version = json["version"].as_str().and_then(Version::parse).ok_or(unreadable)?;
        Ok(Self { version, files: json["files"].clone() })
    }

    /// The file named `name`, as the manifest describes it.
    fn release(&self, name: &str) -> Option<Release> {
        let file = self.files.get(name)?;
        let sha256 = file["sha256"].as_str().and_then(unhex).filter(|hash| hash.len() == 32)?;
        Some(Release { version: self.version.clone(), name: name.to_owned(), size: file["size"].as_u64()?, sha256 })
    }
}

/// Asks `source` for its newest release: `Some` when it is newer than
/// `current` and has the file `asset` (this platform's).
fn check(source: &Source, current: &Version, asset: Option<&str>) -> Result<Option<Release>, String> {
    let agent = source.agent(Duration::from_secs(60));
    let mut response = agent
        .get(format!("{}/{MANIFEST}", source.latest))
        .call()
        .map_err(|e| format!("Could not reach GitHub to look for updates: {e}"))?;
    match response.status().as_u16() {
        200 => {}
        // No release yet, or none made for updating from.
        404 => return Ok(None),
        status => return Err(format!("GitHub answered {status} when asked for the newest release.")),
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MANIFEST_LIMIT)
        .read_to_vec()
        .map_err(|e| format!("Could not read what GitHub says the newest release is: {e}"))?;
    let manifest = Manifest::read(&bytes, &source.key)?;
    if manifest.version <= *current {
        return Ok(None);
    }
    asset
        .and_then(|name| manifest.release(name))
        .map(Some)
        .ok_or_else(|| format!("Version {} is out, but it has no build for this kind of computer.", manifest.version))
}

/// What the app runs from, which an update replaces.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    /// The executable, or on macOS the `.app` bundle it is in.
    path: PathBuf,
    /// Whether it is a bundle.
    bundle: bool,
}

/// What this process runs from.
fn target() -> Result<Target, String> {
    let exe = std::env::current_exe().map_err(|e| format!("AtroxaBot cannot tell where it runs from: {e}"))?;
    // Through a link, the file itself is what is replaced. Windows would answer with a `\\?\` path.
    let exe = if cfg!(windows) { exe } else { fs::canonicalize(&exe).unwrap_or(exe) };
    Ok(target_of(&exe, cfg!(target_os = "macos")))
}

/// What an update replaces for the executable `exe`: itself, or where
/// `bundles` are a thing and it sits in one (`X.app/Contents/MacOS/`), that.
fn target_of(exe: &Path, bundles: bool) -> Target {
    let bundle = exe.parent().filter(|folder| bundles && folder.ends_with("Contents/MacOS")).and_then(|folder| {
        let bundle = folder.parent()?.parent()?;
        bundle.extension().is_some_and(|extension| extension == "app").then(|| bundle.to_path_buf())
    });
    match bundle {
        Some(path) => Target { path, bundle: true },
        None => Target { path: exe.to_path_buf(), bundle: false },
    }
}

/// The path beside `target` whose name is `target`'s with `suffix` after it.
fn beside(target: &Path, suffix: &str) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    target.with_file_name(name)
}

/// Renames `from` to `to`. On Windows a file another program has open just
/// now cannot be moved, and a virus scanner opens a new executable the
/// moment it is written: that is tried again for a couple of seconds.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            // Access denied, a sharing violation, a lock violation.
            Err(e) if cfg!(windows) && tries < 10 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            done => return done,
        }
    }
}

/// Deletes a file or a folder with all in it; one that is not there is fine.
fn remove(path: &Path) -> io::Result<()> {
    let gone = match fs::symlink_metadata(path) {
        Ok(found) if found.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match gone {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Downloads `release` beside `target`, checks it against the manifest and
/// makes it ready to move in.
fn download(source: &Source, release: &Release, target: &Target, said: &dyn Fn(Report)) -> Result<Staged, String> {
    let part = beside(&target.path, ".download");
    let fresh = fetch(source, release, &part, said).and_then(|()| stage(&part, release, target));
    if fresh.is_err() {
        let _ = remove(&part);
        let _ = remove(&beside(&target.path, ".new"));
    }
    Ok(Staged { version: release.version.clone(), target: target.path.clone(), fresh: fresh? })
}

/// Downloads `release` into the file `part`: it must have the size and the
/// SHA-256 the manifest gives it.
fn fetch(source: &Source, release: &Release, part: &Path, said: &dyn Fn(Report)) -> Result<(), String> {
    let mut file = fs::File::create(part).map_err(|e| {
        let folder = part.parent().unwrap_or(part).display();
        format!(
            "AtroxaBot cannot write to the folder it runs from ({folder}): {e}. Install the newest release instead."
        )
    })?;
    let url = format!("{}/v{}/{}", source.releases, release.version, release.name);
    let agent = source.agent(Duration::from_secs(30 * 60));
    let mut response = agent.get(url).call().map_err(|e| format!("Could not reach GitHub for the download: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("GitHub answered {status} for the download of version {}.", release.version));
    }
    let mut body = response.body_mut().as_reader();
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut buffer = vec![0; 64 * 1024];
    let (mut done, mut shown) = (0_u64, u64::MAX);
    loop {
        let read = body.read(&mut buffer).map_err(|e| format!("The download stopped: {e}"))?;
        if read == 0 {
            break;
        }
        done += read as u64;
        if done > release.size {
            return Err(MISMATCH.into());
        }
        hash.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(|e| format!("The download could not be saved: {e}"))?;
        // Said once per percent: a frame each, not one per packet.
        let percent = done * 100 / release.size.max(1);
        if percent != shown {
            shown = percent;
            said(Report::Progress { done, size: release.size });
        }
    }
    file.sync_all().map_err(|e| format!("The download could not be saved: {e}"))?;
    if done == release.size && hash.finish().as_ref() == release.sha256 { Ok(()) } else { Err(MISMATCH.into()) }
}

/// Where the new version is inside its unpacked archive.
const fn payload(bundle: bool) -> &'static str {
    if cfg!(target_os = "macos") {
        if bundle { "AtroxaBot.app" } else { "AtroxaBot.app/Contents/MacOS/atroxabot" }
    } else {
        "atroxabot-linux-x86_64/atroxabot"
    }
}

/// Turns the checked download `part` into what replaces `target`, beside
/// it: the file itself, or what its archive holds. Returns where that is.
fn stage(part: &Path, release: &Release, target: &Target) -> Result<PathBuf, String> {
    let fresh = beside(&target.path, ".new");
    let failed = |e: io::Error| format!("The new version could not be made ready: {e}");
    remove(&fresh).map_err(failed)?;
    if !release.name.ends_with(".tar.gz") {
        rename(part, &fresh).map_err(failed)?;
        return Ok(fresh);
    }
    fs::create_dir(&fresh).map_err(failed)?;
    let unpacked = Command::new("tar").arg("-xzf").arg(part).arg("-C").arg(&fresh).stdin(Stdio::null()).output();
    let _ = remove(part);
    match unpacked {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            return Err(format!(
                "The new version could not be unpacked: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Err(e) => return Err(format!("The new version could not be unpacked, tar did not run: {e}")),
    }
    let inside = fresh.join(payload(target.bundle));
    if inside.exists() { Ok(inside) } else { Err("The new version's archive does not hold the app.".into()) }
}

impl Release {
    /// A release of `version` that names no file, for showing one (the
    /// app's pictures of itself): downloading it fails.
    #[doc(hidden)]
    #[must_use]
    pub fn unnamed(version: Version) -> Self {
        Self { version, name: String::new(), size: 0, sha256: Vec::new() }
    }
}

impl Staged {
    /// A download of `version` that is nowhere, for showing one (the app's
    /// pictures of itself): installing it fails.
    #[doc(hidden)]
    #[must_use]
    pub fn nowhere(version: Version) -> Self {
        Self { version, target: PathBuf::new(), fresh: PathBuf::new() }
    }

    /// Moves the new version to where the app runs from, and the running
    /// one aside; a running program keeps working from the file it was
    /// started from. Returns what to start ([`relaunch`]).
    ///
    /// # Errors
    /// The folder cannot be written to. Then nothing has changed: the
    /// running version is back where it was.
    pub fn install(&self) -> Result<PathBuf, String> {
        if self.target.file_name().is_none() || self.fresh.file_name().is_none() {
            return Err("There is no downloaded version to put in place.".into());
        }
        let old = beside(&self.target, ".old");
        let failed = |e: io::Error| format!("The new version could not be put in place: {e}");
        #[cfg(unix)]
        if self.fresh.is_file() {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&self.fresh, fs::Permissions::from_mode(0o755)).map_err(failed)?;
        }
        remove(&old).map_err(failed)?;
        rename(&self.target, &old).map_err(failed)?;
        if let Err(e) = rename(&self.fresh, &self.target) {
            let _ = rename(&old, &self.target);
            return Err(failed(e));
        }
        // What an archive was unpacked into, now without the app.
        let _ = remove(&beside(&self.target, ".new"));
        Ok(self.target.clone())
    }
}

/// Starts the app at `start` (what [`Staged::install`] returned), telling
/// it that it comes after an update.
///
/// # Errors
/// It could not be started.
pub fn relaunch(start: &Path) -> io::Result<()> {
    let bundle = cfg!(target_os = "macos") && start.extension().is_some_and(|extension| extension == "app");
    let mut command = if bundle {
        // A bundle is opened, not run: the Dock then knows it.
        let mut open = Command::new("open");
        open.arg("-n").arg(start).arg("--args");
        open
    } else {
        Command::new(start)
    };
    command.arg(AFTER_UPDATE).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(drop)
}

/// Clears away what updating left beside the app: the version before, and
/// a download that was never installed. `updated` says the app comes
/// straight from an update: the version before may still be closing, so it
/// is waited for a little, and where an installer listed the app, its
/// entry is given the new version.
pub fn sweep(updated: bool) {
    let Ok(target) = target() else { return };
    let old = beside(&target.path, ".old");
    let tries = if updated { 40 } else { 1 };
    for attempt in 1..=tries {
        if remove(&old).is_ok() || attempt == tries {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = remove(&beside(&target.path, ".new"));
    let _ = remove(&beside(&target.path, ".download"));
    #[cfg(windows)]
    if updated {
        installed::note_version(&target.path);
    }
}

/// The app's entry among Windows' installed apps, which the installer
/// (`packaging/windows/atroxabot.iss`) writes.
#[cfg(windows)]
mod installed {
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// The entry's key: the installer's `AppId` with Inno Setup's `_is1`.
    const KEY: &str =
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{6F0C3A52-9D1B-4E7A-B8C4-2A5D7E913F60}_is1";

    fn reg() -> Command {
        let mut command = Command::new("reg");
        // CREATE_NO_WINDOW: no console flashes up over the app.
        command.creation_flags(0x0800_0000).stdin(Stdio::null()).stderr(Stdio::null());
        command
    }

    /// Whether `listing` (what `reg query` printed) gives `folder` as the value `name`.
    pub(super) fn lists(listing: &str, name: &str, folder: &str) -> bool {
        let trimmed = |path: &str| path.trim().trim_end_matches('\\').to_lowercase();
        listing.lines().any(|line| {
            let mut parts = line.trim().splitn(3, "    ");
            parts.next() == Some(name)
                && parts.next().is_some()
                && parts.next().is_some_and(|value| trimmed(value) == trimmed(folder))
        })
    }

    /// Writes the running version into the entry, when the entry is for the
    /// copy at `exe`: the list of installed apps then shows what is installed.
    pub(super) fn note_version(exe: &Path) {
        let Some(folder) = exe.parent().and_then(Path::to_str) else { return };
        let Ok(found) = reg().args(["query", KEY, "/v", "InstallLocation"]).output() else { return };
        if !found.status.success() || !lists(&String::from_utf8_lossy(&found.stdout), "InstallLocation", folder) {
            return;
        }
        let version = env!("CARGO_PKG_VERSION");
        let _ = reg()
            .args(["add", KEY, "/v", "DisplayVersion", "/t", "REG_SZ", "/d", version, "/f"])
            .stdout(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead as _, BufReader};
    use std::sync::Mutex;

    use ring::signature::KeyPair as _;

    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} is a version"))
    }

    /// A key pair for the tests' releases.
    fn pair() -> signature::Ed25519KeyPair {
        signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("a seed is a key")
    }

    /// A manifest as the release pipeline writes it: signature, newline, JSON.
    fn manifest(json: &str) -> Vec<u8> {
        format!("{}\n{json}", hex(pair().sign(json.as_bytes()).as_ref())).into_bytes()
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    fn sha256(bytes: &[u8]) -> String {
        hex(digest::digest(&digest::SHA256, bytes).as_ref())
    }

    /// The JSON of a release `version` whose file `name` is `bytes`.
    fn describing(version: &str, name: &str, bytes: &[u8]) -> String {
        format!(
            r#"{{"version":"{version}","files":{{"{name}":{{"size":{},"sha256":"{}"}}}}}}"#,
            bytes.len(),
            sha256(bytes)
        )
    }

    /// A server on this computer that answers `GET` with the file at that
    /// path (404 for none); returns a [`Source`] that reads from it.
    fn serving(files: Vec<(&str, Vec<u8>)>) -> Source {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let address = format!("http://{}", listener.local_addr().expect("an address"));
        let files: HashMap<String, Vec<u8>> = files.into_iter().map(|(path, bytes)| (path.to_owned(), bytes)).collect();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                let path = first.split(' ').nth(1).unwrap_or_default();
                let _ = match files.get(path) {
                    Some(bytes) => stream
                        .write_all(
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", bytes.len())
                                .as_bytes(),
                        )
                        .and_then(|()| stream.write_all(bytes)),
                    None => {
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    }
                };
            }
        });
        Source {
            latest: format!("{address}/latest"),
            releases: format!("{address}/download"),
            key: pair().public_key().as_ref().to_vec(),
            secure: false,
        }
    }

    /// An empty folder of the test's own, with an "installed" app in it.
    fn an_app(name: &str) -> Target {
        let folder = std::env::temp_dir().join(format!("atroxabot-update-{name}-{}", crate::new_id()));
        fs::create_dir_all(&folder).expect("temp dir");
        let path = folder.join("atroxabot.exe");
        fs::write(&path, b"the running version").expect("temp file");
        Target { path, bundle: false }
    }

    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn versions_are_read_and_ordered() {
        assert_eq!(version("v1.2.3").to_string(), "1.2.3");
        assert_eq!(version(" 0.10.0-rc.1+build5 ").to_string(), "0.10.0-rc.1");
        for junk in
            ["", "1.2", "1.2.3.4", "1.2.x", "1.2.-3", "+1.2.3", "1..3", "1.2.3-", "v", "1.2.99999999999999999999"]
        {
            assert_eq!(Version::parse(junk), None, "{junk:?}");
        }
        let ordered =
            ["0.1.0", "0.1.1", "0.2.0-alpha", "0.2.0-rc.1", "0.2.0-rc.2", "0.2.0-rc.10", "0.2.0", "0.10.0", "1.0.0"];
        for pair in ordered.windows(2) {
            assert!(version(pair[0]) < version(pair[1]), "{} comes before {}", pair[0], pair[1]);
        }
        assert_eq!(version("1.0.0").cmp(&version("v1.0.0+later")), Ordering::Equal);
        // The workspace's own version is one, so `current` cannot panic.
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_release_key_is_a_key() {
        assert_eq!(
            unhex(KEY).map(|key| key.len()),
            Some(32),
            "packaging/update-key.pub holds an Ed25519 public key in hex"
        );
        assert_eq!(unhex(" 0aFf\n"), Some(vec![0x0a, 0xff]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn only_a_signed_manifest_is_believed() {
        let key = pair().public_key().as_ref().to_vec();
        let json = describing("1.4.0", "app.exe", b"new");
        let signed = manifest(&json);
        let read = Manifest::read(&signed, &key).expect("signed by the key");
        assert_eq!(read.version, version("1.4.0"));
        let release = read.release("app.exe").expect("the file is listed");
        assert_eq!((release.size, release.name.as_str()), (3, "app.exe"));
        assert!(read.release("other.exe").is_none());

        // One byte of what was signed changed: a later version, say.
        let mut forged = signed.clone();
        let at = forged.iter().position(|&byte| byte == b'4').expect("the version");
        forged[at] = b'9';
        assert!(Manifest::read(&forged, &key).is_err());
        // Signed, but by somebody else; and by nobody.
        assert!(Manifest::read(&signed, &[1; 32]).is_err());
        assert!(Manifest::read(&signed, &[]).is_err());
        assert!(Manifest::read(json.as_bytes(), &key).is_err());
        assert!(Manifest::read(b"", &key).is_err());
        assert!(Manifest::read(b"\n", &key).is_err());
        assert!(Manifest::read(&[0xff, b'\n', 0xff], &key).is_err());
        // Signed, and not a manifest.
        assert!(Manifest::read(&manifest("[]"), &key).is_err());
        assert!(Manifest::read(&manifest(r#"{"version":"soon"}"#), &key).is_err());
        // A file without a real hash is not offered.
        let vague = manifest(r#"{"version":"2.0.0","files":{"app.exe":{"size":3,"sha256":"abcd"}}}"#);
        assert!(Manifest::read(&vague, &key).expect("signed").release("app.exe").is_none());
    }

    #[test]
    fn a_check_offers_only_what_is_newer_and_built_for_here() {
        let json = describing("1.4.0", "app.exe", b"new");
        let source = serving(vec![("/latest/update.manifest", manifest(&json))]);
        let offered = check(&source, &version("1.3.9"), Some("app.exe")).expect("reachable").expect("newer");
        assert_eq!(offered.version, version("1.4.0"));
        assert_eq!(check(&source, &version("1.4.0"), Some("app.exe")), Ok(None), "the same version is no update");
        assert_eq!(check(&source, &version("2.0.0"), Some("app.exe")), Ok(None), "an older one neither");
        let no_build = check(&source, &version("1.0.0"), Some("app-for-elsewhere"));
        assert!(no_build.is_err_and(|why| why.contains("1.4.0") && why.contains("no build")));
        assert!(check(&source, &version("1.0.0"), None).is_err());

        // No release was published yet.
        assert_eq!(check(&serving(Vec::new()), &version("1.0.0"), Some("app.exe")), Ok(None));
        // A release signed with another key is refused, newer or not.
        let mut stranger = serving(vec![("/latest/update.manifest", manifest(&json))]);
        stranger.key = vec![9; 32];
        assert!(check(&stranger, &version("1.0.0"), Some("app.exe")).is_err_and(|why| why.contains("not signed")));
    }

    #[test]
    fn a_download_is_checked_and_swapped_in() {
        let fresh = vec![42_u8; 300_000];
        let json = describing("1.4.0", "app.exe", &fresh);
        let source =
            serving(vec![("/latest/update.manifest", manifest(&json)), ("/download/v1.4.0/app.exe", fresh.clone())]);
        let target = an_app("swap");
        let folder = target.path.parent().expect("a folder").to_path_buf();
        let release = check(&source, &version("1.0.0"), Some("app.exe")).expect("reachable").expect("newer");

        let progress = Mutex::new(Vec::new());
        let staged = download(&source, &release, &target, &|report| {
            if let Report::Progress { done, size } = report {
                progress.lock().expect("the list").push((done, size));
            }
        })
        .expect("downloaded");
        let progress = progress.into_inner().expect("the list");
        assert_eq!(progress.last(), Some(&(300_000, 300_000)));
        assert!(progress.len() <= 101, "at most one report per percent");
        assert_eq!(names_in(&folder), ["atroxabot.exe", "atroxabot.exe.new"]);
        assert_eq!(fs::read(&target.path).expect("the app"), b"the running version", "nothing is swapped yet");

        let start = staged.install().expect("installed");
        assert_eq!(start, target.path);
        assert_eq!(fs::read(&target.path).expect("the app"), fresh);
        assert_eq!(fs::read(folder.join("atroxabot.exe.old")).expect("the one before"), b"the running version");
        assert_eq!(names_in(&folder), ["atroxabot.exe", "atroxabot.exe.old"]);
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_download_that_differs_changes_nothing() {
        let promised = vec![1_u8; 5000];
        let json = describing("1.4.0", "app.exe", &promised);
        let release = Manifest::read(&manifest(&json), pair().public_key().as_ref())
            .expect("signed")
            .release("app.exe")
            .expect("listed");
        // Same size with other bytes, longer, shorter, and not there at all.
        let mut altered = promised.clone();
        altered[4000] = 2;
        for served in [Some(altered), Some(vec![1_u8; 5001]), Some(vec![1_u8; 4999]), None] {
            let gone = served.is_none();
            let source = serving(served.map(|bytes| ("/download/v1.4.0/app.exe", bytes)).into_iter().collect());
            let target = an_app("differs");
            let folder = target.path.parent().expect("a folder").to_path_buf();
            let refused = download(&source, &release, &target, &|_| {}).expect_err("not what was signed");
            assert_eq!(refused == MISMATCH, !gone, "{refused}");
            assert_eq!(names_in(&folder), ["atroxabot.exe"], "no download is left behind");
            assert_eq!(fs::read(&target.path).expect("the app"), b"the running version");
            let _ = fs::remove_dir_all(&folder);
        }
    }

    #[test]
    fn a_failed_swap_puts_the_running_version_back() {
        let target = an_app("back");
        let folder = target.path.parent().expect("a folder").to_path_buf();
        // What was staged has gone missing (cleared away by hand, say).
        let staged = Staged { version: version("1.4.0"), target: target.path.clone(), fresh: folder.join("missing") };
        assert!(staged.install().is_err());
        assert_eq!(fs::read(&target.path).expect("the app"), b"the running version");
        assert_eq!(names_in(&folder), ["atroxabot.exe"]);
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_bundle_is_replaced_whole() {
        let inside = Path::new("/Applications/AtroxaBot.app/Contents/MacOS/atroxabot");
        assert_eq!(target_of(inside, true), Target { path: "/Applications/AtroxaBot.app".into(), bundle: true });
        assert_eq!(target_of(inside, false), Target { path: inside.into(), bundle: false });
        let bare = Path::new("/home/me/.local/bin/atroxabot");
        assert_eq!(target_of(bare, true), Target { path: bare.into(), bundle: false });
        let odd = Path::new("/opt/thing/Contents/MacOS/atroxabot");
        assert_eq!(target_of(odd, true), Target { path: odd.into(), bundle: false });
        assert_eq!(beside(Path::new("/a/AtroxaBot.app"), ".old"), Path::new("/a/AtroxaBot.app.old"));
        assert_eq!(beside(Path::new("/a/atroxabot.exe"), ".new"), Path::new("/a/atroxabot.exe.new"));
    }

    /// The release archive of this platform, unpacked and moved in.
    #[cfg(unix)]
    #[test]
    fn an_archive_is_unpacked_and_swapped_in() {
        let folder = std::env::temp_dir().join(format!("atroxabot-update-archive-{}", crate::new_id()));
        // What is installed: a bundle on macOS, the binary elsewhere.
        let bundle = cfg!(target_os = "macos");
        let target = if bundle {
            fs::create_dir_all(folder.join("AtroxaBot.app/Contents/MacOS")).expect("temp dir");
            fs::write(folder.join("AtroxaBot.app/Contents/MacOS/atroxabot"), b"old").expect("temp file");
            Target { path: folder.join("AtroxaBot.app"), bundle }
        } else {
            fs::create_dir_all(&folder).expect("temp dir");
            fs::write(folder.join("atroxabot"), b"old").expect("temp file");
            Target { path: folder.join("atroxabot"), bundle }
        };
        // The release's archive, laid out as the pipeline packs it.
        let packed = folder.join("packed");
        let binary = packed.join(payload(false));
        fs::create_dir_all(binary.parent().expect("a folder")).expect("temp dir");
        fs::write(&binary, b"new").expect("temp file");
        let top = payload(false).split('/').next().expect("a first part");
        let archive = folder.join("release.tar.gz");
        let made = Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&packed).arg(top).status();
        assert!(made.is_ok_and(|status| status.success()), "tar packs the archive");
        let bytes = fs::read(&archive).expect("the archive");
        fs::remove_dir_all(&packed).expect("cleared");
        fs::remove_file(&archive).expect("cleared");

        let json = describing("1.4.0", "app.tar.gz", &bytes);
        let source = serving(vec![("/download/v1.4.0/app.tar.gz", bytes)]);
        let release = Manifest::read(&manifest(&json), pair().public_key().as_ref())
            .expect("signed")
            .release("app.tar.gz")
            .expect("listed");
        let staged = download(&source, &release, &target, &|_| {}).expect("downloaded");
        staged.install().expect("installed");
        let running = if bundle { target.path.join("Contents/MacOS/atroxabot") } else { target.path.clone() };
        assert_eq!(fs::read(&running).expect("the app"), b"new");
        let name = target.path.file_name().expect("a name").to_string_lossy().into_owned();
        assert_eq!(names_in(&folder), [name.clone(), format!("{name}.old")], "the unpacking folder is gone");
        let _ = fs::remove_dir_all(&folder);
    }

    /// The release pipeline runs this on what it built, before publishing:
    /// a release the app would refuse is never put out.
    #[test]
    #[ignore = "needs a built release: ATROXABOT_RELEASE=<the folder of its files>"]
    fn a_built_release_is_one_the_app_takes() {
        let folder = PathBuf::from(std::env::var_os("ATROXABOT_RELEASE").expect("ATROXABOT_RELEASE names a folder"));
        let bytes = fs::read(folder.join(MANIFEST)).expect("the release has a manifest");
        let manifest = Manifest::read(&bytes, &Source::github().key).expect("signed with the key the app knows");
        assert_eq!(manifest.version, Version::current(), "the release is of the version in Cargo.toml");
        for name in ASSETS {
            let listed = manifest.release(name).unwrap_or_else(|| panic!("{name} is in the manifest"));
            let file = fs::read(folder.join(name)).unwrap_or_else(|e| panic!("{name} is in the release: {e}"));
            assert_eq!(listed.size, file.len() as u64, "{name}: size");
            assert_eq!(listed.sha256, digest::digest(&digest::SHA256, &file).as_ref(), "{name}: SHA-256");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_installed_entry_is_matched_by_its_folder() {
        let listing = "\r\nHKEY_CURRENT_USER\\Software\\…\\{6F0C3A52}_is1\r\n    InstallLocation    REG_SZ    C:\\Users\\Me\\AppData\\Local\\Programs\\AtroxaBot\\\r\n\r\n";
        assert!(installed::lists(listing, "InstallLocation", r"c:\users\me\appdata\local\programs\atroxabot"));
        assert!(!installed::lists(listing, "InstallLocation", r"C:\Users\Me\Downloads"));
        assert!(!installed::lists(listing, "DisplayVersion", r"C:\Users\Me\AppData\Local\Programs\AtroxaBot"));
        assert!(!installed::lists("", "InstallLocation", r"C:\Users\Me"));
    }
}
