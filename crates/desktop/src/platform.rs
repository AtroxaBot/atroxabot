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

/// Says that the app runs, where the installer looks (`AppMutex` in
/// `packaging/windows/atroxabot.iss`): a mutex of this name, held until the
/// process ends. Setup and the uninstaller then ask to quit the app first
/// instead of failing on a file that is in use.
#[cfg(target_os = "windows")]
pub fn mark_running() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *const std::ffi::c_void, owned: i32, name: *const u16) -> *mut std::ffi::c_void;
    }
    let name = wide("AtroxaBot.Running");
    // SAFETY: the name is NUL-terminated UTF-16 that outlives the call, and
    // null attributes are allowed. The handle is kept open on purpose: the
    // mutex is to exist for as long as the process does.
    let _held = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
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
/// No helper could be started, or the picture could not be written.
// ponytail: copied files are only read on Windows; elsewhere only a copied picture is.
pub fn clipboard_files(picture: &Path) -> io::Result<Vec<PathBuf>> {
    // Windows is asked directly: a helper takes a second or two to start and
    // takes the keyboard from the window meanwhile.
    #[cfg(target_os = "windows")]
    if let Some(files) = clipboard_native(picture)? {
        return Ok(files);
    }
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

/// What the Windows clipboard holds that is not text, read from it
/// directly: copied files, or else a copied picture, saved as a PNG at
/// `picture`; nothing when it holds neither. `None` when its picture is a
/// bitmap of a kind [`dib_rgb`] does not read, which is left to PowerShell.
#[cfg(target_os = "windows")]
fn clipboard_native(picture: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    let copied = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get().file_list()).unwrap_or_default();
    if !copied.is_empty() {
        return Ok(Some(copied));
    }
    match win_clipboard::picture() {
        None => Ok(Some(Vec::new())),
        // A PNG as its copier wrote it: its transparency included.
        Some(win_clipboard::Picture::Png(png)) => {
            std::fs::write(picture, png)?;
            Ok(Some(vec![picture.to_path_buf()]))
        }
        Some(win_clipboard::Picture::Bitmap(dib)) => {
            let Some((rgb, width, height)) = dib_rgb(&dib) else { return Ok(None) };
            let mut encoder = png::Encoder::new(io::BufWriter::new(std::fs::File::create(picture)?), width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(io::Error::other)?;
            writer.write_image_data(&rgb).map_err(io::Error::other)?;
            writer.finish().map_err(io::Error::other)?;
            Ok(Some(vec![picture.to_path_buf()]))
        }
    }
}

/// A clipboard bitmap (`CF_DIB`: a `BITMAPINFOHEADER` or a later header,
/// then the pixels) as opaque RGB, with its width and height. It reads what
/// screenshots and copied pictures are: 24 or 32 bits a pixel, uncompressed,
/// blue first. `None` for anything else, and for data shorter than its
/// header says. The fourth byte of a pixel is not read: it is rarely alpha,
/// and a copier that has transparency offers a PNG beside the bitmap.
#[cfg(target_os = "windows")]
fn dib_rgb(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let field = |at: usize| dib.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header = field(0)? as usize;
    let (width, height) = (field(4)?.cast_signed(), field(8)?.cast_signed());
    // Planes in the low half, bits a pixel in the high one.
    let bits = (field(12)? >> 16) as usize;
    if header < 40 || width <= 0 || height == 0 || !matches!(bits, 24 | 32) {
        return None;
    }
    // Uncompressed, or `BI_BITFIELDS` naming the same order of bytes. Its three masks follow a
    // 40-byte header and are fields of a longer one, at the same place either way.
    let masks = match field(16)? {
        0 => 0,
        3 if bits == 32 && (field(40)?, field(44)?, field(48)?) == (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) => {
            if header == 40 { 12 } else { 0 }
        }
        _ => return None,
    };
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height.unsigned_abs()).ok()?);
    // A colour table may sit before the pixels even when they do not index it.
    let table = usize::try_from(field(32)?).ok()?.checked_mul(4)?;
    let start = header.checked_add(masks)?.checked_add(table)?;
    // Rows are padded to four bytes.
    let stride = w.checked_mul(bits)?.checked_add(31)? / 32 * 4;
    let pixels = dib.get(start..start.checked_add(stride.checked_mul(h)?)?)?;
    let step = bits / 8;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        // The bottom row comes first, unless the height is negative.
        let line = if height > 0 { h - 1 - row } else { row } * stride;
        for pixel in pixels[line..line + w * step].chunks_exact(step) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    Some((rgb, u32::try_from(w).ok()?, u32::try_from(h).ok()?))
}

