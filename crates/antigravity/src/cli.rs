//! Finding the Antigravity binary and building its command line.

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use crate::{Error, McpTransport, Options};

/// How long `agy --version` may take: the binary unpacks itself before it answers.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// The folder under an account's own that the binary keeps its sessions and settings in. It is joined to the
/// account's folder by the binary, also when given as an absolute path.
const APP_DATA: &str = "antigravity-acp";

/// The folder the binary keeps the same in by itself, in the user's own `~/.gemini`.
const OWN_APP_DATA: &str = "antigravity-cli";

/// Removes what the binary kept of the conversation `id` in an account's folder (`profile`; `None` is the
/// user's own `~/.gemini`): one this crate opened only to ask what the account offers, in which nothing was
/// said and which would otherwise sit in the account's history. Call it once the process has ended. Only what
/// carries the id in its name is removed, and an id that is not of the binary's making (a UUID) removes
/// nothing: nothing else in the folder is read or touched.
pub(crate) fn forget_conversation(profile: Option<&Path>, id: &str) {
    let made_here = id.len() == 36 && id.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    let own = || {
        let named = env::var_os("GEMINI_HOME").filter(|home| !home.is_empty()).map(PathBuf::from);
        named.or_else(|| env::home_dir().map(|home| home.join(".gemini")))
    };
    let (true, Some(home)) = (made_here, profile.map(Path::to_path_buf).or_else(own)) else { return };
    // Another account's folder is started with the name given here; the user's own goes by either.
    let kept_in: &[&str] = if profile.is_some() { &[APP_DATA] } else { &[APP_DATA, OWN_APP_DATA] };
    for data in kept_in.iter().map(|data| home.join(data)) {
        for suffix in [".db", ".db-wal", ".db-shm", ".db-journal", ".meta"] {
            let _ = fs::remove_file(data.join("conversations").join(format!("{id}{suffix}")));
        }
        let _ = fs::remove_dir_all(data.join("brain").join(id));
        let _ = fs::remove_file(data.join("annotations").join(format!("{id}.pbtxt")));
    }
}

/// What a process's own temporary folder is called, before its number.
const RUN_PREFIX: &str = "run-";
/// How often a folder a process has left is tried to be removed, and how long between two tries: on Windows
/// what the binary started may hold its files open a while after it is gone.
const RECLAIM_TRIES: u32 = 30;
const RECLAIM_EVERY: Duration = Duration::from_secs(2);

/// The temporary folders of the processes that run now: never swept away.
static RUNNING: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// The temporary folder of one process of the binary, under the caller's `root`. The binary unpacks itself
/// into its temporary folder at every start and cannot take that away again when it is killed.
pub(crate) fn run_dir(root: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(root)?;
    let name = format!("{RUN_PREFIX}{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let dir = root.join(name);
    // One of that name is what an earlier run with this process id left.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir)?;
    let first = RUNNING.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_default().insert(dir.clone());
    debug_assert!(first, "every process has a folder of its own");
    sweep(root);
    Ok(dir)
}

/// Removes what earlier runs of the caller left under `root`, off this thread: a gigabyte takes a while.
fn sweep(root: &Path) {
    let root = root.to_owned();
    // Without the thread the folders stay until the next start, no worse.
    let _ = thread::Builder::new().name("agy-sweep".into()).spawn(move || {
        let Ok(entries) = fs::read_dir(&root) else { return };
        for entry in entries.flatten() {
            let (dir, name) = (entry.path(), entry.file_name());
            let ours = name.to_string_lossy().starts_with(RUN_PREFIX) && dir.is_dir();
            let running =
                RUNNING.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|set| set.contains(&dir));
            if ours && !running {
                let _ = fs::remove_dir_all(&dir);
            }
        }
    });
}

/// Removes the temporary folder `dir` of a process that has ended, trying again for a while off this thread.
pub(crate) fn reclaim(dir: &Path) {
    let forget = |dir: &Path| {
        if let Some(running) = RUNNING.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            running.remove(dir);
        }
    };
    if fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return forget(dir);
    }
    let again = dir.to_owned();
    let spawned = thread::Builder::new().name("agy-reclaim".into()).spawn(move || {
        for _ in 0..RECLAIM_TRIES {
            thread::sleep(RECLAIM_EVERY);
            if fs::remove_dir_all(&again).is_ok() || !again.exists() {
                break;
            }
        }
        // What is left now goes with the next sweep.
        forget(&again);
    });
    if spawned.is_err() {
        forget(dir);
    }
}

/// The environment that makes `dir` a process's temporary folder.
pub(crate) fn temp_env(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    let names: &[&str] = if cfg!(windows) { &["TEMP", "TMP"] } else { &["TMPDIR"] };
    names.iter().map(|name| (*name, dir.to_owned())).collect()
}

