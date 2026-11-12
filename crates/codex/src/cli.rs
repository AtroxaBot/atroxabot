//! Finding the Codex binary and building its command line.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::{Error, Options};

/// How long `codex --version` may take; a cold start of the binary takes a second or two.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// The platform packages npm installs the native binary in: operating system and architecture (as
/// `std::env::consts` spell them), the package's name under `@openai`, and the folder under its `vendor`.
/// The same table as in Codex's own `bin/codex.js`.
const TARGETS: [(&str, &str, &str, &str); 6] = [
    ("linux", "x86_64", "codex-linux-x64", "x86_64-unknown-linux-musl"),
    ("linux", "aarch64", "codex-linux-arm64", "aarch64-unknown-linux-musl"),
    ("macos", "x86_64", "codex-darwin-x64", "x86_64-apple-darwin"),
    ("macos", "aarch64", "codex-darwin-arm64", "aarch64-apple-darwin"),
    ("windows", "x86_64", "codex-win32-x64", "x86_64-pc-windows-msvc"),
    ("windows", "aarch64", "codex-win32-arm64", "aarch64-pc-windows-msvc"),
];

/// A platform package's name and the folder under its `vendor`.
type Target = (&'static str, &'static str);

/// Finds the installed Codex binary: every `PATH` directory first, then the installers' usual locations.
///
/// The native binary is returned wherever one exists. An npm install puts only a launcher on `PATH`
/// (`codex.cmd` on Windows, a link to `codex.js` elsewhere); the binary is then looked up in the platform
/// package beside it, the way the launcher does. On Windows nothing but a native `codex.exe` is ever returned;
/// elsewhere the launcher itself is, when its package holds no binary.
#[must_use]
pub fn find_cli() -> Option<PathBuf> {
    let path = env::var_os("PATH").map(|path| env::split_paths(&path).collect::<Vec<_>>()).unwrap_or_default();
    let targets = targets(env::consts::OS, env::consts::ARCH);
    // Relative PATH entries are skipped: they would probe whatever directory the process happens to be in.
    let mut dirs = path.into_iter().chain(usual_dirs()).filter(|dir| dir.is_absolute());
    dirs.find_map(|dir| locate(&dir, cfg!(windows), &targets))
}

/// Where installers put `codex` besides `PATH`, which a desktop app often gets only the system's part of.
fn usual_dirs() -> Vec<PathBuf> {
    let under = |base: Option<PathBuf>, relative: &str| {
        base.map(|mut path| {
            path.extend(relative.split('/'));
            path
        })
    };
    let home = |relative: &str| under(env::home_dir(), relative);
    let variable = |name: &str, relative: &str| under(env::var_os(name).map(PathBuf::from), relative);
    let dirs = if cfg!(windows) {
        vec![
            // OpenAI's own installer (`install.ps1`): a link to the version in use.
            variable("LOCALAPPDATA", "Programs/OpenAI/Codex/bin"),
            variable("APPDATA", "npm"),
            variable("LOCALAPPDATA", "Microsoft/WinGet/Links"),
            home(".local/bin"),
            home(".cargo/bin"),
            home(".bun/bin"),
        ]
    } else {
        vec![
            home(".local/bin"),
            Some(PathBuf::from("/usr/local/bin")),
            Some(PathBuf::from("/opt/homebrew/bin")),
            Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin")),
            home(".npm-global/bin"),
            home(".cargo/bin"),
            home(".bun/bin"),
        ]
    };
    dirs.into_iter().flatten().collect()
}

/// The platform packages whose binary runs here, the machine's own first. An ARM Mac or Windows also runs
/// the x64 build, which is what npm installs under an x64 Node.
fn targets(os: &str, arch: &str) -> Vec<Target> {
    let of = |arch: &str| {
        TARGETS.iter().find(|target| target.0 == os && target.1 == arch).map(|target| (target.2, target.3))
    };
    let emulated = if arch == "aarch64" && os != "linux" { of("x86_64") } else { None };
    of(arch).into_iter().chain(emulated).collect()
}