/// The clipboard's picture, through Win32: `user32` and `kernel32`, which
/// every Windows GUI process loads.
#[cfg(target_os = "windows")]
mod win_clipboard {
    use std::ffi::c_void;
    use std::time::Duration;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(memory: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(memory: *mut c_void) -> i32;
        fn GlobalSize(memory: *const c_void) -> usize;
    }
    /// A device-independent bitmap: what Windows makes of any copied picture.
    const CF_DIB: u32 = 8;

    /// A picture as the clipboard holds it.
    pub enum Picture {
        /// A PNG file's bytes: browsers, image editors and the snipping tool offer one.
        Png(Vec<u8>),
        /// A `CF_DIB` bitmap: a header, then the pixels.
        Bitmap(Vec<u8>),
    }

    /// The picture on the clipboard, if it holds one (and no other program
    /// is holding the clipboard open for longer than a moment).
    pub fn picture() -> Option<Picture> {
        let opened = (0..5).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: a null owner is allowed; it ties the open clipboard to this thread.
            unsafe { OpenClipboard(std::ptr::null_mut()) != 0 }
        });
        if !opened {
            return None;
        }
        let name = super::wide("PNG");
        // SAFETY: the name is NUL-terminated UTF-16 that outlives the call, and the clipboard is
        // open on this thread from here until it is closed, as `data` needs.
        unsafe {
            let png = RegisterClipboardFormatW(name.as_ptr());
            let found = data(png).map(Picture::Png).or_else(|| data(CF_DIB).map(Picture::Bitmap));
            CloseClipboard();
            found
        }
    }

    /// A copy of what the clipboard holds in `format`, if anything.
    ///
    /// # Safety
    /// This thread must have the clipboard open, and `format` must be one
    /// whose data is a global memory block (`CF_DIB` and registered formats are).
    unsafe fn data(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open (the caller's promise), so the handle stays the
        // clipboard's and valid until it is closed; a null one is checked for.
        let memory = unsafe { GetClipboardData(format) };
        if memory.is_null() {
            return None;
        }
        // SAFETY: `memory` is a global memory block (the caller's promise). While it is locked,
        // `bytes` points to at least `GlobalSize` readable bytes, which are copied before the unlock.
        unsafe {
            let bytes = GlobalLock(memory).cast::<u8>();
            if bytes.is_null() {
                return None;
            }
            let copy = std::slice::from_raw_parts(bytes, GlobalSize(memory)).to_vec();
            GlobalUnlock(memory);
            Some(copy)
        }
    }
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
/// type in, with `env` added to the app's environment. `title` names the
/// window where the system lets it be named (Windows).
///
/// # Errors
/// No terminal could be started.
pub fn run_in_terminal(title: &str, program: &Path, args: &[String], env: &[(String, String)]) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `start` opens a console of its own and `cmd /K` keeps it open afterwards. `start` takes its
        // first quoted argument as the window's title and its first unquoted one as the program, so the
        // title goes in quoted, with nothing in it that could end the quotes or be read by `cmd`.
        let title: String = title.chars().filter(|c| c.is_alphanumeric() || " .-".contains(*c)).collect();
        let mut command = Command::new("cmd");
        command.args(["/C", "start"]).raw_arg(format!("\"{title}\"")).args(["cmd", "/K"]).arg(program).args(args);
        command.envs(env.iter().map(|(name, value)| (name.as_str(), value.as_str())));
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        let _ = title;
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
        run_in_terminal("AtroxaBot test", Path::new("cmd.exe"), &args, &env).unwrap();
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

    /// An installer's one-line command (a PowerShell pipe, as `irm … | iex` is) gets through the
    /// terminal's `cmd` whole: `cargo test -p atroxabot installer_line -- --ignored`.
    #[test]
    #[ignore = "opens a console window"]
    fn an_installer_line_runs_whole_in_a_terminal() {
        let out = std::env::temp_dir().join(format!("atroxabot-installer-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let line = format!("'it works' | Out-File -Encoding ascii -FilePath '{}'", out.display());
        let command = atroxabot_core::TerminalCommand::shell_line(&line, "");
        run_in_terminal("Install test", &command.program, &command.args, &command.env).unwrap();
        let mut written = String::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            written = std::fs::read_to_string(&out).unwrap_or_default();
            if !written.is_empty() {
                break;
            }
        }
        // The console `cmd /K` keeps open is closed again, found by the file it was told to write.
        let marker = out.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='cmd.exe'\" | Where-Object {{ $_.CommandLine -like '*{marker}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
        );
        let _ = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
        let _ = std::fs::remove_file(&out);
        assert_eq!(written.trim(), "it works", "the pipe ran in PowerShell, not in cmd");
    }

    /// A `BITMAPINFOHEADER` for `width` by `height` at `bits` a pixel, compressed as `compression` says.
    fn dib_header(width: i32, height: i32, bits: u16, compression: u32) -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&width.to_le_bytes());
        dib.extend_from_slice(&height.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&bits.to_le_bytes());
        dib.extend_from_slice(&compression.to_le_bytes());
        dib.extend_from_slice(&[0; 20]);
        dib
    }

    #[test]
    fn a_clipboard_bitmap_becomes_rgb_and_a_broken_one_nothing() {
        // Two by two at 24 bits: blue first, the bottom row first, rows padded to four bytes.
        let mut dib = dib_header(2, 2, 24, 0);
        dib.extend_from_slice(&[1, 2, 3, 4, 5, 6, 0, 0]);
        dib.extend_from_slice(&[7, 8, 9, 10, 11, 12, 0, 0]);
        assert_eq!(dib_rgb(&dib), Some((vec![9, 8, 7, 12, 11, 10, 3, 2, 1, 6, 5, 4], 2, 2)));
        // At 32 bits, the top row first, with the masks that name the usual order.
        let mut dib = dib_header(1, -2, 32, 3);
        for mask in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF] {
            dib.extend_from_slice(&mask.to_le_bytes());
        }
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        assert_eq!(dib_rgb(&dib), Some((vec![3, 2, 1, 6, 5, 4], 1, 2)));
        // Other masks, a palette, pixels that are not all there, a size no memory holds: nothing.
        let mut odd = dib.clone();
        odd[40] = 0xFF;
        assert_eq!(dib_rgb(&odd), None);
        assert_eq!(dib_rgb(&dib_header(2, 2, 8, 0)), None);
        assert_eq!(dib_rgb(&dib[..dib.len() - 1]), None);
        assert_eq!(dib_rgb(&dib_header(i32::MAX, i32::MIN, 32, 0)), None);
        assert_eq!(dib_rgb(&dib_header(0, 1, 24, 0)), None);
        assert_eq!(dib_rgb(&[40, 0, 0]), None);
        assert_eq!(dib_rgb(&[]), None);
    }

    /// Reads the picture that is on the real clipboard (copy one first: a screenshot will do)
    /// and leaves the clipboard as it is: `cargo test -p atroxabot clipboard -- --ignored --nocapture`.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "needs a picture on the clipboard"]
    fn a_copied_picture_is_read_without_a_helper() {
        let out = std::env::temp_dir().join(format!("atroxabot-clipboard-{}.png", std::process::id()));
        let started = std::time::Instant::now();
        let files = clipboard_native(&out).unwrap();
        let took = started.elapsed();
        assert_eq!(files, Some(vec![out.clone()]), "copy a picture first");
        let decoded = crate::image::decode_png(&std::fs::read(&out).unwrap());
        let _ = std::fs::remove_file(&out);
        let (rgba, width, height) = decoded.unwrap();
        assert!(width > 0 && height > 0 && rgba.len() == (width * height * 4) as usize);
        println!("{width} by {height} read and saved in {took:?}");
        assert!(took < std::time::Duration::from_secs(1), "a paste should not be waited for: {took:?}");
    }
}