/// Finds the installed Antigravity binary: every `PATH` directory first, then where Google's installer puts
/// it. On Windows nothing but a native `agy.exe` is returned.
#[must_use]
pub fn find_cli() -> Option<PathBuf> {
    let path = env::var_os("PATH").map(|path| env::split_paths(&path).collect::<Vec<_>>()).unwrap_or_default();
    // Relative PATH entries are skipped: they would probe whatever directory the process happens to be in.
    let mut dirs = path.into_iter().chain(usual_dirs()).filter(|dir| dir.is_absolute());
    dirs.find_map(|dir| locate(&dir, cfg!(windows)))
}

/// Where the installer puts `agy` besides `PATH`, which a desktop app often gets only the system's part of.
fn usual_dirs() -> Vec<PathBuf> {
    let under = |base: Option<PathBuf>, relative: &str| {
        base.map(|mut path| {
            path.extend(relative.split('/'));
            path
        })
    };
    let home = |relative: &str| under(env::home_dir(), relative);
    let dirs = if cfg!(windows) {
        vec![under(env::var_os("LOCALAPPDATA").map(PathBuf::from), "agy/bin"), home(".local/bin")]
    } else {
        vec![home(".local/bin"), Some(PathBuf::from("/usr/local/bin")), Some(PathBuf::from("/opt/homebrew/bin"))]
    };
    dirs.into_iter().flatten().collect()
}

/// The Antigravity binary that `dir` holds, if any. `windows` says which name to look for.
fn locate(dir: &Path, windows: bool) -> Option<PathBuf> {
    let path = dir.join(if windows { "agy.exe" } else { "agy" });
    path.is_file().then_some(path)
}

/// Runs `agy --version` and returns the version number, e.g. `1.5.12`. Waits at most half a minute.
///
/// # Errors
/// [`Error::BatchScript`] for a `.cmd`/`.bat` path on Windows, [`Error::Spawn`] when the binary cannot be started,
/// [`Error::Version`] when it does not answer in time or prints something else.
pub fn cli_version(path: &Path) -> Result<String, Error> {
    let mut child = command(path)?
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::Spawn)?;
    let stdout = child.stdout.take();
    let (sender, receiver) = mpsc::channel();
    // A thread, so that a binary that hangs (or a grandchild holding the pipe) cannot block the caller.
    let reader = thread::Builder::new().name("agy-version".into()).spawn(move || {
        let mut output = String::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(4096).read_to_string(&mut output);
        }
        let _ = sender.send(output);
    });
    let output = receiver.recv_timeout(VERSION_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();
    reader.map_err(Error::Io)?;
    let output = output.map_err(|_| Error::Version("no answer within thirty seconds".into()))?;
    parse_version(&output).ok_or_else(|| Error::Version(format!("unexpected output {:?}", preview(&output))))
}

/// The `major.minor.patch` in `text`, such as `agy 1.5.12` or the `1.5.12-rc.1` a handshake reports: the first
/// word that starts with a digit.
pub(crate) fn parse_version(text: &str) -> Option<String> {
    let word = text.split_whitespace().find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let token = word.split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?;
    let mut parts = token.split('.');
    let (major, minor, patch) = (parts.next()?, parts.next()?, parts.next()?);
    (!major.is_empty() && !minor.is_empty() && !patch.is_empty()).then(|| format!("{major}.{minor}.{patch}"))
}

/// The first 200 characters of `text`, for error messages.
pub(crate) fn preview(text: &str) -> String {
    text.chars().take(200).collect()
}

