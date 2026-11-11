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

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use app::{App, WorkerEvent};

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
        // On macOS, quitting from the menu (Cmd+Q) exits the process right
        // after this, so `run_app` never returns: drop the app here, which
        // finishes its queue of file writes.
        self.app = None;
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

fn main() -> ExitCode {
    log::init();
    // Two copies would write the same files over each other.
    let _lock = match atroxabot_core::lock_instance(LOCK_FILE) {
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
