//! The app's log and crash report, both in `~/.atroxabot/`.
//!
//! Release builds on Windows have no console and apps started from a file
//! manager lose stderr, so problems are also appended to `desktop.log`
//! (a previous one over [`MAX_LOG`] is kept as `desktop.log.old`). Lines are
//! written by a thread of their own, never the UI thread; they never hold
//! the token, which nothing logs.
//!
//! A panic aborts the process (`panic = "abort"` in release), so the panic
//! hook writes `desktop-crash.log` itself, at once, and tells the user where
//! it is before the process goes.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

use atroxabot_core::{Config, unix_now};

use crate::platform;

/// The log's file name in `~/.atroxabot/`.
const LOG_FILE: &str = "desktop.log";
/// The crash report's file name in `~/.atroxabot/`.
const CRASH_FILE: &str = "desktop-crash.log";
/// Size past which the log starts over (the previous one is kept).
const MAX_LOG: u64 = 1 << 20;

/// Where log lines go: the logging thread, once started.
static LINES: OnceLock<Sender<String>> = OnceLock::new();

/// Starts the logging thread and installs the panic hook. Call it first.
pub fn init() {
    let Ok(dir) = Config::dir() else { return };
    let (lines, inbox) = mpsc::channel::<String>();
    let log = dir.join(LOG_FILE);
    let spawned = std::thread::Builder::new().name("atroxabot-log".into()).spawn(move || {
        if private_dir(&dir).is_err() {
            return;
        }
        let old = fs::metadata(&log).is_ok_and(|m| m.len() > MAX_LOG);
        if old {
            let _ = fs::rename(&log, dir.join(format!("{LOG_FILE}.old")));
        }
        for line in inbox {
            let _ = append(&log, &line);
        }
    });
    if spawned.is_ok() {
        let _ = LINES.set(lines);
    }
    install_panic_hook();
}

/// Records a problem: on stderr, and in the log file.
pub fn error(message: impl Into<String>) {
    let message = message.into();
    eprintln!("atroxabot: {message}");
    if let Some(lines) = LINES.get() {
        let _ = lines.send(format!("{} {message}", unix_now()));
    }
}

/// Creates `dir` if needed, readable only by the user on Unix (it holds the
/// config, with its token).
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Appends `line` to the file at `path`, creating it if needed.
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    writeln!(options.open(path)?, "{line}")
}

/// Writes a crash report for every panic (any thread), then tells the user.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let mut report = format!("AtroxaBot {} crashed at {} (unix time)\n", env!("CARGO_PKG_VERSION"), unix_now());
        let thread = std::thread::current();
        let _ = writeln!(report, "thread: {}", thread.name().unwrap_or("unnamed"));
        let _ = writeln!(report, "{info}");
        let _ = writeln!(report, "\n{}\n", std::backtrace::Backtrace::force_capture());
        let path = Config::dir().map(|dir| dir.join(CRASH_FILE));
        let saved = path.as_ref().is_ok_and(|path| {
            path.parent().is_some_and(|dir| private_dir(dir).is_ok()) && append(path, &report).is_ok()
        });
        let detail = match (&path, saved) {
            (Ok(path), true) => format!("Details were saved to {}.", path.display()),
            _ => "No crash report could be saved.".to_owned(),
        };
        platform::alert(
            "AtroxaBot crashed",
            &format!("AtroxaBot ran into a problem it could not recover from and has to close. {detail}"),
        );
    }));
}
