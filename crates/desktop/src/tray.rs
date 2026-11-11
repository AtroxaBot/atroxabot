//! What closing the window does ([`CloseAction`]), and the icon in the
//! Windows notification area (the tray) that brings a hidden window back.
//!
//! Windows: closing hides the window and puts an icon by the clock; a click
//! on it opens the window again, its menu opens it or quits. A second start
//! of the app asks the running one to show itself ([`wake_running`]).
//! macOS: closing hides the app, as Cmd+H does; its Dock icon brings it back.
//! Linux has no tray without a D-Bus client (`StatusNotifierItem`), so there
//! closing minimises.

/// What the window's close button (and Alt+F4) does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// The window goes away and the app keeps running: in the tray on
    /// Windows, hidden behind its Dock icon on macOS.
    Background,
    /// The window is minimised to the taskbar or Dock.
    Minimize,
    /// The app quits.
    Quit,
}

/// Whether this platform can keep running without a window ([`CloseAction::Background`]).
const BACKGROUND: bool = cfg!(any(windows, target_os = "macos"));

impl CloseAction {
    /// The choices this platform offers, its default first.
    pub fn offered() -> &'static [Self] {
        if BACKGROUND { &[Self::Background, Self::Minimize, Self::Quit] } else { &[Self::Minimize, Self::Quit] }
    }

    /// What the config calls it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Minimize => "minimize",
            Self::Quit => "quit",
        }
    }

    /// The one saved as `key`; the platform's default if none is, or the
    /// one saved is not offered here.
    pub fn from_key(key: Option<&str>) -> Self {
        let offered = Self::offered();
        offered.iter().copied().find(|action| Some(action.key()) == key).unwrap_or(offered[0])
    }

    /// Its name in the settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Background if cfg!(windows) => "Keep running in the tray",
            Self::Background => "Keep running in the Dock",
            Self::Minimize if cfg!(windows) => "Minimise to the taskbar",
            Self::Minimize if cfg!(target_os = "macos") => "Minimise to the Dock",
            Self::Minimize => "Minimise",
            Self::Quit => "Quit",
        }
    }

    /// A line on what it means.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Background if cfg!(windows) => "Hides the window; the tray icon brings it back.",
            Self::Background => "Hides the app; its Dock icon brings it back.",
            Self::Minimize => "Agents keep working in the minimised window.",
            Self::Quit => "Replies in progress and calls end with it.",
        }
    }
}

/// What the user did with the tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Open the window: a click on the icon, its menu, or a second start of the app.
    Open,
    /// Quit, from the icon's menu.
    Quit,
}

#[cfg(windows)]
pub use windows::{Tray, wake_running};

/// The tray where there is none: nothing to show.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
#[allow(clippy::unused_self, reason = "the same calls as the Windows tray's, doing nothing")]
impl Tray {
    /// There is no tray here.
    ///
    /// # Errors
    /// Always: this platform has no tray.
    pub fn start(_: impl Fn(TrayEvent) + Send + 'static) -> std::io::Result<Self> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no tray on this platform"))
    }

    /// Does nothing.
    pub fn show(&self) {}

    /// Does nothing.
    pub fn hide(&self) {}
}

#[cfg(windows)]
mod windows {
    //! `Shell_NotifyIconW` and a hidden window of our own to receive its
    //! clicks, on a thread with its own message loop. The window is a
    //! top-level one that is never shown, not a message-only one: only
    //! top-level windows hear Explorer's `TaskbarCreated` (sent when it
    //! restarts, which drops every icon), and only they can take the
    //! foreground that a popup menu needs to close properly.

    use std::cell::{Cell, OnceCell};
    use std::ffi::c_void;
    use std::io;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use super::TrayEvent;

    type Handle = *mut c_void;
    type Procedure = unsafe extern "system" fn(Handle, u32, usize, isize) -> isize;

