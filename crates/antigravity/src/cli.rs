//! Finding the Antigravity binary and building its command line.

use std::env;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::{Error, McpTransport, Options};

/// How long `agy --version` may take: the binary unpacks itself before it answers.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// The folder under an account's own that the binary keeps its sessions and settings in. It is joined to the
/// account's folder by the binary, also when given as an absolute path.
const APP_DATA: &str = "antigravity-acp";

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
    fn a_missing_binary_is_a_spawn_error() {
        let missing = env::temp_dir().join("atroxabot-no-such-dir").join("agy.exe");
        assert!(matches!(cli_version(&missing), Err(Error::Spawn(_))));
    }
}
