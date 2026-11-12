//! AtroxaBot: a native, GPU-rendered desktop app for agents you chat with and call.
//!
//! No web view and no UI framework: winit provides the window, wgpu draws
//! every pixel through a single instanced SDF pipeline (`gpu.rs`,
//! `shader.wgsl`), and the UI is immediate mode (`ui.rs`, `shell/`),
//! redrawn only when something changes.

// Release builds are GUI apps on Windows: no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod atlas;
mod color;
mod doc;
mod editor;
mod font;
mod form;
mod gpu;
mod highlight;
mod image;
mod log;
mod logo;
mod markdown;
mod orb;
mod paint;
mod platform;
mod raster;
mod shell;
mod text;
mod theme;
mod titlebar;
mod tray;
mod ui;

use std::process::ExitCode;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use app::{App, WorkerEvent};
use atroxabot_core::update;

/// Bridges winit callbacks to the [`App`], which can only be created once
/// the event loop is running.
struct Handler {
    app: Option<App>,
    proxy: EventLoopProxy<WorkerEvent>,
    error: Option<app::StartupError>,
}

impl ApplicationHandler<WorkerEvent> for Handler {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let (StartCause::ResumeTimeReached { .. }, Some(app)) = (cause, &mut self.app) {
            app.wake();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }
        match App::new(event_loop, self.proxy.clone()) {
            Ok(app) => self.app = Some(app),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WorkerEvent) {
        if let Some(app) = &mut self.app {
            app.worker_event(event_loop, event);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let Some(app) = &mut self.app {
            app.window_event(event_loop, event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(app) = &self.app {
            event_loop.set_control_flow(app.control_flow());
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        let restart = self.app.as_mut().and_then(App::take_restart);
        // On macOS, quitting from the menu (Cmd+Q) exits the process right
        // after this, so `run_app` never returns: drop the app here, which
        // finishes its queue of file writes.
        self.app = None;
        // An update was moved in: the new version starts, and waits for this one to be gone.
        if let Some(start) = restart
            && let Err(e) = update::relaunch(&start)
        {
            log::error(format!("cannot start the new version at {}: {e}", start.display()));
            platform::alert("AtroxaBot was updated", "It could not start again by itself: start it as you usually do.");
        }
    }
}

/// Name of the lock file in `~/.atroxabot/` that keeps a second copy away.
const LOCK_FILE: &str = "desktop.lock";

/// Tells the user why the app cannot run, in a dialog and the log.
fn fail(message: &str) -> ExitCode {
    log::error(message);
    platform::alert("AtroxaBot cannot start", message);
    ExitCode::FAILURE
}

/// Takes the lock that keeps a second copy away. Started after an update
/// (`patient`), the copy that made way may still be closing: it is waited
/// for a while.
fn lock_instance(patient: bool) -> atroxabot_core::Result<Option<std::fs::File>> {
    let tries = if patient { 150 } else { 1 };
    for attempt in 1..=tries {
        match atroxabot_core::lock_instance(LOCK_FILE)? {
            None if attempt < tries => std::thread::sleep(Duration::from_millis(100)),
            lock => return Ok(lock),
        }
    }
    Ok(None)
}

fn main() -> ExitCode {
    log::init();
    let updated = std::env::args().any(|argument| argument == update::AFTER_UPDATE);
    // Two copies would write the same files over each other.
    let _lock = match lock_instance(updated) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            // Perhaps it only waits in the tray: it shows its window instead.
            #[cfg(windows)]
            if tray::wake_running() {
                return ExitCode::SUCCESS;
            }
            return fail("AtroxaBot is already running. Switch to its window, or close it before starting it again.");
        }
        Err(e) => {
            log::error(format!("cannot take the instance lock, running without it: {e}"));
            None
        }
    };
    #[cfg(windows)]
    platform::mark_running();
    // What an update left beside the app goes, off the start's way: nothing here looks for a new one.
    let swept = std::thread::Builder::new().name("atroxabot-update".into()).spawn(move || update::sweep(updated));
    if let Err(e) = swept {
        log::error(format!("cannot spawn the thread that clears up after updates: {e}"));
    }
    let event_loop = match EventLoop::<WorkerEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => return fail(&format!("The window system could not be started: {e}")),
    };
    let mut handler = Handler { app: None, proxy: event_loop.create_proxy(), error: None };
    let result = event_loop.run_app(&mut handler);
    if let Err(e) = result {
        return fail(&format!("The window system stopped unexpectedly: {e}"));
    }
    if let Some(e) = handler.error {
        return fail(&format!("AtroxaBot could not open its window: {e}. Updating your graphics driver may help."));
    }
    ExitCode::SUCCESS
}