/// The Codex binary that `dir` holds or leads to, if any. `windows` says which layout to look for.
fn locate(dir: &Path, windows: bool, targets: &[Target]) -> Option<PathBuf> {
    if windows {
        let native = dir.join("codex.exe");
        if native.is_file() {
            return Some(native);
        }
        // npm leaves launcher scripts only (never run: `cmd.exe` re-parses arguments). They start the
        // package beside them: under the npm prefix for a global install, next to `.bin` for a local one.
        if !dir.join("codex.cmd").is_file() && !dir.join("codex").is_file() {
            return None;
        }
        let global = dir.join("node_modules").join("@openai").join("codex");
        let local = dir.parent().filter(|_| dir.ends_with(".bin")).map(|modules| modules.join("@openai").join("codex"));
        return [Some(global), local].into_iter().flatten().find_map(|package| vendored(&package, true, targets));
    }
    let path = dir.join("codex");
    if !path.is_file() {
        return None;
    }
    // npm's `codex` is a link to the package's `bin/codex.js`, which needs `node` on PATH; the binary it
    // starts does not.
    let script =
        fs::canonicalize(&path).ok().filter(|real| real.extension().is_some_and(|extension| extension == "js"));
    let package = script.as_deref().and_then(Path::parent).and_then(Path::parent);
    package.and_then(|package| vendored(package, false, targets)).or(Some(path))
}

/// The native binary an npm install of the package at `package` (`…/node_modules/@openai/codex`) comes with:
/// in a platform package nested in it or hoisted beside it, or (older releases) in the package itself.
fn vendored(package: &Path, windows: bool, targets: &[Target]) -> Option<PathBuf> {
    let name = if windows { "codex.exe" } else { "codex" };
    targets.iter().find_map(|(platform, triple)| {
        let nested = package.join("node_modules").join("@openai").join(platform);
        let hoisted = package.parent().map(|scope| scope.join(platform));
        let roots = [Some(nested), hoisted, Some(package.to_owned())];
        roots
            .into_iter()
            .flatten()
            .map(|root| root.join("vendor").join(triple).join("bin").join(name))
            .find(|path| path.is_file())
    })
}