    /// The hidden window's class; another start of the app finds it by it.
    const CLASS: &str = if cfg!(test) { "AtroxaBotTrayTest" } else { "AtroxaBotTray" };
    const WM_NULL: u32 = 0;
    const WM_DESTROY: u32 = 0x0002;
    const WM_CLOSE: u32 = 0x0010;
    const WM_CONTEXTMENU: u32 = 0x007B;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_APP: u32 = 0x8000;
    /// What the icon sends: its mouse message in `lparam`.
    const CALLBACK: u32 = WM_APP + 1;
    /// Put the icon in the tray.
    const SHOW: u32 = WM_APP + 2;
    /// Take it out.
    const HIDE: u32 = WM_APP + 3;
    /// Another start of the app asks for the window.
    const WAKE: u32 = WM_APP + 4;
    const NIM_ADD: u32 = 0;
    const NIM_DELETE: u32 = 2;
    const NIF_MESSAGE: u32 = 1;
    const NIF_ICON: u32 = 2;
    const NIF_TIP: u32 = 4;
    const MF_STRING: u32 = 0;
    const MF_SEPARATOR: u32 = 0x0800;
    const TPM_RIGHTBUTTON: u32 = 0x0002;
    const TPM_NONOTIFY: u32 = 0x0080;
    const TPM_RETURNCMD: u32 = 0x0100;
    const IMAGE_ICON: u32 = 1;
    const SM_CXSMICON: i32 = 49;
    const SM_CYSMICON: i32 = 50;
    /// `IDI_APPLICATION`, the stock icon when ours is not embedded.
    const IDI_APPLICATION: usize = 32512;
    /// `ASFW_ANY`.
    const ANY_PROCESS: u32 = u32::MAX;
    /// The menu's choices.
    const OPEN: usize = 1;
    const QUIT: usize = 2;

    #[repr(C)]
    struct WindowClass {
        style: u32,
        procedure: Procedure,
        class_extra: i32,
        window_extra: i32,
        instance: Handle,
        icon: Handle,
        cursor: Handle,
        background: Handle,
        menu_name: *const u16,
        class_name: *const u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        window: Handle,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        point: Point,
        private: u32,
    }

    /// `NOTIFYICONDATAW`, whole (976 bytes on 64-bit Windows).
    #[repr(C)]
    struct NotifyIconData {
        size: u32,
        window: Handle,
        id: u32,
        flags: u32,
        callback: u32,
        icon: Handle,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        version: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid: [u8; 16],
        balloon_icon: Handle,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassW(class: *const WindowClass) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Handle,
            menu: Handle,
            instance: Handle,
            param: *mut c_void,
        ) -> Handle;
        fn DefWindowProcW(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize;
        fn GetMessageW(message: *mut Msg, window: Handle, min: u32, max: u32) -> i32;
        fn TranslateMessage(message: *const Msg) -> i32;
        fn DispatchMessageW(message: *const Msg) -> isize;
        fn PostMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> i32;
        fn PostQuitMessage(code: i32);
        fn DestroyWindow(window: Handle) -> i32;
        fn FindWindowW(class: *const u16, name: *const u16) -> Handle;
        fn RegisterWindowMessageW(name: *const u16) -> u32;
        fn CreatePopupMenu() -> Handle;
        fn AppendMenuW(menu: Handle, flags: u32, id: usize, text: *const u16) -> i32;
        fn TrackPopupMenu(
            menu: Handle,
            flags: u32,
            x: i32,
            y: i32,
            reserved: i32,
            window: Handle,
            rect: *const c_void,
        ) -> i32;
        fn DestroyMenu(menu: Handle) -> i32;
        fn SetForegroundWindow(window: Handle) -> i32;
        fn AllowSetForegroundWindow(process: u32) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn LoadImageW(instance: Handle, name: *const u16, kind: u32, width: i32, height: i32, flags: u32) -> Handle;
        fn LoadIconW(instance: Handle, name: *const u16) -> Handle;
        fn DestroyIcon(icon: Handle) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Handle;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
    }

    thread_local! {
        /// Where the tray thread's events go.
        static SINK: OnceCell<Box<dyn Fn(TrayEvent)>> = const { OnceCell::new() };
        /// Whether the icon is (meant to be) in the tray.
        static SHOWN: Cell<bool> = const { Cell::new(false) };
        /// The icon, and whether it is ours to destroy.
        static ICON: Cell<(usize, bool)> = const { Cell::new((0, false)) };
        /// The message Explorer sends when it restarts.
        static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    }

