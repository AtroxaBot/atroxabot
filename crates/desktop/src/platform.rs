//! Operating-system integration through the tools each platform ships with:
//! opening URLs and folders and the native file pickers (open and save).
//! No shell is involved: every argument is passed to the program directly,
//! and text that is not ours never becomes part of a script.
//!
//! The pickers block until the user answers, so call them on a worker thread.
//!
//! ponytail: the picker runs a helper process (PowerShell / `osascript` /
//! `zenity`/`kdialog`) instead of linking the platform dialog APIs; it takes
//! a moment to appear. Swap in direct Win32/Cocoa/portal calls if that grates.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Configures `command` to open no console window (Windows only; a GUI app
/// spawning console programs would otherwise flash one).
pub fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Starts `command` detached from our stdio and reaps it in the background.
fn launch(mut command: Command) -> io::Result<()> {
    let mut child = no_window(&mut command).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Opens `url` in the default browser.
///
/// # Errors
/// The launcher could not be started.
pub fn open_url(url: &str) -> io::Result<()> {
    let command = if cfg!(target_os = "windows") {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    launch(command)
}

/// Shows `path` in the file manager.
///
/// # Errors
/// The file manager could not be started.
pub fn open_folder(path: &Path) -> io::Result<()> {
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    command.arg(path);
    launch(command)
}

/// Shows a native error dialog with `title` and `message` and waits until
/// the user closes it. For failures the window cannot show (it failed to
/// start, or the app is crashing); safe from any thread. Does nothing where
/// no dialog helper is installed.
pub fn alert(title: &str, message: &str) {
    #[cfg(target_os = "windows")]
    {
        // `MessageBoxW` from user32, which every Windows GUI process loads.
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(owner: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        /// `MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST`.
        const KIND: u32 = 0x10 | 0x1_0000 | 0x4_0000;
        let (text, caption) = (wide(message), wide(title));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the
        // call, and a null owner is allowed.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), KIND);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // The text goes in as arguments, never into the script itself.
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            "on run argv",
            "-e",
            "display alert (item 1 of argv) message (item 2 of argv) as critical",
            "-e",
            "end run",
            title,
            message,
        ]);
        let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut zenity = Command::new("zenity");
        zenity.args(["--error", "--no-markup", "--title", title, "--text", message]);
        let shown = zenity.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok();
        if !shown {
            let mut kdialog = Command::new("kdialog");
            kdialog.args(["--title", title, "--error", message]);
            let _ = kdialog.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

/// `s` as NUL-terminated UTF-16 for Win32, without inner NULs.
#[cfg(target_os = "windows")]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().filter(|&c| c != 0).chain([0]).collect()
}

/// Runs a picker helper and returns the non-empty lines it printed.
fn run_picker(mut command: Command) -> io::Result<Vec<String>> {
    let output = no_window(&mut command).stdin(Stdio::null()).stderr(Stdio::null()).output()?;
    // A cancelled dialog exits non-zero or prints nothing; both mean "none".
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// PowerShell prelude: an invisible topmost owner so the dialog appears in
/// front of our window, and UTF-8 output.
const PS_PRELUDE: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
    $owner = New-Object System.Windows.Forms.Form -Property @{TopMost = $true; ShowInTaskbar = $false; Opacity = 0}; \
    $owner.Show(); $owner.Activate();";

/// Runs `script` in PowerShell with `vars` in its environment. Text that is
/// not ours (a story's title) goes in as a variable, never into the script.
fn powershell(script: &str, vars: &[(&str, &str)]) -> io::Result<Vec<String>> {
    // Windows PowerShell 5 is always present as a fallback for PowerShell 7.
    let run = |program: &str| {
        let mut command = Command::new(program);
        command.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", &format!("{PS_PRELUDE} {script}")]);
        command.envs(vars.iter().copied());
        run_picker(command)
    };
    run("pwsh").or_else(|_| run("powershell"))
}

/// Kinds of file a picker offers: a name and the extensions, without dots.
pub type Filter<'a> = (&'a str, &'a [&'a str]);

/// The portrait picker's file types.
const IMAGES: Filter<'static> = ("Images", &["png", "jpg", "jpeg", "webp"]);

/// Asks the user for a PNG, JPEG or WebP image. `None` when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_image() -> io::Result<Option<PathBuf>> {
    Ok(pick_files("Choose a picture", IMAGES, false)?.into_iter().next())
}

/// The files on the clipboard, for a paste that is not text: files copied
/// in the file manager, or else a copied picture, which is saved as a PNG
/// at `picture`. None when the clipboard holds neither.
///
/// # Errors
/// No helper could be started.
// ponytail: copied files are only read on Windows; elsewhere only a copied picture is.
pub fn clipboard_files(picture: &Path) -> io::Result<Vec<PathBuf>> {
    let out = picture.display().to_string();
    let lines = if cfg!(target_os = "windows") {
        let script = "Add-Type -AssemblyName System.Drawing; $c = [System.Windows.Forms.Clipboard]; \
            if ($c::ContainsFileDropList()) { $c::GetFileDropList() } \
            elseif ($c::ContainsImage()) { $c::GetImage().Save($env:ATROXABOT_OUT, [System.Drawing.Imaging.ImageFormat]::Png); \
            $env:ATROXABOT_OUT }";
        powershell(script, &[("ATROXABOT_OUT", &out)])?
    } else if cfg!(target_os = "macos") {
        let script = "try\n\
            set png to (the clipboard as «class PNGf»)\n\
            set f to open for access (POSIX file (item 1 of argv)) with write permission\n\
            write png to f\n\
            close access f\n\
            return item 1 of argv\n\
            end try\n\
            return \"\"";
        let mut command = Command::new("osascript");
        command.args(["-e", "on run argv", "-e", script, "-e", "end run", &out]);
        run_picker(command)?
    } else {
        // Wayland's clipboard first, then X11's.
        let bytes = |program: &str, args: &[&str]| {
            let output = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            Some(output.stdout).filter(|bytes| output.status.success() && !bytes.is_empty())
        };
        let png = bytes("wl-paste", &["--type", "image/png"])
            .or_else(|| bytes("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]));
        match png {
            Some(png) => {
                std::fs::write(picture, png)?;
                vec![out]
            }
            None => Vec::new(),
        }
    };
    Ok(lines.into_iter().map(PathBuf::from).collect())
}

/// Asks the user for files of any kind; none when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_any(title: &str) -> io::Result<Vec<PathBuf>> {
    pick_files(title, ("All files", &["*"]), true)
}

/// Asks the user for files of the kinds `filter` names (several when
/// `multiple`); none when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_files(title: &str, (kind, extensions): Filter<'_>, multiple: bool) -> io::Result<Vec<PathBuf>> {
    let lines = if cfg!(target_os = "windows") {
        let filter = format!("{kind}|{}", extensions.iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";"));
        let multiple = if multiple { "$true" } else { "$false" };
        powershell(
            &format!(
                "$d = New-Object System.Windows.Forms.OpenFileDialog -Property @{{Title = $env:ATROXABOT_TITLE; Filter = $env:ATROXABOT_FILTER; Multiselect = {multiple}}}; \
                 if ($d.ShowDialog($owner) -eq 'OK') {{ $d.FileNames }}"
            ),
            &[("ATROXABOT_TITLE", title), ("ATROXABOT_FILTER", &filter)],
        )?
    } else if cfg!(target_os = "macos") {
        // Type identifiers, from our own list only: never text from elsewhere.
        let types: Vec<String> = extensions.iter().map(|e| format!("\"{}\"", uti(e))).collect();
        let several = if multiple { " with multiple selections allowed" } else { "" };
        let script = format!(
            "set picked to choose file with prompt (item 1 of argv) of type {{{}}}{several}\n\
             if class of picked is not list then set picked to {{picked}}\n\
             set out to \"\"\n\
             repeat with f in picked\n\
             set out to out & POSIX path of f & linefeed\n\
             end repeat\n\
             return out",
            types.join(", ")
        );
        let mut command = Command::new("osascript");
        command.args(["-e", "on run argv", "-e", &script, "-e", "end run", title]);
        run_picker(command)?
    } else {
        // Any file at all includes those without an extension.
        let pattern = |e: &&str| if *e == "*" { "*".to_owned() } else { format!("*.{e}") };
        let patterns: Vec<String> = extensions.iter().map(pattern).collect();
        let mut command = Command::new("zenity");
        command.args([
            "--file-selection",
            &format!("--title={title}"),
            &format!("--file-filter={kind} | {}", patterns.join(" ")),
        ]);
        if multiple {
            command.args(["--multiple", "--separator=\n"]);
        }
        run_picker(command).or_else(|_| {
            let mut command = Command::new("kdialog");
            command.args(["--getopenfilename", ".", &format!("{}|{kind}", patterns.join(" ")), "--title", title]);
            if multiple {
                command.args(["--multiple", "--separate-output"]);
            }
            run_picker(command)
        })?
    };
    Ok(lines.into_iter().map(PathBuf::from).collect())
}

