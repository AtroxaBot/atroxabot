//! Finding the Claude Code binary and building its command line.

use std::env;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::{Error, Options, SystemPrompt};

/// How long `claude --version` may take; a cold start of the binary takes a second or two.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Finds the installed Claude Code binary: every `PATH` directory first, then the installers' usual locations.
///
/// On Windows only a native `claude.exe` counts (npm's `claude.cmd` shim is never returned).
#[must_use]
pub fn find_cli() -> Option<PathBuf> {
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    let mut candidates: Vec<PathBuf> =
        env::var_os("PATH").map(|path| env::split_paths(&path).map(|dir| dir.join(name)).collect()).unwrap_or_default();
    let home = env::home_dir();
    let under_home = |relative: &str| home.as_ref().map(|home| home.join(relative));
    if cfg!(windows) {
        candidates.extend(home.as_ref().map(|home| home.join(".local").join("bin").join(name)));
    } else {
        candidates.extend(
            [
                under_home(".npm-global/bin/claude"),
                Some(PathBuf::from("/usr/local/bin/claude")),
                under_home(".local/bin/claude"),
                under_home("node_modules/.bin/claude"),
                under_home(".yarn/bin/claude"),
                under_home(".claude/local/claude"),
            ]
            .into_iter()
            .flatten(),
        );
    }
    // Relative PATH entries are skipped: they would probe whatever directory the process happens to be in.
    candidates.into_iter().find(|path| path.is_absolute() && path.is_file())
}