    /// `s` as a NUL-terminated UTF-16 string.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The tray icon's thread and hidden window. Dropping it takes the icon
    /// away and ends the thread.
    pub struct Tray {
        /// The hidden window, as a number so the handle can cross threads.
        window: usize,
        thread: Option<JoinHandle<()>>,
    }

    impl Tray {
        /// Starts the tray's thread; `on_event` runs on it for each click.
        /// The icon is not shown until [`Tray::show`].
        ///
        /// # Errors
        /// The thread or its window could not be made.
        pub fn start(on_event: impl Fn(TrayEvent) + Send + 'static) -> io::Result<Self> {
            let (made, window) = mpsc::channel();
            let thread = std::thread::Builder::new().name("atroxabot-tray".into()).spawn(move || {
                SINK.with(|sink| {
                    let _ = sink.set(Box::new(on_event));
                });
                // SAFETY: plain Win32 calls with valid, NUL-terminated strings.
                let window = unsafe { create() };
                let failed = window.is_null();
                let _ = made.send(if failed { Err(io::Error::last_os_error()) } else { Ok(window as usize) });
                if !failed {
                    // SAFETY: the loop of the thread that owns the window.
                    unsafe { run() };
                }
                let (icon, owned) = ICON.get();
                if owned {
                    // SAFETY: loaded by `create` and no longer in the tray.
                    unsafe { DestroyIcon(icon as Handle) };
                }
            })?;
            let window = window.recv().map_err(|_| io::Error::other("the tray thread ended"))??;
            Ok(Self { window, thread: Some(thread) })
        }

        /// Puts the icon in the tray.
        pub fn show(&self) {
            self.post(SHOW);
        }

        /// Takes the icon out of the tray.
        pub fn hide(&self) {
            self.post(HIDE);
        }

        fn post(&self, message: u32) {
            // SAFETY: posting to our own window; fails harmlessly once it is gone.
            unsafe { PostMessageW(self.window as Handle, message, 0, 0) };
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.post(WM_CLOSE);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Asks a running copy of the app to show its window. Returns whether
    /// there was one to ask.
    pub fn wake_running() -> bool {
        let class = wide(CLASS);
        // SAFETY: a lookup by a valid class name, then a post to what it found.
        unsafe {
            let window = FindWindowW(class.as_ptr(), std::ptr::null());
            if window.is_null() {
                return false;
            }
            // We were just started by the user, so we may let it come to the front.
            AllowSetForegroundWindow(ANY_PROCESS);
            PostMessageW(window, WAKE, 0, 0) != 0
        }
    }

    /// Registers the class and makes the hidden window, and loads the icon.
    unsafe fn create() -> Handle {
        let class = wide(CLASS);
        let title = wide("AtroxaBot");
        // SAFETY: the caller's; every pointer outlives its call.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let description = WindowClass {
                style: 0,
                procedure,
                class_extra: 0,
                window_extra: 0,
                instance,
                icon: std::ptr::null_mut(),
                cursor: std::ptr::null_mut(),
                background: std::ptr::null_mut(),
                menu_name: std::ptr::null(),
                class_name: class.as_ptr(),
            };
            // Fails only if it is already registered (a second tray in tests).
            RegisterClassW(&raw const description);
            TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
            // `build.rs` embeds the app icon as resource 1.
            let (width, height) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
            let icon = LoadImageW(instance, std::ptr::without_provenance(1), IMAGE_ICON, width, height, 0);
            ICON.set(if icon.is_null() {
                (LoadIconW(std::ptr::null_mut(), std::ptr::without_provenance(IDI_APPLICATION)) as usize, false)
            } else {
                (icon as usize, true)
            });
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        }
    }