/// macOS's type identifier for a file extension.
fn uti(extension: &str) -> &'static str {
    match extension {
        "png" => "public.png",
        "jpg" | "jpeg" => "public.jpeg",
        "webp" => "org.webmproject.webp",
        "json" => "public.json",
        _ => "public.data",
    }
}

/// Asks the user for a folder. `None` when cancelled.
///
/// # Errors
/// No dialog helper could be started.
pub fn pick_folder(title: &str) -> io::Result<Option<PathBuf>> {
    let lines = if cfg!(target_os = "windows") {
        powershell(
            "$d = New-Object System.Windows.Forms.FolderBrowserDialog -Property @{Description = $env:ATROXABOT_TITLE; UseDescriptionForTitle = $true}; if ($d.ShowDialog($owner) -eq 'OK') { $d.SelectedPath }",
            &[("ATROXABOT_TITLE", title)],
        )?
    } else if cfg!(target_os = "macos") {
        // The title goes in as an argument, never into the script itself.
        let script = "return POSIX path of (choose folder with prompt (item 1 of argv))";
        let mut command = Command::new("osascript");
        command.args(["-e", "on run argv", "-e", script, "-e", "end run", title]);
        run_picker(command)?
    } else {
        let mut command = Command::new("zenity");
        command.args(["--file-selection", "--directory", &format!("--title={title}")]);
        run_picker(command).or_else(|_| {
            let mut command = Command::new("kdialog");
            command.args(["--getexistingdirectory", ".", "--title", title]);
            run_picker(command)
        })?
    };
    Ok(lines.into_iter().next().map(PathBuf::from))
}