/// Runs `claude --version` and returns the version number, e.g. `2.1.322`. Waits at most ten seconds.
///
/// # Errors
/// [`Error::BatchScript`] for a `.cmd`/`.bat` path on Windows, [`Error::Spawn`] when the binary cannot be started,
/// [`Error::Version`] when it does not answer in time or prints something else.
pub fn cli_version(path: &Path) -> Result<String, Error> {
    let mut child = command(path)?
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::Spawn)?;
    let stdout = child.stdout.take();
    let (sender, receiver) = mpsc::channel();
    // A thread, so that a binary that hangs (or a grandchild holding the pipe) cannot block the caller.
    let reader = thread::Builder::new().name("claude-agent-version".into()).spawn(move || {
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

/// The leading `major.minor.patch` of `claude --version` output such as `2.1.322 (Claude Code)`.
fn parse_version(output: &str) -> Option<String> {
    let token = output.trim().split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?;
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

/// Whether `value` holds characters `cmd.exe` interprets. Refused in values that often come from outside
/// (session ids), so they stay inert even if a `cmd.exe` hop ever sits between this process and the binary.
fn has_cmd_metacharacters(value: &str) -> bool {
    value.contains(['&', '|', '<', '>', '^', '%', '!', '"', '\r', '\n'])
}

/// The binary's arguments for `options`: always the stream-json protocol in both directions, plus one flag per
/// option that is set.
///
/// No `--print`: with stream-json formats and piped standard streams the binary runs non-interactively by itself.
pub(crate) fn build_args(options: &Options) -> Result<Vec<OsString>, Error> {
    fn add(args: &mut Vec<OsString>, items: &[&str]) {
        args.extend(items.iter().map(OsString::from));
    }
    let mut args = Vec::new();
    add(&mut args, &["--output-format", "stream-json", "--verbose"]);
    match &options.system_prompt {
        SystemPrompt::Preset => {}
        SystemPrompt::Append(text) => add(&mut args, &["--append-system-prompt", text]),
        SystemPrompt::Custom(text) => add(&mut args, &["--system-prompt", text]),
    }
    if !options.allowed_tools.is_empty() {
        add(&mut args, &["--allowedTools", &options.allowed_tools.join(",")]);
    }
    if let Some(turns) = options.max_turns.filter(|&turns| turns > 0) {
        add(&mut args, &["--max-turns", &turns.to_string()]);
    }
    if !options.disallowed_tools.is_empty() {
        add(&mut args, &["--disallowedTools", &options.disallowed_tools.join(",")]);
    }
    if let Some(model) = options.model.as_deref().filter(|model| !model.is_empty()) {
        add(&mut args, &["--model", model]);
    }
    if options.ask_permissions {
        add(&mut args, &["--permission-prompt-tool", "stdio"]);
    }
    if let Some(mode) = options.permission_mode {
        add(&mut args, &["--permission-mode", mode.as_str()]);
    }
    // `--flag=value` for values that may come from outside: `--resume` takes an optional value, so as two
    // arguments a value starting with a dash would be read as another flag.
    if let Some(resume) = options.resume.as_deref().filter(|resume| !resume.is_empty()) {
        if cfg!(windows) && has_cmd_metacharacters(resume) {
            return Err(Error::InvalidOption(format!("resume value {resume:?} is unsafe on a Windows command line")));
        }
        add(&mut args, &[&format!("--resume={resume}")]);
    }
    for dir in &options.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.into());
    }
    if let Some(config) = &options.mcp_config {
        add(&mut args, &["--mcp-config", config]);
    }
    if options.include_partial_messages {
        add(&mut args, &["--include-partial-messages"]);
    }
    if options.strict_mcp_config {
        add(&mut args, &["--strict-mcp-config"]);
    }
    if options.fork_session {
        add(&mut args, &["--fork-session"]);
    }
    if let Some(sources) = &options.setting_sources {
        add(&mut args, &[&format!("--setting-sources={}", sources.join(","))]);
    }
    for (flag, value) in &options.extra_args {
        if flag == "bare" {
            return Err(Error::InvalidOption("--bare is not supported: it disables subscription sign-in".into()));
        }
        match value {
            None => add(&mut args, &[&format!("--{flag}")]),
            Some(value) if value.starts_with('-') => add(&mut args, &[&format!("--{flag}={value}")]),
            Some(value) => add(&mut args, &[&format!("--{flag}"), value]),
        }
    }
    if let Some(effort) = &options.effort {
        add(&mut args, &["--effort", effort]);
    }
    add(&mut args, &["--input-format", "stream-json"]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PermissionMode;

    fn args(options: &Options) -> Vec<String> {
        build_args(options).unwrap().into_iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn default_options_are_the_bare_protocol() {
        let expected = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"];
        assert_eq!(args(&Options::default()), expected);
    }

    #[test]
    fn every_option_maps_to_its_flag() {
        let options = Options {
            cli_path: None,
            cwd: None,
            system_prompt: SystemPrompt::Append("be brief".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
            permission_mode: Some(PermissionMode::AcceptEdits),
            allowed_tools: vec!["Read".into(), "Bash(git *)".into()],
            disallowed_tools: vec!["WebFetch".into()],
            resume: Some("abc-123".into()),
            fork_session: true,
            max_turns: Some(3),
            add_dirs: vec![PathBuf::from("extra")],
            mcp_config: Some("{\"mcpServers\":{}}".into()),
            strict_mcp_config: true,
            setting_sources: Some(vec!["user".into(), "project".into()]),
            include_partial_messages: true,
            ask_permissions: true,
            env: Vec::new(),
            extra_args: vec![
                ("debug".into(), None),
                ("name".into(), Some("x".into())),
                ("n".into(), Some("-y".into())),
            ],
        };
        let expected = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--append-system-prompt",
            "be brief",
            "--allowedTools",
            "Read,Bash(git *)",
            "--max-turns",
            "3",
            "--disallowedTools",
            "WebFetch",
            "--model",
            "opus",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            "acceptEdits",
            "--resume=abc-123",
            "--add-dir",
            "extra",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--include-partial-messages",
            "--strict-mcp-config",
            "--fork-session",
            "--setting-sources=user,project",
            "--debug",
            "--name",
            "x",
            "--n=-y",
            "--effort",
            "high",
            "--input-format",
            "stream-json",
        ];
        assert_eq!(args(&options), expected);
    }

    #[test]
    fn custom_system_prompt_and_empty_values() {
        let options = Options {
            system_prompt: SystemPrompt::Custom("only this".into()),
            model: Some(String::new()),
            resume: Some(String::new()),
            max_turns: Some(0),
            setting_sources: Some(Vec::new()),
            ..Options::default()
        };
        let expected = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--system-prompt",
            "only this",
            "--setting-sources=",
            "--input-format",
            "stream-json",
        ];
        assert_eq!(args(&options), expected);
    }

    #[test]
    fn bare_is_refused() {
        let options = Options { extra_args: vec![("bare".into(), None)], ..Options::default() };
        assert!(matches!(build_args(&options), Err(Error::InvalidOption(_))));
    }

    #[test]
    fn permission_modes_round_trip() {
        use PermissionMode::{AcceptEdits, Auto, BypassPermissions, Default, DontAsk, Plan};
        for mode in [Default, AcceptEdits, Plan, BypassPermissions, DontAsk, Auto] {
            assert_eq!(PermissionMode::from_key(mode.as_str()), Some(mode));
        }
        assert_eq!(PermissionMode::from_key("manual"), Some(Default));
        assert_eq!(PermissionMode::from_key("Plan"), None);
    }

    #[test]
    fn cmd_metacharacters_are_detected() {
        for bad in ["a&calc", "a|b", "%PATH%", "a\"b", "a\nb", "!x!", "a^b", "a<b", "a>b"] {
            assert!(has_cmd_metacharacters(bad), "{bad}");
        }
        assert!(!has_cmd_metacharacters("0b5e3a52-1c0e-4b7e-9a57-6f1f0e9f5c11"));
        assert!(!has_cmd_metacharacters("my session (2)"));
    }

    #[cfg(windows)]
    #[test]
    fn unsafe_resume_and_batch_scripts_are_refused_on_windows() {
        let options = Options { resume: Some("x&calc".into()), ..Options::default() };
        assert!(matches!(build_args(&options), Err(Error::InvalidOption(_))));
        assert!(matches!(command(Path::new("C:\\npm\\claude.cmd")), Err(Error::BatchScript(_))));
        assert!(matches!(cli_version(Path::new("claude.bat")), Err(Error::BatchScript(_))));
        assert!(command(Path::new("C:\\bin\\claude.exe")).is_ok());
    }

    #[test]
    fn batch_scripts_are_recognised_however_they_are_spelled() {
        for script in [
            "claude.cmd",
            "C:\\x\\claude.CMD",
            "a/claude.bat",
            "claude.cmd. ",
            "claude.cmd:stream",
            "C:claude.bat",
            ".cmd",
        ] {
            assert!(is_batch_script(script), "{script}");
        }
        assert!(is_batch_script("claude.cmd\\...\\.."), "a batch component anywhere counts");
        for native in ["claude.exe", "C:\\Users\\cmd\\claude.exe", "/usr/local/bin/claude", "claude.cmdx"] {
            assert!(!is_batch_script(native), "{native}");
        }
    }

    #[test]
    fn versions_are_parsed_strictly() {
        assert_eq!(parse_version("2.1.322 (Claude Code)\n").as_deref(), Some("2.1.322"));
        assert_eq!(parse_version("10.0.1").as_deref(), Some("10.0.1"));
        assert_eq!(parse_version("2.1.322.7").as_deref(), Some("2.1.322"));
        for bad in ["", "claude", "2.1", "2..3", "v2.1.3", "Error: 2.1.3"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let missing = env::temp_dir().join("atroxabot-no-such-dir").join("claude.exe");
        assert!(matches!(cli_version(&missing), Err(Error::Spawn(_))));
    }
}