/// Runs `codex --version` and returns the version number, e.g. `0.167.1`. Waits at most ten seconds.
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
    let reader = thread::Builder::new().name("codex-version".into()).spawn(move || {
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
    let output = output.map_err(|_| Error::Version("no answer within ten seconds".into()))?;
    parse_version(&output).ok_or_else(|| Error::Version(format!("unexpected output {:?}", preview(&output))))
}

/// The `major.minor.patch` of `codex --version` output such as `codex-cli 0.167.1`: the first word that
/// starts with a digit.
fn parse_version(output: &str) -> Option<String> {
    let word = output.split_whitespace().find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
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

/// The binary's arguments for `options`: the `app-server` subcommand (which talks over its standard streams
/// unless told otherwise), then one `-c key=value` per configuration override.
pub(crate) fn build_args(options: &Options) -> Result<Vec<OsString>, Error> {
    let mut args = vec![OsString::from("app-server")];
    for (key, value) in &options.config {
        // A key is a dotted path. Anything looking like a flag would be read as one.
        if key.is_empty() || key.starts_with('-') {
            return Err(Error::InvalidOption(format!("configuration key {key:?} is not a key")));
        }
        args.push("-c".into());
        args.push(format!("{key}={value}").into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    const WINDOWS_X64: [Target; 1] = [("codex-win32-x64", "x86_64-pc-windows-msvc")];
    const LINUX_X64: [Target; 1] = [("codex-linux-x64", "x86_64-unknown-linux-musl")];

    /// A fresh folder under the system's temporary one, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let name = format!("atroxabot-codex-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
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

        fn dir(&self, relative: &str) -> PathBuf {
            let mut path = self.0.clone();
            path.extend(relative.split('/'));
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn args(options: &Options) -> Vec<String> {
        build_args(options).unwrap().into_iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn default_options_are_the_bare_subcommand() {
        assert_eq!(args(&Options::default()), ["app-server"]);
    }

    #[test]
    fn configuration_becomes_c_arguments() {
        let config =
            vec![("model_provider".to_owned(), "\"oss\"".to_owned()), ("features.x".to_owned(), "true".to_owned())];
        let options = Options { config, ..Options::default() };
        assert_eq!(args(&options), ["app-server", "-c", "model_provider=\"oss\"", "-c", "features.x=true"]);
    }

    #[test]
    fn keys_that_are_no_keys_are_refused() {
        for key in ["", "-c", "--listen"] {
            let options = Options { config: vec![(key.to_owned(), "1".to_owned())], ..Options::default() };
            assert!(matches!(build_args(&options), Err(Error::InvalidOption(_))), "{key:?}");
        }
    }

    #[test]
    fn targets_put_the_machines_own_first() {
        assert_eq!(targets("windows", "x86_64"), WINDOWS_X64);
        assert_eq!(targets("linux", "x86_64"), LINUX_X64);
        assert_eq!(targets("linux", "aarch64"), [("codex-linux-arm64", "aarch64-unknown-linux-musl")]);
        assert_eq!(
            targets("macos", "aarch64"),
            [("codex-darwin-arm64", "aarch64-apple-darwin"), ("codex-darwin-x64", "x86_64-apple-darwin")]
        );
        assert_eq!(
            targets("windows", "aarch64"),
            [("codex-win32-arm64", "aarch64-pc-windows-msvc"), ("codex-win32-x64", "x86_64-pc-windows-msvc")]
        );
        assert_eq!(targets("macos", "x86_64"), [("codex-darwin-x64", "x86_64-apple-darwin")]);
        assert!(targets("freebsd", "x86_64").is_empty());
        assert!(targets("linux", "riscv64").is_empty());
    }

    #[test]
    fn a_native_binary_on_windows_is_taken_as_is() {
        let scratch = Scratch::new();
        let native = scratch.file("bin/codex.exe");
        scratch.file("bin/codex.cmd");
        assert_eq!(locate(&scratch.dir("bin"), true, &WINDOWS_X64), Some(native));
        assert_eq!(locate(&scratch.dir("empty"), true, &WINDOWS_X64), None);
    }

    #[test]
    fn a_global_npm_install_on_windows_leads_to_the_nested_platform_package() {
        let scratch = Scratch::new();
        scratch.file("npm/codex.cmd");
        scratch.file("npm/node_modules/@openai/codex/bin/codex.js");
        // The shim alone is never an answer.
        assert_eq!(locate(&scratch.dir("npm"), true, &WINDOWS_X64), None);
        let package = "npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64";
        let native = scratch.file(&format!("{package}/vendor/x86_64-pc-windows-msvc/bin/codex.exe"));
        assert_eq!(locate(&scratch.dir("npm"), true, &WINDOWS_X64), Some(native));
    }

    #[test]
    fn a_hoisted_platform_package_is_found_too() {
        let scratch = Scratch::new();
        scratch.file("npm/codex.cmd");
        let native =
            scratch.file("npm/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        assert_eq!(locate(&scratch.dir("npm"), true, &WINDOWS_X64), Some(native));
    }

    #[test]
    fn a_local_npm_install_on_windows_is_found_from_its_bin_folder() {
        let scratch = Scratch::new();
        scratch.file("project/node_modules/.bin/codex.cmd");
        let native =
            scratch.file("project/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        assert_eq!(locate(&scratch.dir("project/node_modules/.bin"), true, &WINDOWS_X64), Some(native));
    }

    #[test]
    fn the_other_architecture_is_a_fallback_only() {
        let scratch = Scratch::new();
        scratch.file("npm/codex.cmd");
        let both = targets("windows", "aarch64");
        let x64 = scratch.file("npm/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        assert_eq!(locate(&scratch.dir("npm"), true, &both), Some(x64));
        let arm =
            scratch.file("npm/node_modules/@openai/codex-win32-arm64/vendor/aarch64-pc-windows-msvc/bin/codex.exe");
        assert_eq!(locate(&scratch.dir("npm"), true, &both), Some(arm));
        // A package for another machine is not for this one.
        assert_eq!(locate(&scratch.dir("npm"), true, &[("codex-win32-ia32", "i686-pc-windows-msvc")]), None);
    }

    #[test]
    fn a_plain_binary_on_unix_is_taken_as_is() {
        let scratch = Scratch::new();
        let native = scratch.file("bin/codex");
        assert_eq!(locate(&scratch.dir("bin"), false, &LINUX_X64), Some(native));
        assert_eq!(locate(&scratch.dir("empty"), false, &LINUX_X64), None);
        // A folder of that name is not a binary.
        fs::create_dir_all(scratch.dir("odd/codex")).unwrap();
        assert_eq!(locate(&scratch.dir("odd"), false, &LINUX_X64), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_npm_link_on_unix_leads_to_the_platform_package() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        let script = scratch.file("lib/node_modules/@openai/codex/bin/codex.js");
        fs::create_dir_all(scratch.dir("bin")).unwrap();
        let link = scratch.dir("bin/codex");
        symlink(&script, &link).unwrap();
        // Without the platform package the launcher itself is all there is.
        assert_eq!(locate(&scratch.dir("bin"), false, &LINUX_X64), Some(link));
        let package = "lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64";
        let native = scratch.file(&format!("{package}/vendor/x86_64-unknown-linux-musl/bin/codex"));
        let found = locate(&scratch.dir("bin"), false, &LINUX_X64).unwrap();
        assert_eq!(fs::canonicalize(found).unwrap(), fs::canonicalize(native).unwrap());
    }

    #[test]
    fn the_vendored_binary_of_an_old_release_sits_in_the_package_itself() {
        let scratch = Scratch::new();
        let native = scratch.file("node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl/bin/codex");
        assert_eq!(vendored(&scratch.dir("node_modules/@openai/codex"), false, &LINUX_X64), Some(native));
        assert_eq!(vendored(&scratch.dir("node_modules/@openai/codex"), true, &WINDOWS_X64), None);
    }

    #[cfg(windows)]
    #[test]
    fn batch_scripts_are_refused_on_windows() {
        assert!(matches!(command(Path::new("C:\\npm\\codex.cmd")), Err(Error::BatchScript(_))));
        assert!(matches!(cli_version(Path::new("codex.bat")), Err(Error::BatchScript(_))));
        assert!(command(Path::new("C:\\bin\\codex.exe")).is_ok());
    }

    #[test]
    fn batch_scripts_are_recognised_however_they_are_spelled() {
        for script in
            ["codex.cmd", "C:\\x\\codex.CMD", "a/codex.bat", "codex.cmd. ", "codex.cmd:stream", "C:codex.bat", ".cmd"]
        {
            assert!(is_batch_script(script), "{script}");
        }
        assert!(is_batch_script("codex.cmd\\...\\.."), "a batch component anywhere counts");
        for native in ["codex.exe", "C:\\Users\\cmd\\codex.exe", "/usr/local/bin/codex", "codex.cmdx"] {
            assert!(!is_batch_script(native), "{native}");
        }
    }

    #[test]
    fn versions_are_parsed_strictly() {
        assert_eq!(parse_version("codex-cli 0.167.1\n").as_deref(), Some("0.167.1"));
        assert_eq!(parse_version("codex-cli 0.168.0-alpha.3").as_deref(), Some("0.168.0"));
        assert_eq!(parse_version("10.0.1").as_deref(), Some("10.0.1"));
        for bad in ["", "codex-cli", "codex-cli 0.167", "codex 1..3", "v2.1.3"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let missing = env::temp_dir().join("atroxabot-no-such-dir").join("codex.exe");
        assert!(matches!(cli_version(&missing), Err(Error::Spawn(_))));
    }
}