/// Runs `program` with `args` in a terminal window the user sees and can
/// type in, with `env` added to the app's environment.
///
/// # Errors
/// No terminal could be started.
pub fn run_in_terminal(program: &Path, args: &[String], env: &[(String, String)]) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `start` opens a console of its own and `cmd /K` keeps it open afterwards. `start` takes its
        // first quoted argument as the window's title and its first unquoted one as the program, so the
        // title goes in quoted, exactly as written here.
        let mut command = Command::new("cmd");
        command.args(["/C", "start"]).raw_arg("\"Sign in to Claude\"").args(["cmd", "/K"]).arg(program).args(args);
        command.envs(env.iter().map(|(name, value)| (name.as_str(), value.as_str())));
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        run_in_unix_terminal(program, args, env)
    }
}

/// [`run_in_terminal`] on macOS and Linux.
#[cfg(not(windows))]
fn run_in_unix_terminal(program: &Path, args: &[String], env: &[(String, String)]) -> io::Result<()> {
    // One shell line: the variables, the program and its arguments, each quoted.
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut line = String::new();
    for (name, value) in env {
        line.push_str(name);
        line.push('=');
        line.push_str(&quote(value));
        line.push(' ');
    }
    line.push_str(&quote(&program.to_string_lossy()));
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    if cfg!(target_os = "macos") {
        // The line goes in as an argument, never into the script itself.
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            "on run argv",
            "-e",
            "tell application \"Terminal\" to do script (item 1 of argv)",
            "-e",
            "tell application \"Terminal\" to activate",
            "-e",
            "end run",
            &line,
        ]);
        return launch(command);
    }
    line.push_str("; echo; echo 'You can close this window.'; read -r _");
    let terminals: [(&str, &[&str]); 4] =
        [("x-terminal-emulator", &["-e"]), ("gnome-terminal", &["--"]), ("konsole", &["-e"]), ("xterm", &["-e"])];
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no terminal emulator found");
    for (terminal, flag) in terminals {
        let mut command = Command::new(terminal);
        command.args(flag).args(["sh", "-c", &line]);
        match command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Opens a real console window for a moment: `cargo test -p atroxabot terminal -- --ignored`.
    #[test]
    #[ignore = "opens a console window"]
    fn a_terminal_runs_the_program_with_the_environment() {
        let out = std::env::temp_dir().join(format!("atroxabot-terminal-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        // The program is `cmd` itself, writing a variable it was given and closing its window again.
        let args = ["/C".to_owned(), format!("echo %ATROXABOT_TEST%>{} & exit", out.display())];
        let env = [("ATROXABOT_TEST".to_owned(), "it works".to_owned())];
        run_in_terminal(Path::new("cmd.exe"), &args, &env).unwrap();
        let mut written = String::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            written = std::fs::read_to_string(&out).unwrap_or_default();
            if !written.is_empty() {
                break;
            }
        }
        // The console `cmd /K` keeps open is closed again: that one process, found by the file it
        // was told to write. Never by window title, which a terminal with tabs shares with other tabs.
        let marker = out.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | Where-Object {{ $_.CommandLine -like '*{marker}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
        );
        let _ = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
        let _ = std::fs::remove_file(&out);
        assert_eq!(written.trim(), "it works", "the program ran in the terminal, with the variable");
    }
}