/// A `Command` for the binary at `path`, run directly and without a console window.
pub(crate) fn command(path: &Path) -> Result<Command, Error> {
    if cfg!(windows) && is_batch_script(&path.to_string_lossy()) {
        return Err(Error::BatchScript(path.to_owned()));
    }
    let mut command = Command::new(path);
    hide_console(&mut command);
    Ok(command)
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`: a GUI parent must not flash a console.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

/// Whether any component of `path` names a `.bat`/`.cmd` script, as Windows would resolve it (trailing dots and
/// spaces stripped, drive prefixes and stream names split off). Windows runs those through `cmd.exe`, which
/// re-parses the command line, so arguments could inject commands.
fn is_batch_script(path: &str) -> bool {
    path.split(['/', '\\']).flat_map(|component| component.split(':')).any(|segment| {
        // Not `Path::extension`: Windows counts a bare `.cmd` as a batch script, `Path` sees no extension in it.
        let extension = segment.trim_end_matches(['.', ' ']).rsplit_once('.').map(|(_, extension)| extension);
        extension.is_some_and(|extension| ["bat", "cmd"].iter().any(|batch| extension.eq_ignore_ascii_case(batch)))
    })
}

/// The arguments that select another account's folder: where the binary keeps that account's sign-in, settings
/// and conversations, instead of the user's own `~/.gemini`.
fn profile_args(profile: Option<&Path>) -> Vec<OsString> {
    let Some(profile) = profile else { return Vec::new() };
    vec!["--gemini-dir".into(), profile.into(), "--app-data-dir".into(), APP_DATA.into()]
}

/// The environment that goes with another account's folder (`profile`), on top of the inherited one.
///
/// The binary keeps the Google sign-in of every folder in one entry of the system's keyring, so that two
/// accounts would be one. Only when it takes itself to be reached over SSH does it keep the sign-in in the
/// account's own folder, and sign in by a code pasted into the terminal; `SSH_CONNECTION` says so (the user's
/// own, where there is one). The sign-in itself stays the binary's: nothing here reads or writes it.
#[must_use]
pub fn profile_env(profile: &Path) -> Vec<(String, String)> {
    let ssh = env::var("SSH_CONNECTION").ok().filter(|value| !value.is_empty());
    vec![
        ("GEMINI_HOME".to_owned(), profile.to_string_lossy().into_owned()),
        // What older releases went by for the same.
        ("AGY_ACP_FORCE_FILE_STORAGE".to_owned(), "1".to_owned()),
        ("SSH_CONNECTION".to_owned(), ssh.unwrap_or_else(|| "127.0.0.1 0 127.0.0.1 0".to_owned())),
    ]
}

/// The binary's arguments for signing in: `agy acp --login` prints a link to Google, reads the code its page
/// shows, keeps the sign-in and exits. It is the caller's to run in a terminal the user sees, with
/// [`profile_env`] for another account's folder.
#[must_use]
pub fn login_args(profile: Option<&Path>) -> Vec<OsString> {
    let mut args = vec![OsString::from("acp")];
    args.extend(profile_args(profile));
    args.push("--login".into());
    args
}

/// The binary's arguments for `options`: the `acp` subcommand, which talks over its standard streams, and the
/// account's folder.
pub(crate) fn build_args(options: &Options) -> Vec<OsString> {
    let mut args = vec![OsString::from("acp")];
    args.extend(profile_args(options.profile.as_deref()));
    // The binary serves an MCP server over HTTP only from a worker of the session's own.
    if options.mcp_servers.iter().any(|server| matches!(server.transport, McpTransport::Http { .. })) {
        args.push("--process-per-session".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::McpServer;

    /// A fresh folder under the system's temporary one, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let name = format!("atroxabot-agy-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            let path = env::temp_dir().join(name);
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// Creates the file `relative` (and its folders) and returns its path.
        fn file(&self, relative: &str) -> PathBuf {
            let mut path = self.0.clone();
            path.extend(relative.split('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "x").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_process_has_a_temporary_folder_that_goes_with_it() {
        let scratch = Scratch::new();
        let root = scratch.0.join("tmp");
        // What an earlier run left, and something that is not a process's folder.
        scratch.file("tmp/run-1-0/_MEI1/python.dll");
        scratch.file("tmp/keep/file");
        let dir = run_dir(&root).unwrap();
        assert!(dir.is_dir() && dir.starts_with(&root));
        let other = run_dir(&root).unwrap();
        assert_ne!(dir, other);
        fs::write(dir.join("unpacked"), "x").unwrap();
        // The sweep runs off this thread: it takes the leftover, and nothing of a process that runs.
        for _ in 0..200 {
            if !root.join("run-1-0").exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("run-1-0").exists(), "what an earlier run left is removed");
        assert!(dir.join("unpacked").exists() && other.exists() && root.join("keep").join("file").exists());
        reclaim(&dir);
        reclaim(&other);
        assert!(!dir.exists() && !other.exists());
        let names: Vec<&str> = temp_env(&dir).iter().map(|(name, _)| *name).collect();
        assert_eq!(names, if cfg!(windows) { vec!["TEMP", "TMP"] } else { vec!["TMPDIR"] });
    }

    #[test]
    fn default_options_are_the_bare_subcommand() {
        assert_eq!(strings(&build_args(&Options::default())), ["acp"]);
    }

    #[test]
    fn another_account_is_a_folder_of_its_own() {
        let profile = PathBuf::from("/accounts/two");
        let options = Options { profile: Some(profile.clone()), ..Options::default() };
        assert_eq!(
            strings(&build_args(&options)),
            ["acp", "--gemini-dir", "/accounts/two", "--app-data-dir", "antigravity-acp"]
        );
        assert_eq!(
            strings(&login_args(Some(&profile))),
            ["acp", "--gemini-dir", "/accounts/two", "--app-data-dir", "antigravity-acp", "--login"]
        );
        assert_eq!(strings(&login_args(None)), ["acp", "--login"]);
        let env = profile_env(&profile);
        assert!(env.contains(&("GEMINI_HOME".to_owned(), "/accounts/two".to_owned())));
        assert!(env.iter().any(|(key, value)| key == "SSH_CONNECTION" && !value.is_empty()));
    }

    #[test]
    fn an_http_mcp_server_asks_for_a_worker_per_session() {
        let http = McpTransport::Http { url: "http://127.0.0.1:1/mcp".into(), headers: Vec::new() };
        let stdio = McpTransport::Stdio { command: "server".into(), args: Vec::new(), env: Vec::new() };
        let with = |transport| Options {
            mcp_servers: vec![McpServer { name: "tools".into(), transport }],
            ..Options::default()
        };
        assert_eq!(strings(&build_args(&with(http))), ["acp", "--process-per-session"]);
        assert_eq!(strings(&build_args(&with(stdio))), ["acp"]);
    }

    #[test]
    fn the_binary_is_found_by_the_platforms_name_only() {
        let scratch = Scratch::new();
        let windows = scratch.file("win/agy.exe");
        scratch.file("win/agy.cmd");
        assert_eq!(locate(&scratch.0.join("win"), true), Some(windows));
        let unix = scratch.file("unix/agy");
        assert_eq!(locate(&scratch.0.join("unix"), false), Some(unix));
        // A launcher script alone is never an answer on Windows, and a folder of that name is no binary.
        scratch.file("shim/agy.cmd");
        assert_eq!(locate(&scratch.0.join("shim"), true), None);
        fs::create_dir_all(scratch.0.join("odd").join("agy")).unwrap();
        assert_eq!(locate(&scratch.0.join("odd"), false), None);
        assert_eq!(locate(&scratch.0.join("empty"), false), None);
    }

    #[cfg(windows)]
    #[test]
    fn batch_scripts_are_refused_on_windows() {
        assert!(matches!(command(Path::new("C:\\bin\\agy.cmd")), Err(Error::BatchScript(_))));
        assert!(matches!(cli_version(Path::new("agy.bat")), Err(Error::BatchScript(_))));
        assert!(command(Path::new("C:\\bin\\agy.exe")).is_ok());
    }

    #[test]
    fn batch_scripts_are_recognised_however_they_are_spelled() {
        for script in ["agy.cmd", "C:\\x\\agy.CMD", "a/agy.bat", "agy.cmd. ", "agy.cmd:stream", "C:agy.bat", ".cmd"] {
            assert!(is_batch_script(script), "{script}");
        }
        for native in ["agy.exe", "C:\\Users\\cmd\\agy.exe", "/usr/local/bin/agy", "agy.cmdx"] {
            assert!(!is_batch_script(native), "{native}");
        }
    }

    #[test]
    fn versions_are_parsed_strictly() {
        assert_eq!(parse_version("agy 1.5.12\n").as_deref(), Some("1.5.12"));
        assert_eq!(parse_version("1.6.0-rc.2").as_deref(), Some("1.6.0"));
        assert_eq!(parse_version("Antigravity CLI version 10.0.1 (abc)").as_deref(), Some("10.0.1"));
        for bad in ["", "agy", "agy 1.5", "agy 1..3", "v2.1.3", "unknown"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_the_conversation_asked_about_is_forgotten() {
        let (asked, other) = ("0b6f1c2e-5a3d-4c8e-9f10-2b7a6d4e1c90", "9a1b2c3d-4e5f-4a6b-8c7d-0e1f2a3b4c5d");
        let scratch = Scratch::new();
        let of = |id: &str| {
            let files = [".db", ".db-wal", ".meta"].map(|suffix| format!("antigravity-acp/conversations/{id}{suffix}"));
            let mut kept: Vec<PathBuf> = files.iter().map(|file| scratch.file(file)).collect();
            kept.push(scratch.file(&format!("antigravity-acp/brain/{id}/plan.md")));
            kept.push(scratch.file(&format!("antigravity-acp/annotations/{id}.pbtxt")));
            kept
        };
        let (gone, stays) = (of(asked), of(other));
        let sign_in = scratch.file("antigravity-acp/acp_token.json");
        // A name that could lead elsewhere removes nothing.
        forget_conversation(Some(&scratch.0), "../../antigravity-acp/acp_token.json/..");
        forget_conversation(Some(&scratch.0), "");
        assert!(gone.iter().chain(&stays).all(|file| file.exists()));
        forget_conversation(Some(&scratch.0), asked);
        assert!(gone.iter().all(|file| !file.exists()), "what the check left is gone");
        assert!(stays.iter().all(|file| file.exists()) && sign_in.exists(), "and nothing else");
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let missing = env::temp_dir().join("atroxabot-no-such-dir").join("agy.exe");
        assert!(matches!(cli_version(&missing), Err(Error::Spawn(_))));
    }
}