    /// The thread's message loop, until the window is destroyed.
    unsafe fn run() {
        let mut message = Msg {
            window: std::ptr::null_mut(),
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            point: Point::default(),
            private: 0,
        };
        // SAFETY: the caller's; `message` is a valid MSG.
        unsafe {
            while GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }

    fn emit(event: TrayEvent) {
        SINK.with(|sink| {
            if let Some(sink) = sink.get() {
                sink(event);
            }
        });
    }

    /// Adds the icon to the tray (`NIM_ADD`) or takes it out (`NIM_DELETE`).
    unsafe fn notify(window: Handle, message: u32) {
        let mut data = NotifyIconData {
            size: size_of::<NotifyIconData>() as u32,
            window,
            id: 1,
            flags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            callback: CALLBACK,
            icon: ICON.get().0 as Handle,
            tip: [0; 128],
            state: 0,
            state_mask: 0,
            info: [0; 256],
            version: 0,
            info_title: [0; 64],
            info_flags: 0,
            guid: [0; 16],
            balloon_icon: std::ptr::null_mut(),
        };
        for (slot, unit) in data.tip.iter_mut().zip("AtroxaBot".encode_utf16()) {
            *slot = unit;
        }
        // SAFETY: the caller's; `data` is a whole NOTIFYICONDATAW.
        unsafe { Shell_NotifyIconW(message, &raw const data) };
    }

    /// The icon's menu: open or quit.
    unsafe fn menu(window: Handle) {
        // SAFETY: the caller's; the menu is ours until destroyed here.
        let chosen = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING, OPEN, wide("Open AtroxaBot").as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, QUIT, wide("Quit AtroxaBot").as_ptr());
            let mut point = Point::default();
            GetCursorPos(&raw mut point);
            // Without these two the menu would not close when clicked beside.
            SetForegroundWindow(window);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                window,
                std::ptr::null(),
            );
            PostMessageW(window, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        };
        match chosen as usize {
            OPEN => emit(TrayEvent::Open),
            QUIT => emit(TrayEvent::Quit),
            _ => {}
        }
    }

    unsafe extern "system" fn procedure(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: called by Windows on the tray thread, for our window.
        unsafe {
            match message {
                CALLBACK => match lparam as u32 {
                    WM_LBUTTONUP => emit(TrayEvent::Open),
                    WM_RBUTTONUP | WM_CONTEXTMENU => menu(window),
                    _ => {}
                },
                SHOW if !SHOWN.get() => {
                    SHOWN.set(true);
                    notify(window, NIM_ADD);
                }
                HIDE if SHOWN.get() => {
                    SHOWN.set(false);
                    notify(window, NIM_DELETE);
                }
                SHOW | HIDE => {}
                WAKE => emit(TrayEvent::Open),
                WM_CLOSE => {
                    if SHOWN.replace(false) {
                        notify(window, NIM_DELETE);
                    }
                    DestroyWindow(window);
                }
                WM_DESTROY => PostQuitMessage(0),
                // Explorer restarted and forgot every icon: ours goes back.
                m if m != 0 && m == TASKBAR_CREATED.get() => {
                    if SHOWN.get() {
                        notify(window, NIM_ADD);
                    }
                }
                _ => return DefWindowProcW(window, message, wparam, lparam),
            }
        }
        0
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[cfg(target_pointer_width = "64")]
        fn the_icon_data_is_as_large_as_windows_expects() {
            assert_eq!(size_of::<NotifyIconData>(), 976);
        }

        #[test]
        fn a_second_start_wakes_the_running_tray() {
            let (sent, events) = mpsc::channel();
            let tray = Tray::start(move |event| {
                let _ = sent.send(event);
            })
            .expect("the tray starts");
            assert!(wake_running(), "the hidden window is found");
            let event = events.recv_timeout(std::time::Duration::from_secs(5));
            assert_eq!(event, Ok(TrayEvent::Open));
            drop(tray);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_close_action_is_read_back_or_the_default_taken() {
        let default = CloseAction::offered()[0];
        assert_eq!(CloseAction::from_key(None), default);
        assert_eq!(CloseAction::from_key(Some("nonsense")), default);
        for &action in CloseAction::offered() {
            assert_eq!(CloseAction::from_key(Some(action.key())), action);
        }
        // Linux cannot keep running without a window: it minimises instead.
        let background = CloseAction::from_key(Some("background"));
        assert_eq!(background == CloseAction::Background, cfg!(any(windows, target_os = "macos")));
    }
}
