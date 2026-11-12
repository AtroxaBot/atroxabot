//! Application state, event routing and background work.
//!
//! Threading model: the winit thread owns all state and renders. Model
//! requests, tools, the Claude Agent's process, voice calls, dialogs and
//! file reads run on other threads that report back through
//! [`WorkerEvent`]s posted to the event loop, and every file the app writes
//! goes through one [`Writer`] thread, so the UI never blocks.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use atroxabot_core::git;
use atroxabot_core::skills::{self, Library, Skill};
use atroxabot_core::stats::{self, Ledger, Record};
use atroxabot_core::tools::{Jobs, Workspace};
use atroxabot_core::update;
use atroxabot_core::worktree;
use atroxabot_core::{
    Agent, AgentEvent, ApiEngine, ClaudeAgent, ClaudeError, ClaudeInfo, ClaudeMcpServer, Client, CodexAgent,
    CodexError, CodexInfo, Config, Connection, ConnectionKind, Control, Error, FollowUp, Limits, McpHub, McpServer,
    Model, PlanCheck, Records, SearchHit, Session, SessionStore, SessionSummary, Verdict,
};
use atroxabot_voice::{Call, CallEvent, CallOptions, VoiceConfig};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::Key;
use winit::window::{CursorIcon, Theme, UserAttentionType, Window};

use crate::gpu::{Frame, GpuError, Instance, Renderer};
use crate::image::{self, ImageAtlas, ImageKey};
use crate::log;
use crate::paint::{Painter, Rect};
use crate::platform;
use crate::shell::{Cli, Shell};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::{Palette, Scheme};
use crate::titlebar::{self, Command, Grab};
use crate::tray::{CloseAction, Tray, TrayEvent};
use crate::ui::{Ui, copy};

/// Length of the cross-fade when the colour scheme changes.
const THEME_FADE_SECS: f32 = 0.25;
/// Window size on first start, in logical pixels.
const WINDOW_SIZE: (u32, u32) = (1240, 820);
/// Smallest window size, in logical pixels.
const MIN_WINDOW_SIZE: (u32, u32) = (820, 560);
/// Interface zoom limits, in percent.
const ZOOM: (u16, u16) = (50, 200);
/// How far one press of Ctrl/Cmd + or - zooms, in percent.
const ZOOM_STEP: u16 = 10;
/// Wait between attempts to bring the GPU back after it was lost.
const GPU_RETRY: Duration = Duration::from_secs(2);
/// Wait before drawing again when a frame could not be shown (a hidden or
/// busy surface) while something moves: not at once, which would spin.
const SKIPPED_RETRY: Duration = Duration::from_millis(100);

/// Results delivered from other threads.
pub enum WorkerEvent {
    /// Something happened in the turn of a session.
    Agent {
        /// The session's id.
        session: String,
        /// Which of the session's engines it came from ([`App::renew`]).
        generation: u64,
        /// What happened.
        event: AgentEvent,
    },
    /// A connection's model list arrived.
    Models {
        /// The connection's id.
        connection: String,
        /// The models, or why they could not be listed.
        result: Result<Vec<Model>, Error>,
    },
    /// A session's messages were read from disk.
    SessionLoaded {
        /// The session's id.
        session: String,
        /// The session (boxed: it dwarfs the other events), or why it could not be read.
        result: Result<Box<Session>, Error>,
    },
    /// Something happened on the voice call.
    Call(CallEvent),
    /// The folder picker closed with this folder, if any.
    FolderPicked(Option<String>),
    /// The file picker closed with these files to send along, if any.
    FilesPicked(Vec<String>),
    /// The picture picker closed with this image file, if any.
    PicturePicked(Option<String>),
    /// Claude Code said (or could not say) what a Claude account offers.
    ClaudeInfo {
        /// The account's connection id.
        connection: String,
        /// Its models and who is signed in, or why it could not be asked.
        result: Result<ClaudeInfo, ClaudeError>,
    },
    /// Codex said (or could not say) what a Codex account offers.
    CodexInfo {
        /// The account's connection id.
        connection: String,
        /// Its models and who is signed in, or why it could not be asked.
        result: Result<CodexInfo, CodexError>,
    },
    /// Claude Code said (or could not say) where a Claude account's plan limits stand.
    LimitsChecked {
        /// The account's connection id.
        connection: String,
        /// Its plan's windows, or why it could not say.
        result: Result<PlanCheck, ClaudeError>,
    },
    /// Claude Code said (or could not say) which MCP servers a Claude account's sessions get.
    Connectors {
        /// The account's connection id.
        connection: String,
        /// The servers and how each is doing, or why it could not be asked.
        result: Result<Vec<ClaudeMcpServer>, ClaudeError>,
    },
    /// Sessions whose messages hold the searched text.
    SearchResults {
        /// The search they answer, to drop a late one.
        generation: u64,
        /// The sessions.
        hits: Vec<SearchHit>,
    },
    /// Every session's usage was read, for the statistics.
    Stats(Box<Ledger>),
    /// Git answered about a folder.
    Git {
        /// The folder.
        folder: String,
        /// What came of it.
        report: git::Report,
    },
    /// A skill library job is done.
    Skills(skills::Report),
    /// Something asked about the sessions' worktrees is done.
    Worktree(worktree::Report),
    /// A session's worktree is ready for the message that waited for it (or could not be made).
    WorktreeReady {
        /// The message that waits.
        pending: Box<Pending>,
        /// The engine it was to go to ([`App::renew`]): a newer one means it was given up.
        generation: u64,
        /// The worktree and the hold on it, or why there is none.
        prepared: Result<worktree::Prepared, String>,
    },
    /// A folder's files were listed, for naming one in a message.
    Files {
        /// The folder.
        folder: String,
        /// Its files, relative to it.
        files: Vec<String>,
    },
    /// A thumbnail was decoded (or could not be).
    Thumbnail(ImageKey, Result<image::Pixels, String>),
    /// Something went wrong out of sight: what to tell the user.
    Problem(String),
    /// The tray icon was clicked, or another start of the app asks for the window.
    Tray(TrayEvent),
    /// What looking for an update or downloading one came to, or how far a download is.
    Update(update::Report),
    /// The app is to close: a new version is in place and starts once it has.
    Quit,
}

/// A message on its way to an engine: what [`Action::Send`] carries.
pub struct Pending {
    agent: Agent,
    connection: Connection,
    model: Option<Box<Model>>,
    session: Session,
    text: String,
}

/// Something the interface wants done that needs a thread, a file or a device.
pub enum Action {
    /// Answer the latest prompt of `session`.
    Send {
        /// Who answers.
        agent: Box<Agent>,
        /// What it thinks through.
        connection: Box<Connection>,
        /// The model's prices and context window, when listed.
        model: Option<Box<Model>>,
        /// The session, with the prompt already in it.
        session: Box<Session>,
        /// The prompt as the Claude Agent is sent it.
        text: String,
    },
    /// Stop the turn of this session, with what was sent to it meanwhile.
    Stop(String),
    /// Stop the step in progress in this session, so the agent reads at once
    /// what was sent to it meanwhile (it is answered next).
    Interrupt(String),
    /// Hand a message the user sent while it works to the turn of a session.
    FollowUp {
        /// The session's id.
        session: String,
        /// The message.
        message: Box<FollowUp>,
    },
    /// Answer an approval of a session's turn.
    Decide {
        /// The session's id.
        session: String,
        /// The approval's id.
        ask: String,
        /// The answer.
        verdict: Verdict,
    },
    /// Summarise the conversation of an API session now (`/compact`).
    Compact {
        /// Who summarises.
        agent: Box<Agent>,
        /// What it thinks through.
        connection: Box<Connection>,
        /// The model's prices and context window, when listed.
        model: Option<Box<Model>>,
        /// The session.
        session: Box<Session>,
    },
    /// End the agent's process (Claude Code, Codex) of this session, if it has one.
    CloseLive(String),
    /// Stop the commands an API session's agent left running in the background.
    StopJobs(String),
    /// Ask the Claude Agent process of this session, if it has one, what its context holds.
    AskContext(String),
    /// Read a session's messages.
    LoadSession(String),
    /// Write a session to disk.
    SaveSession(Box<Session>),
    /// Delete a session's file.
    DeleteSession(String),
    /// Write an agent to disk.
    SaveAgent(Box<Agent>),
    /// Delete an agent's file.
    DeleteAgent(String),
    /// Write the settings (the shell's) to disk.
    SaveConfig,
    /// Fetch a connection's model list.
    LoadModels(Box<Connection>),
    /// Ask Claude Code what a Claude account offers.
    ProbeClaude(Box<Connection>),
    /// Ask Claude Code which MCP servers (claude.ai connectors among them) a Claude account's sessions get.
    ProbeConnectors(Box<Connection>),
    /// Ask Claude Code where a Claude account's plan limits stand (its own `/usage`).
    CheckLimits(Box<Connection>),
    /// Open Claude Code's own sign-in for a Claude account, in a terminal.
    ClaudeLogin(Box<Connection>),
    /// Ask Codex what a Codex account offers.
    ProbeCodex(Box<Connection>),
    /// Open Codex's own sign-in for a Codex account, in a terminal.
    CodexLogin(Box<Connection>),
    /// Open the maker's own installer of Claude Code or Codex, in a terminal.
    Install(Cli),
    /// Search every session's messages.
    Search {
        /// The text to find.
        query: String,
        /// Counts searches, to drop a late answer.
        generation: u64,
    },
    /// Read the usage log, for the statistics.
    LoadStats,
    /// Add a response's usage to the log.
    LogUsage(Box<Record>),
    /// Keep the accounts' plan limits for the next start.
    SaveLimits(HashMap<String, Limits>),
    /// Have git do something in a folder.
    Git {
        /// The folder.
        folder: String,
        /// What to do there.
        job: git::Job,
    },
    /// Change the skill library, or read a repository of skills.
    Skills(skills::Job),
    /// Look after the sessions' worktrees: clean up, take one away, measure.
    Worktree(worktree::Job),
    /// List a folder's files, for naming one in a message.
    ListFiles(String),
    /// Ask for a folder.
    PickFolder,
    /// Ask for an image file.
    PickPicture,
    /// Ask for files to send along with a message.
    PickFiles,
    /// Take what the clipboard holds that is not text (copied files, a picture) to send along.
    PasteFiles,
    /// Start a voice call.
    StartCall {
        /// The provider, key and voice.
        config: Box<VoiceConfig>,
        /// Devices and thresholds.
        options: Box<CallOptions>,
    },
    /// Hang up.
    EndCall,
    /// More of the agent's reply, to be spoken.
    Say(String),
    /// The agent's reply is complete.
    FinishReply,
    /// Drop whatever is being or about to be spoken.
    StopSpeaking,
    /// Mute or unmute the microphone.
    Mute(bool),
    /// Switch and persist the colour scheme.
    SetTheme(Scheme),
    /// Show the data folder in the file manager.
    OpenDataDir,
    /// Open a link from a reply in the browser.
    OpenLink(String),
    /// Look for a newer version, or download one: only ever because the user clicked.
    Update(update::Job),
    /// Move the downloaded version in and restart into it.
    InstallUpdate(update::Staged),
    /// Put text on the clipboard.
    Copy(String),
    /// A turn finished or needs an answer: flash the window if it is in the background.
    Attention,
}

/// Failure to start the app.
#[derive(Debug)]
pub enum StartupError {
    /// The OS refused to create a window.
    Window(winit::error::OsError),
    /// GPU initialisation failed.
    Gpu(GpuError),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window(e) => write!(f, "cannot create the window: {e}"),
            Self::Gpu(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Window(e) => Some(e),
            Self::Gpu(e) => Some(e),
        }
    }
}

/// The running application.
pub struct App {
    window: Arc<Window>,
    /// For creating the renderer again after the GPU was lost.
    display: winit::event_loop::OwnedDisplayHandle,
    renderer: Renderer,
    /// When to try again to bring back a lost GPU.
    gpu_retry: Option<Instant>,
    /// When to draw again after a frame that could not be shown.
    redraw_at: Option<Instant>,
    fonts: Fonts,
    atlas: GlyphAtlas,
    images: ImageAtlas,
    instances: Vec<Instance>,
    proxy: EventLoopProxy<WorkerEvent>,
    clipboard: Option<Clipboard>,
    ui: Ui,
    start: Instant,
    last_frame: Instant,
    shell: Shell,
    /// Where sessions are saved; `None` when there is no home directory.
    sessions_dir: Option<PathBuf>,
    /// Where agents are saved; `None` without a home directory.
    agents: Option<Records>,
    /// Performs every file write, in order, off the UI thread.
    writer: Writer,
    /// The agent's process (Claude Code, Codex) of each session that has one.
    live: HashMap<String, Live>,
    /// The running API turn of each session that has one.
    turns: HashMap<String, Control>,
    /// The hold on the worktree of each session something works in (a
    /// turn, an agent's process): nothing cleans it up meanwhile.
    leases: HashMap<String, worktree::Lease>,
    /// The sessions whose message waits for its worktree to be ready.
    preparing: HashSet<String>,
    /// The engine (a Claude process, an API turn) each session hears from,
    /// by a number given to each: what one it replaced still says is dropped.
    generations: HashMap<String, u64>,
    /// The number the next engine gets.
    next_generation: u64,
    /// Tools approved for the rest of each API session.
    approved: HashMap<String, Arc<Mutex<HashSet<String>>>>,
    /// The skills each API session has had since its first turn here: its
    /// tool list stays as it began.
    skill_sets: HashMap<String, Arc<Vec<Skill>>>,
    /// The commands each API session's agent runs in the background: they
    /// outlive its turns, and stop with the session, `/clear` or the app.
    jobs: HashMap<String, Arc<Jobs>>,
    /// The user's MCP servers, connected for API sessions, and the settings they were made from.
    mcp: Option<(Vec<McpServer>, McpHub)>,
    /// The voice call in progress.
    call: Option<Call>,
    scheme: Scheme,
    /// Colours drawn this frame; differs from `scheme` during a cross-fade.
    palette: Palette,
    /// Palette a cross-fade started from, and its progress in `0..=1`.
    fade: Option<(Palette, f32)>,
    /// Whether the window has been made visible (after the first frame).
    shown: bool,
    /// Cursor currently set on the window.
    cursor: CursorIcon,
    /// Input-method candidate area last sent to the window.
    ime_area: Option<Rect>,
    /// Interface zoom in percent, on top of the display's scale.
    zoom: u16,
    /// The title bar's close button was clicked: the window closes as the
    /// settings say ([`CloseAction`]) once the frame is drawn.
    closing: bool,
    /// The tray icon that brings the window back (Windows only).
    tray: Option<Tray>,
    /// The window is hidden, its icon in the tray.
    hidden: bool,
    /// When the title bar strip was last pressed, to maximize on a second press.
    title_press: Option<f32>,
    /// The new version to start once the app has closed: an update was moved in.
    restart: Option<PathBuf>,
}

impl App {
    /// Creates the window and GPU state and reads what was saved.
    ///
    /// # Errors
    /// See [`StartupError`].
    pub fn new(event_loop: &ActiveEventLoop, proxy: EventLoopProxy<WorkerEvent>) -> Result<Self, StartupError> {
        let config = Config::load().unwrap_or_else(|e| {
            log::error(format!("ignoring unreadable settings: {e}"));
            Config::default()
        });
        let scheme = Scheme::from_key(config.theme.as_deref());
        let zoom = config.zoom.as_deref().and_then(|z| z.parse().ok()).map_or(100, |z: u16| z.clamp(ZOOM.0, ZOOM.1));
        let ((width, height), maximized) = window_state(config.window.as_deref());
        let attributes = Window::default_attributes()
            .with_title("AtroxaBot")
            .with_inner_size(LogicalSize::new(width, height))
            .with_min_inner_size(min_size(zoom, event_loop.primary_monitor()))
            // Windows shows a window as it maximizes it, so there it waits
            // for the first frame (see `frame`).
            .with_maximized(maximized && !cfg!(windows))
            .with_theme(Some(window_theme(scheme)))
            // Shown after the first frame so the user never sees a blank window.
            .with_visible(false)
            // The app draws its own title bar (`titlebar.rs`), in the scheme's colours.
            .with_decorations(!crate::titlebar::CUSTOM);
        // Undecorated, a window still casts the system's shadow.
        #[cfg(windows)]
        let attributes =
            winit::platform::windows::WindowAttributesExtWindows::with_undecorated_shadow(attributes, true);
        // Let the sidebar run under a transparent title bar on macOS.
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attributes.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true)
        };
        // Wayland's app id and X11's WM_CLASS: desktops match it to the
        // installed `atroxabot.desktop` for the name and icon.
        #[cfg(target_os = "linux")]
        let attributes =
            winit::platform::wayland::WindowAttributesExtWayland::with_name(attributes, "atroxabot", "atroxabot");
        let window = Arc::new(event_loop.create_window(attributes).map_err(StartupError::Window)?);
        set_icon(&window);
        // Chinese, Japanese and Korean input methods deliver text through IME events.
        window.set_ime_allowed(true);
        let display = event_loop.owned_display_handle();
        let renderer = block_on(Renderer::new(Arc::clone(&window), display.clone())).map_err(StartupError::Gpu)?;

        let sessions_dir = SessionStore::open()
            .inspect_err(|e| log::error(format!("sessions will not be saved: {e}")))
            .ok()
            .map(|store| store.dir().to_owned());
        let agents = Records::agents().inspect_err(|e| log::error(format!("agents will not be saved: {e}"))).ok();
        let reporter = proxy.clone();
        let mut writer = Writer::start(sessions_dir.clone(), move |text: String| {
            let _ = reporter.send_event(WorkerEvent::Problem(text));
        });
        // Agents are a handful of small files: read here, before the first frame.
        let (saved, unreadable) = match agents.as_ref().map(Records::list::<Agent>) {
            Some(Ok((agents, errors))) => (agents, errors.len()),
            Some(Err(e)) => {
                log::error(format!("cannot list the agents: {e}"));
                (Vec::new(), 1)
            }
            None => (Vec::new(), 0),
        };
        let (sessions, lost) = writer.list_sessions();
        let mut shell = Shell::new(config, saved.into_iter().filter(|agent| !agent.is_newer()).collect(), sessions);
        if unreadable + lost > 0 {
            shell.notify(
                "Some saved agents or sessions could not be read and are not listed. Their files are left untouched."
                    .into(),
            );
        }
        // Started now, not when first needed: another start of the app finds
        // this one by the tray's window.
        let tray_proxy = proxy.clone();
        let tray = Tray::start(move |event| {
            let _ = tray_proxy.send_event(WorkerEvent::Tray(event));
        })
        .inspect_err(|e| {
            if cfg!(windows) {
                log::error(format!("there will be no tray icon: {e}"));
            }
        })
        .ok();
        let now = Instant::now();
        let mut app = Self {
            window,
            display,
            renderer,
            gpu_retry: None,
            redraw_at: None,
            fonts: Fonts::load(),
            atlas: GlyphAtlas::default(),
            images: ImageAtlas::default(),
            instances: Vec::new(),
            proxy,
            clipboard: None,
            ui: Ui::default(),
            start: now,
            last_frame: now,
            shell,
            sessions_dir,
            agents,
            writer,
            live: HashMap::new(),
            turns: HashMap::new(),
            leases: HashMap::new(),
            preparing: HashSet::new(),
            generations: HashMap::new(),
            next_generation: 1,
            approved: HashMap::new(),
            skill_sets: HashMap::new(),
            jobs: HashMap::new(),
            mcp: None,
            call: None,
            scheme,
            palette: *scheme.palette(),
            fade: None,
            shown: false,
            cursor: CursorIcon::Default,
            ime_area: None,
            zoom,
            closing: false,
            tray,
            hidden: false,
            title_press: None,
            restart: None,
        };
        app.ui.focused = true;
        app.shell.set_icons(provider_icons());
        app.shell.set_limits(stats::load_limits());
        // What the sessions used so far starts the usage log, once.
        app.writer.send(Job::StartUsageLog);
        let mut actions = Vec::new();
        app.shell.start(&mut actions);
        app.apply(actions);
        // Draw the first frame directly: hidden windows never receive
        // `RedrawRequested` on Windows, so waiting for one would keep the
        // window invisible forever. `frame` shows it once drawn.
        app.frame();
        Ok(app)
    }

    /// Starts a cross-fade to `scheme` and persists it.
    fn set_scheme(&mut self, scheme: Scheme) {
        self.fade = Some((self.palette, 0.0));
        self.scheme = scheme;
        self.window.set_theme(Some(window_theme(scheme)));
        self.shell.config.theme = Some(scheme.key().to_owned());
        self.save_config();
    }

    /// Physical pixels per logical pixel: the display's scale, zoomed.
    fn scale(&self) -> f32 {
        self.window.scale_factor() as f32 * f32::from(self.zoom) / 100.0
    }

    /// The window's area in logical pixels.
    fn view(&self) -> Rect {
        let (scale, size) = (self.scale(), self.window.inner_size());
        Rect::new(0.0, 0.0, size.width as f32 / scale, size.height as f32 / scale)
    }

    fn save_config(&mut self) {
        self.writer.send(Job::SaveConfig(Box::new(self.shell.config.clone())));
    }

    /// Runs `job` on a worker thread and posts its result to the event loop.
    fn spawn(&self, job: impl FnOnce() -> WorkerEvent + Send + 'static) {
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("atroxabot-worker".into()).spawn(move || {
            // Fails only if the event loop is gone, i.e. the app is exiting.
            let _ = proxy.send_event(job());
        });
        if let Err(e) = spawned {
            log::error(format!("cannot spawn worker thread: {e}"));
        }
    }

    /// Something to post a session's engine events to the event loop with.
    fn emitter(&self, session: &str) -> impl FnMut(AgentEvent) + Send + 'static {
        let (proxy, session) = (self.proxy.clone(), session.to_owned());
        let generation = self.generations.get(&session).copied().unwrap_or_default();
        move |event| {
            let _ = proxy.send_event(WorkerEvent::Agent { session: session.clone(), generation, event });
        }
    }

    /// A new engine answers `session` from now on: the one it replaces (a
    /// process being closed, say) is no longer heard, so that its last words
    /// cannot end or close the new one.
    fn renew(&mut self, session: &str) {
        self.generations.insert(session.to_owned(), self.next_generation);
        self.next_generation += 1;
    }

    /// Routes a window event.
    pub fn window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        let scale = self.scale();
        let mut actions = Vec::new();
        match event {
            WindowEvent::CloseRequested => self.close(event_loop),
            WindowEvent::RedrawRequested => {
                self.frame();
                if std::mem::take(&mut self.closing) {
                    self.close(event_loop);
                }
                return;
            }
            WindowEvent::Resized(size) => {
                self.renderer.resize(size.width, size.height);
                self.window.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                // Coming back, perhaps from signing in to Claude in a terminal.
                if focused && !self.ui.focused {
                    self.shell.focused(&mut actions);
                }
                self.ui.focused = focused;
            }
            // A file dragged onto the window goes along with the next message.
            WindowEvent::DroppedFile(path) => self.shell.files_picked(vec![path.display().to_string()]),
            WindowEvent::ModifiersChanged(mods) => self.ui.mods = mods.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.ui.mouse = (position.x as f32 / scale, position.y as f32 / scale);
            }
            WindowEvent::CursorLeft { .. } => self.ui.mouse = (f32::MIN, f32::MIN),
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Pressed {
                    let now = self.start.elapsed().as_secs_f32();
                    // The title bar and the window's edges take the press
                    // themselves; the page never sees it.
                    match titlebar::grab(self.view(), self.ui.mouse, self.window.is_maximized()) {
                        Some(Grab::Resize(direction)) => {
                            let _ = self.window.drag_resize_window(direction);
                        }
                        Some(Grab::Move) if self.title_press.is_some_and(|at| now - at < 0.4) => {
                            self.title_press = None;
                            self.window.set_maximized(!self.window.is_maximized());
                        }
                        Some(Grab::Move) => {
                            self.title_press = Some(now);
                            let _ = self.window.drag_window();
                        }
                        None => self.ui.press(now),
                    }
                } else {
                    self.ui.down = false;
                    self.ui.released = true;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 100.0, y * 100.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / scale, p.y as f32 / scale),
                };
                // Shift turns a plain wheel sideways, as everywhere else.
                let (dx, dy) = if self.ui.mods.shift_key() && dx == 0.0 { (dy, 0.0) } else { (dx, dy) };
                self.ui.scroll -= dy;
                self.ui.scroll_x -= dx;
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if event.state == ElementState::Pressed => {
                let primary =
                    if cfg!(target_os = "macos") { self.ui.mods.super_key() } else { self.ui.mods.control_key() };
                // Quitting when closing does not: Cmd+Q is the macOS menu's own.
                if primary && !cfg!(target_os = "macos") && is_key(&event.logical_key, "q") {
                    event_loop.exit();
                    return;
                }
                if primary && let Some(zoom) = zoomed(self.zoom, &event.logical_key) {
                    if zoom != self.zoom {
                        self.zoom = zoom;
                        // The smallest window holds the same layout at any zoom.
                        self.window.set_min_inner_size(Some(min_size(zoom, self.window.current_monitor())));
                        // Resent next frame at the new zoom.
                        self.ime_area = None;
                        self.shell.config.zoom = (zoom != 100).then(|| zoom.to_string());
                        self.save_config();
                    }
                    self.window.request_redraw();
                    return;
                }
                self.shell.key(&event, self.ui.mods, &mut self.clipboard, &mut actions);
                self.ui.last_edit = self.ui.time;
            }
            WindowEvent::Ime(ime) => {
                match ime {
                    Ime::Commit(text) => self.shell.ime_commit(&text, &mut actions),
                    Ime::Preedit(text, cursor) => self.shell.ime_preedit(text, cursor),
                    Ime::Disabled => self.shell.ime_preedit(String::new(), None),
                    Ime::Enabled => return,
                }
                self.ui.last_edit = self.ui.time;
            }
            _ => return,
        }
        self.apply(actions);
        self.window.request_redraw();
    }

    /// Closes the window as the settings say: into the tray (or behind the
    /// Dock icon), minimised, or by quitting.
    fn close(&mut self, event_loop: &ActiveEventLoop) {
        match CloseAction::from_key(self.shell.config.close.as_deref()) {
            CloseAction::Quit => event_loop.exit(),
            CloseAction::Minimize => self.window.set_minimized(true),
            CloseAction::Background => self.go_to_background(event_loop),
        }
    }

    /// Takes the window off the screen while the app keeps running.
    #[cfg_attr(target_os = "macos", allow(clippy::unused_self, reason = "macOS hides the whole app"))]
    fn go_to_background(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::ActiveEventLoopExtMacOS;
            event_loop.hide_application();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = event_loop;
            match &self.tray {
                Some(tray) => {
                    tray.show();
                    self.window.set_visible(false);
                    self.hidden = true;
                }
                // Without a tray icon a hidden window could not be found again.
                None => self.window.set_minimized(true),
            }
        }
    }

    /// Brings the window back to the front, from the tray or minimised.
    fn restore(&mut self) {
        if std::mem::take(&mut self.hidden) {
            self.window.set_visible(true);
            if let Some(tray) = &self.tray {
                tray.hide();
            }
        }
        if self.window.is_minimized() == Some(true) {
            self.window.set_minimized(false);
        }
        self.window.focus_window();
        self.window.request_redraw();
    }

    /// The new version to start now that the app closes, if an update was moved in.
    pub fn take_restart(&mut self) -> Option<PathBuf> {
        self.restart.take()
    }

    /// Applies a result from another thread.
    pub fn worker_event(&mut self, event_loop: &ActiveEventLoop, event: WorkerEvent) {
        let mut actions = Vec::new();
        match event {
            WorkerEvent::Tray(TrayEvent::Open) => self.restore(),
            WorkerEvent::Tray(TrayEvent::Quit) | WorkerEvent::Quit => event_loop.exit(),
            WorkerEvent::Update(report) => self.shell.update_report(report),
            WorkerEvent::Thumbnail(key, pixels) => self.images.insert(key, pixels),
            // From an engine that was replaced: nothing it says matters any more.
            WorkerEvent::Agent { session, generation, .. } if self.generations.get(&session) != Some(&generation) => {}
            WorkerEvent::Agent { session, event, .. } => {
                if matches!(event, AgentEvent::TurnEnded { .. }) {
                    self.turns.remove(&session);
                    // A turn through an API is all that worked in its worktree; a process stays in it.
                    if !self.live.contains_key(&session) {
                        self.leases.remove(&session);
                    }
                }
                self.shell.agent_event(&session, event, &mut actions);
            }
            WorkerEvent::Models { connection, result } => {
                if let Err(e) = &result {
                    log::error(format!("could not list models: {e}"));
                }
                self.shell.models_loaded(&connection, result);
            }
            WorkerEvent::SessionLoaded { session, result } => {
                self.shell.session_loaded(&session, result.map(|s| *s), &mut actions);
            }
            WorkerEvent::Call(event) => {
                if matches!(event, CallEvent::Ended(_)) {
                    self.hang_up();
                }
                self.shell.call_event(event, &mut actions);
            }
            WorkerEvent::FolderPicked(folder) => self.shell.folder_picked(folder, &mut actions),
            WorkerEvent::PicturePicked(picture) => self.shell.picture_picked(picture),
            WorkerEvent::FilesPicked(files) => self.shell.files_picked(files),
            WorkerEvent::ClaudeInfo { connection, result } => {
                if let Err(e) = &result {
                    log::error(format!("Claude Code could not be asked about an account: {e}"));
                }
                self.shell.claude_info(&connection, result);
            }
            WorkerEvent::CodexInfo { connection, result } => {
                if let Err(e) = &result {
                    log::error(format!("Codex could not be asked about an account: {e}"));
                }
                self.shell.codex_info(&connection, result, &mut actions);
            }
            WorkerEvent::LimitsChecked { connection, result } => {
                match &result {
                    Err(e) => log::error(format!("Claude Code could not say where a plan's limits stand: {e}")),
                    Ok(PlanCheck::Unavailable) => {
                        log::error("Claude Code could not get a plan's limits from Anthropic");
                    }
                    Ok(_) => {}
                }
                self.shell.limits_checked(&connection, result, &mut actions);
            }
            WorkerEvent::Connectors { connection, result } => {
                if let Err(e) = &result {
                    log::error(format!("Claude Code could not say which MCP servers an account has: {e}"));
                }
                self.shell.connectors_loaded(&connection, result);
            }
            WorkerEvent::SearchResults { generation, hits } => self.shell.search_results(generation, hits),
            WorkerEvent::Stats(ledger) => self.shell.stats_loaded(*ledger),
            WorkerEvent::Git { folder, report } => self.shell.git_report(&folder, report),
            WorkerEvent::Files { folder, files } => self.shell.files_listed(&folder, files),
            WorkerEvent::Skills(report) => self.shell.skills_report(report),
            WorkerEvent::Worktree(report) => self.shell.worktree_report(report, &mut actions),
            WorkerEvent::WorktreeReady { pending, generation, prepared } => {
                let Pending { agent, connection, model, mut session, text } = *pending;
                let id = session.id.clone();
                // Stopped or deleted meanwhile: the message no longer goes. One sent since then is
                // waiting for a worktree of its own asking, and is not this one's to answer for.
                let wanted = self.generations.get(&id) == Some(&generation) && self.preparing.remove(&id);
                match prepared {
                    Ok(prepared) => {
                        // Made is made: the session keeps it either way, so it is found again and cleaned up.
                        self.shell.worktree_ready(&id, &prepared.worktree, &prepared.workdir, &mut actions);
                        if wanted {
                            session.worktree = Some(prepared.worktree);
                            session.workdir = prepared.workdir;
                            self.leases.insert(id, prepared.lease);
                            self.start_turn(&agent, &connection, model, session, &text);
                        }
                    }
                    Err(problem) if wanted => {
                        fail(self.emitter(&id), format!("Its worktree could not be made ready: {problem}"));
                    }
                    Err(_) => {}
                }
            }
            WorkerEvent::Problem(text) => self.shell.notify(text),
        }
        self.apply(actions);
        self.window.request_redraw();
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.run(action);
        }
    }

    fn run(&mut self, action: Action) {
        match action {
            Action::Send { agent, connection, model, session, text } => {
                self.send(&agent, &connection, model, *session, &text);
            }
            Action::Compact { agent, connection, model, session } => self.compact(&agent, &connection, model, *session),
            Action::StopJobs(session) => self.stop_jobs(&session),
            Action::Stop(session) => {
                // Not started yet: its worktree is still being made ready.
                if self.preparing.remove(&session) {
                    self.emitter(&session)(AgentEvent::TurnEnded { problem: None });
                }
                if let Some(turn) = self.turns.get(&session) {
                    turn.stop();
                }
                if let Some(live) = self.live.get(&session) {
                    live.interrupt(true);
                }
            }
            Action::Interrupt(session) => {
                if self.preparing.remove(&session) {
                    self.emitter(&session)(AgentEvent::TurnEnded { problem: None });
                }
                // An API turn ends; the shell sends what it had not read as the next message.
                if let Some(turn) = self.turns.get(&session) {
                    turn.stop();
                }
                if let Some(live) = self.live.get(&session) {
                    live.interrupt(false);
                }
            }
            Action::FollowUp { session, message } => {
                if let Some(live) = self.live.get(&session) {
                    live.follow_up(*message);
                } else if let Some(turn) = self.turns.get(&session) {
                    turn.follow_up(*message);
                }
            }
            Action::Decide { session, ask, verdict } => {
                if let Some(turn) = self.turns.get(&session) {
                    turn.decide(&ask, verdict);
                } else if let Some(live) = self.live.get(&session) {
                    live.decide(&ask, verdict);
                }
            }
            Action::CloseLive(session) => {
                if let Some(live) = self.live.remove(&session) {
                    // Its worktree is let go once the process has ended.
                    retire(live, self.leases.remove(&session));
                }
            }
            Action::AskContext(session) => {
                if let Some(live) = self.live.get(&session) {
                    live.ask_context();
                }
            }
            Action::LoadSession(session) => {
                if let Some(store) = self.sessions_dir.clone().map(SessionStore::at) {
                    self.spawn(move || WorkerEvent::SessionLoaded {
                        result: store.load(&session).map(Box::new),
                        session,
                    });
                }
            }
            Action::SaveSession(session) => self.writer.send(Job::SaveSession(session)),
            Action::DeleteSession(session) => {
                self.preparing.remove(&session);
                self.approved.remove(&session);
                self.skill_sets.remove(&session);
                self.stop_jobs(&session);
                self.writer.send(Job::DeleteSession(session));
            }
            Action::SaveAgent(agent) => {
                if let Some(agents) = &self.agents {
                    self.writer.send(Job::SaveAgent(agents.clone(), agent));
                }
            }
            Action::DeleteAgent(agent) => {
                if let Some(agents) = &self.agents {
                    self.writer.send(Job::DeleteAgent(agents.clone(), agent));
                }
            }
            Action::SaveConfig => self.save_config(),
            Action::LoadModels(connection) => self.spawn(move || {
                let client = client_for(&connection);
                WorkerEvent::Models { connection: connection.id.clone(), result: client.models() }
            }),
            Action::ProbeClaude(connection) => self.spawn(move || WorkerEvent::ClaudeInfo {
                result: atroxabot_core::claude_info(&connection),
                connection: connection.id.clone(),
            }),
            Action::ProbeConnectors(connection) => self.spawn(move || WorkerEvent::Connectors {
                result: atroxabot_core::claude_connectors(&connection),
                connection: connection.id.clone(),
            }),
            Action::CheckLimits(connection) => self.spawn(move || WorkerEvent::LimitsChecked {
                result: atroxabot_core::claude_usage(&connection),
                connection: connection.id.clone(),
            }),
            Action::ClaudeLogin(connection) => {
                // Claude Code's own sign-in, in a terminal the user sees: the app only starts it.
                let started = atroxabot_core::claude_login(&connection)
                    .ok_or_else(|| "Claude Code is not installed.".to_owned())
                    .and_then(|login| in_terminal("Sign in to Claude", &login));
                if let Err(e) = started {
                    self.shell.notify(format!("The sign-in could not be opened: {e}"));
                }
            }
            Action::ProbeCodex(connection) => self.spawn(move || WorkerEvent::CodexInfo {
                result: atroxabot_core::codex_info(&connection),
                connection: connection.id.clone(),
            }),
            Action::CodexLogin(connection) => {
                // Codex's own sign-in, in a terminal the user sees: the app only starts it.
                let started = atroxabot_core::codex_login(&connection)
                    .ok_or_else(|| "Codex is not installed.".to_owned())
                    .and_then(|login| in_terminal("Sign in to Codex", &login));
                if let Err(e) = started {
                    self.shell.notify(format!("The sign-in could not be opened: {e}"));
                }
            }
            Action::Install(cli) => {
                // The maker's own installer, in a terminal the user sees and answers: the app only starts it.
                let (title, installer) = match cli {
                    Cli::Claude => ("Install Claude Code", atroxabot_core::claude_install()),
                    Cli::Codex => ("Install Codex", atroxabot_core::codex_install()),
                };
                match in_terminal(title, &installer) {
                    Ok(()) => self.shell.installing(cli),
                    Err(e) => self.shell.notify(format!("The installer could not be opened: {e}")),
                }
            }
            Action::Search { query, generation } => {
                if let Some(store) = self.sessions_dir.clone().map(SessionStore::at) {
                    self.spawn(move || WorkerEvent::SearchResults {
                        generation,
                        hits: store.search(&query, 30).unwrap_or_default(),
                    });
                }
            }
            Action::LoadStats => {
                let store = self.sessions_dir.clone().map(SessionStore::at);
                self.spawn(move || {
                    // The log; before there is one, what the sessions still say.
                    let logged = Ledger::path().ok().and_then(|path| Ledger::read(&path));
                    let sessions = || store.map(|store| Ledger::collect(&store.all())).unwrap_or_default();
                    WorkerEvent::Stats(Box::new(logged.unwrap_or_else(sessions)))
                });
            }
            Action::LogUsage(record) => self.writer.send(Job::LogUsage(record)),
            Action::SaveLimits(limits) => self.writer.send(Job::SaveLimits(limits)),
            Action::Git { folder, job } => {
                self.spawn(move || WorkerEvent::Git { report: git::run(std::path::Path::new(&folder), job), folder });
            }
            Action::Skills(job) => self.spawn(move || WorkerEvent::Skills(skills::run(job))),
            Action::Worktree(job) => self.spawn(move || WorkerEvent::Worktree(worktree::run(job))),
            Action::ListFiles(folder) => {
                self.spawn(move || WorkerEvent::Files { files: git::files(std::path::Path::new(&folder)), folder });
            }
            Action::PickFolder => {
                self.spawn(|| match platform::pick_folder("Choose the folder this session works in") {
                    Ok(folder) => WorkerEvent::FolderPicked(folder.map(|path| path.display().to_string())),
                    Err(e) => WorkerEvent::Problem(format!("Could not open the folder picker: {e}")),
                });
            }
            Action::PickFiles => self.spawn(|| match platform::pick_any("Choose files to send along") {
                Ok(files) => WorkerEvent::FilesPicked(files.iter().map(|path| path.display().to_string()).collect()),
                Err(e) => WorkerEvent::Problem(format!("Could not open the file picker: {e}")),
            }),
            Action::PasteFiles => self.spawn(|| {
                // A pasted picture becomes a file of the app's own.
                let saved = Config::dir().map_err(|e| e.to_string()).and_then(|dir| {
                    let dir = dir.join("attachments");
                    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                    platform::clipboard_files(&dir.join(format!("{}.png", atroxabot_core::new_id())))
                        .map_err(|e| e.to_string())
                });
                match saved {
                    Ok(files) => {
                        WorkerEvent::FilesPicked(files.iter().map(|path| path.display().to_string()).collect())
                    }
                    Err(e) => WorkerEvent::Problem(format!("Could not read the clipboard: {e}")),
                }
            }),
            Action::PickPicture => self.spawn(|| match platform::pick_image() {
                Ok(picture) => WorkerEvent::PicturePicked(picture.map(|path| path.display().to_string())),
                Err(e) => WorkerEvent::Problem(format!("Could not open the file picker: {e}")),
            }),
            Action::StartCall { config, options } => {
                self.hang_up();
                let proxy = self.proxy.clone();
                let started = Call::start(*config, *options, move |event| {
                    let _ = proxy.send_event(WorkerEvent::Call(event));
                });
                match started {
                    Ok(call) => self.call = Some(call),
                    Err(e) => {
                        let _ = self.proxy.send_event(WorkerEvent::Call(CallEvent::Ended(Some(e.to_string()))));
                    }
                }
            }
            Action::EndCall => self.hang_up(),
            Action::Say(text) => {
                if let Some(call) = &self.call {
                    call.say(&text);
                }
            }
            Action::FinishReply => {
                if let Some(call) = &self.call {
                    call.finish_reply();
                }
            }
            Action::StopSpeaking => {
                if let Some(call) = &self.call {
                    call.stop_speaking();
                }
            }
            Action::Mute(muted) => {
                if let Some(call) = &self.call {
                    call.set_muted(muted);
                }
            }
            Action::SetTheme(scheme) => self.set_scheme(scheme),
            Action::OpenDataDir => {
                if let Ok(dir) = Config::dir() {
                    // The folder may not exist before the first save.
                    let _ = std::fs::create_dir_all(&dir);
                    if let Err(e) = platform::open_folder(&dir) {
                        log::error(format!("cannot open the data folder: {e}"));
                    }
                }
            }
            Action::OpenLink(url) => {
                // Only web and mail links: a reply must never launch local programs.
                let safe = ["https://", "http://", "mailto:"].iter().any(|scheme| url.starts_with(scheme));
                if safe && let Err(e) = platform::open_url(&url) {
                    log::error(format!("cannot open link: {e}"));
                }
            }
            Action::Update(job) => {
                let proxy = self.proxy.clone();
                self.spawn(move || {
                    let said = |report| {
                        let _ = proxy.send_event(WorkerEvent::Update(report));
                    };
                    WorkerEvent::Update(update::run(job, &said))
                });
            }
            Action::InstallUpdate(staged) => match staged.install() {
                // Closing as on Quit, so everything is saved; the new version starts after.
                Ok(start) => {
                    self.restart = Some(start);
                    let _ = self.proxy.send_event(WorkerEvent::Quit);
                }
                Err(why) => self.shell.update_failed(why),
            },
            Action::Copy(text) => copy(&mut self.clipboard, &text),
            Action::Attention if !self.ui.focused => {
                self.window.request_user_attention(Some(UserAttentionType::Informational));
            }
            Action::Attention => {}
        }
    }

    /// Ends the voice call, if there is one. Its devices are let go on
    /// another thread, which may take a moment.
    fn hang_up(&mut self) {
        if let Some(call) = self.call.take() {
            let _ = std::thread::Builder::new().name("atroxabot-hangup".into()).spawn(move || call.end());
        }
    }

    /// The connections to the user's MCP servers as the settings have them
    /// now: made again when the settings changed, the old ones stopped.
    fn mcp_hub(&mut self) -> Option<McpHub> {
        let servers = &self.shell.config.mcp;
        if self.mcp.as_ref().is_none_or(|(made_from, _)| made_from != servers) {
            if let Some((_, old)) = self.mcp.take() {
                // Stopping servers waits for them: not on this thread.
                let _ = std::thread::Builder::new().name("atroxabot-mcp-close".into()).spawn(move || old.close());
            }
            self.mcp = McpHub::new(servers).map(|hub| (servers.clone(), hub));
        }
        self.mcp.as_ref().map(|(_, hub)| hub.clone())
    }

    /// Answers the latest prompt of `session`. One that works in a
    /// worktree waits for it to be there first: made on its first message,
    /// made again if it was cleaned up since.
    fn send(
        &mut self,
        agent: &Agent,
        connection: &Connection,
        model: Option<Box<Model>>,
        session: Session,
        text: &str,
    ) {
        // With a hold on it already (its agent's process is up), it is there.
        if session.worktree.is_none() || self.leases.contains_key(&session.id) {
            self.start_turn(agent, connection, model, session, text);
            return;
        }
        let id = session.id.clone();
        self.renew(&id);
        self.preparing.insert(id.clone());
        self.emitter(&id)(AgentEvent::Status("Getting its worktree ready…".to_owned()));
        let generation = self.generations.get(&id).copied().unwrap_or_default();
        let pending = Box::new(Pending {
            agent: agent.clone(),
            connection: connection.clone(),
            model,
            session,
            text: text.to_owned(),
        });
        self.spawn(move || {
            let prepared = worktree::prepare(&pending.session);
            WorkerEvent::WorktreeReady { pending, generation, prepared }
        });
    }

    /// Starts the turn answering the latest prompt of `session`.
    fn start_turn(
        &mut self,
        agent: &Agent,
        connection: &Connection,
        model: Option<Box<Model>>,
        session: Session,
        text: &str,
    ) {
        let id = session.id.clone();
        // What the latest prompt sends along.
        let files = match session.entries.last() {
            Some(atroxabot_core::Entry::User { files, .. }) => files.clone(),
            _ => Vec::new(),
        };
        if connection.kind.is_account() {
            if !self.live.contains_key(&id) {
                self.renew(&id);
            }
            let mut emit = self.emitter(&id);
            let codex = connection.kind == ConnectionKind::Codex;
            // One process serves the session until it is closed or its agent changes.
            let servers = &self.shell.config.mcp;
            let sent = if let Some(live) = self.live.get(&id) {
                live.send(text, &files)
            } else {
                let started = if codex {
                    CodexAgent::connect(agent, connection, &session, servers, self.emitter(&id))
                        .map(Live::Codex)
                        .map_err(|e| e.to_string())
                } else {
                    let mcp = atroxabot_core::claude_mcp_config(servers);
                    ClaudeAgent::connect(agent, connection, &session, mcp, self.shell.suggesting(), self.emitter(&id))
                        .map(Live::Claude)
                        .map_err(|e| e.to_string())
                };
                started.and_then(|live| {
                    let sent = live.send(text, &files);
                    self.live.insert(id.clone(), live);
                    sent
                })
            };
            if let Err(e) = sent {
                if let Some(live) = self.live.remove(&id) {
                    retire(live, self.leases.remove(&id));
                }
                let who = if codex { "Codex" } else { "The Claude Agent" };
                fail(&mut emit, format!("{who} could not be reached: {e}"));
            }
            return;
        }
        let engine = self.api_engine(agent, connection, model, &session);
        let (control, inbox) = Control::new();
        self.turns.insert(id.clone(), control);
        self.renew(&id);
        let mut emit = self.emitter(&id);
        let spawned = std::thread::Builder::new().name("atroxabot-turn".into()).spawn(move || {
            let mut session = session;
            engine.run(&mut session, &inbox, &mut emit);
        });
        if let Err(e) = spawned {
            self.turns.remove(&id);
            fail(self.emitter(&id), format!("The turn could not be started: {e}"));
        }
    }

    /// The engine that answers an API session: with the system prompt the
    /// session was started with (rendered now, at its first turn), the
    /// skills it began with, its background commands and its approvals.
    fn api_engine(
        &mut self,
        agent: &Agent,
        connection: &Connection,
        model: Option<Box<Model>>,
        session: &Session,
    ) -> ApiEngine {
        let id = session.id.clone();
        let library = Library::open().ok();
        let skill_set = self
            .skill_sets
            .entry(id.clone())
            .or_insert_with(|| Arc::new(library.as_ref().map(Library::skills).unwrap_or_default()));
        let jobs = Arc::clone(self.jobs.entry(id.clone()).or_default());
        let mut workspace = Workspace::new(&session.folder(), agent.memory_dir().as_deref()).with_jobs(jobs);
        if let Some(library) = &library {
            workspace = workspace.with_skills(&library.skills_dir(), Arc::clone(skill_set));
        }
        // A model the provider lists without pictures among its inputs is not shown any.
        workspace.blind = session.blind
            || model.as_ref().is_some_and(|model| {
                !model.input_types.is_empty() && !model.input_types.iter().any(|kind| kind == "image")
            });
        let system = if session.system.is_empty() {
            agent.api_system_prompt(&session.folder(), &session.worktree_note())
        } else {
            session.system.clone()
        };
        ApiEngine {
            client: client_for(connection),
            model: agent.model.clone(),
            effort: Some(agent.effort.clone()).filter(|effort| !effort.is_empty()),
            system,
            workspace,
            autonomy: agent.autonomy,
            // Anthropic's own web tools; other providers have none to offer here.
            web: connection.kind == ConnectionKind::ClaudeApi,
            pricing: model.map(|model| *model),
            approved: Arc::clone(self.approved.entry(id).or_default()),
            mcp: self.mcp_hub(),
        }
    }

    /// `/compact` for an API session: its conversation is summarised now,
    /// as a turn of its own that adds no message.
    fn compact(&mut self, agent: &Agent, connection: &Connection, model: Option<Box<Model>>, session: Session) {
        let id = session.id.clone();
        let engine = self.api_engine(agent, connection, model, &session);
        let (control, inbox) = Control::new();
        self.turns.insert(id.clone(), control);
        self.renew(&id);
        let mut emit = self.emitter(&id);
        let spawned = std::thread::Builder::new().name("atroxabot-compact".into()).spawn(move || {
            let mut session = session;
            engine.compact_now(&mut session, &inbox, &mut emit);
        });
        if let Err(e) = spawned {
            self.turns.remove(&id);
            fail(self.emitter(&id), format!("The summary could not be started: {e}"));
        }
    }

    /// Stops the commands `session`'s agent runs in the background; ending
    /// them waits for them, so not on this thread.
    fn stop_jobs(&mut self, session: &str) {
        if let Some(jobs) = self.jobs.remove(session) {
            let _ = std::thread::Builder::new().name("atroxabot-jobs".into()).spawn(move || jobs.stop_all());
        }
    }

    /// Creates the renderer again after the GPU was lost (a driver reset or
    /// update, a GPU switch), with fresh atlases: theirs lived on the old
    /// device. Tries again every [`GPU_RETRY`] while it fails.
    fn recover_gpu(&mut self) {
        if self.gpu_retry.is_some_and(|at| at > Instant::now()) {
            return;
        }
        match block_on(Renderer::new(Arc::clone(&self.window), self.display.clone())) {
            Ok(renderer) => {
                self.renderer = renderer;
                self.atlas.clear();
                self.images = ImageAtlas::default();
                self.gpu_retry = None;
                let size = self.window.inner_size();
                self.renderer.resize(size.width, size.height);
            }
            Err(e) => {
                log::error(format!("cannot restore the GPU: {e}"));
                self.gpu_retry = Some(Instant::now() + GPU_RETRY);
            }
        }
    }

    /// Builds and presents one frame.
    fn frame(&mut self) {
        if self.renderer.is_lost() {
            self.recover_gpu();
        }
        let now = Instant::now();
        self.redraw_at = None;
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        self.ui.begin(dt, (now - self.start).as_secs_f32());

        // Cross-fade between colour schemes with a smoothstep ease.
        let target = self.scheme.palette();
        self.palette = match &mut self.fade {
            Some((from, progress)) if *progress < 1.0 => {
                *progress = (*progress + dt / THEME_FADE_SECS).min(1.0);
                let eased = *progress * *progress * (3.0 - 2.0 * *progress);
                self.ui.animating = true;
                from.mix(target, eased)
            }
            _ => {
                self.fade = None;
                *target
            }
        };

        let scale = self.scale();
        let view = self.view();
        let maximized = self.window.is_maximized();
        let mut actions = Vec::new();
        // What is due is done before it is drawn (a busy app never sleeps long enough for its timer).
        self.shell.tick(&mut actions);
        let mut command = None;
        // A second pass runs only if the glyph atlas overflowed mid-frame.
        for _ in 0..2 {
            let mut painter = Painter::new(
                &self.fonts,
                (&mut self.atlas, &mut self.images),
                &mut self.instances,
                scale,
                view,
                &self.palette,
            );
            self.shell.draw(&mut painter, &mut self.ui, view, self.scheme, &mut actions);
            command = command.or(titlebar::draw(&mut painter, &mut self.ui, view, maximized));
            if !painter.atlas_full {
                break;
            }
            // The first pass already acted on this frame's input; the redraw
            // must not act on it twice.
            self.atlas.clear();
            self.ui.end();
        }

        match command {
            Some(Command::Minimize) => self.window.set_minimized(true),
            Some(Command::Maximize) => self.window.set_maximized(!maximized),
            Some(Command::Close) => self.closing = true,
            None => {}
        }
        let drawn = self.renderer.render(
            &self.instances,
            &mut self.atlas.uploads,
            &mut self.images.uploads,
            scale,
            self.palette.panel,
        );
        self.ui.end();
        if drawn == Frame::Retry {
            // The surface was set up again: draw this frame onto it.
            self.window.request_redraw();
        }
        if !self.shown {
            self.shown = true;
            // Windows shows a window as it maximizes it: only now, drawn.
            if cfg!(windows) && window_state(self.shell.config.window.as_deref()).1 {
                self.window.set_maximized(true);
            }
            self.window.set_visible(true);
            // Some platforms refuse to present to a hidden surface; draw
            // again now that the window is visible.
            self.window.request_redraw();
        }
        if self.cursor != self.ui.cursor {
            self.cursor = self.ui.cursor;
            self.window.set_cursor(self.cursor);
        }
        // Keep the input method's candidate window next to the caret.
        let area = self.shell.ime_area();
        if area != self.ime_area {
            self.ime_area = area;
            if let Some(r) = area {
                let z = f32::from(self.zoom) / 100.0;
                self.window
                    .set_ime_cursor_area(LogicalPosition::new(r.x * z, r.y * z), LogicalSize::new(r.w * z, r.h * z));
            }
        }
        self.apply(actions);
        // One worker per frame's misses, decoding them in turn, so a page
        // of images never decodes dozens at once.
        let mut wanted = self.images.take_wanted();
        if let Some(last) = wanted.pop() {
            let proxy = self.proxy.clone();
            self.spawn(move || {
                for key in wanted {
                    let pixels = image::thumbnail(&key);
                    let _ = proxy.send_event(WorkerEvent::Thumbnail(key, pixels));
                }
                let pixels = image::thumbnail(&last);
                WorkerEvent::Thumbnail(last, pixels)
            });
        }
        if self.ui.animating || self.shell.is_busy() {
            if drawn == Frame::Skipped {
                self.redraw_at = Some(Instant::now() + SKIPPED_RETRY);
            } else {
                self.window.request_redraw();
            }
        }
    }

    /// How long the event loop may sleep: until the caret blinks next, a
    /// tooltip shows, or something the shell waits for is due.
    pub fn control_flow(&self) -> ControlFlow {
        let blink = self.ui.focused.then(|| self.last_frame + Duration::from_secs_f32(self.ui.next_blink().max(0.01)));
        // A tooltip shows once the mouse has rested a moment, with nothing else moving.
        let tip = self.ui.next_tip().map(|left| self.last_frame + Duration::from_secs_f32(left.max(0.01)));
        // A session going on after a usage limit, or a limit's notice going: the clock is looked at once a
        // minute meanwhile (a sleeping computer stops the event loop's own), and what counts down is redrawn.
        let due = self.shell.next_wake().map(|at| {
            let left = at.saturating_sub(atroxabot_core::unix_now()).clamp(1, 60);
            Instant::now() + Duration::from_secs(left)
        });
        match blink.into_iter().chain(tip).chain(self.gpu_retry).chain(self.redraw_at).chain(due).min() {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        }
    }

    /// A timer fired: does what the shell has due, and redraws for the caret.
    pub fn wake(&mut self) {
        let mut actions = Vec::new();
        self.shell.tick(&mut actions);
        self.apply(actions);
        self.window.request_redraw();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.hang_up();
        // The agents' processes end with the app.
        for (_, live) in self.live.drain() {
            live.close();
        }
        for turn in self.turns.values() {
            turn.stop();
        }
        // So do the commands API agents left running, even those a turn still holds.
        for (_, jobs) in self.jobs.drain() {
            jobs.stop_all();
        }
        if let Some((_, hub)) = self.mcp.take() {
            hub.close();
        }
        // The next start opens the window as it was closed. A minimized
        // window tells nothing, and a maximized one keeps its normal size.
        if self.window.is_minimized() == Some(true) {
            return;
        }
        // macOS's green button makes it fullscreen: as good as maximized.
        let maximized = self.window.is_maximized() || self.window.fullscreen().is_some();
        let (mut size, _) = window_state(self.shell.config.window.as_deref());
        if !maximized {
            let logical = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
            size = (logical.width.round() as u32, logical.height.round() as u32);
        }
        let state = format!("{}x{}{}", size.0, size.1, if maximized { " maximized" } else { "" });
        if self.shell.config.window.as_deref() != Some(state.as_str()) {
            self.shell.config.window = Some(state);
            // The writer, dropped after this, finishes the write.
            self.save_config();
        }
    }
}

/// The user's own pictures of providers: the image files in the data
/// folder's `icons`, by their names without the extension, in lower case.
fn provider_icons() -> HashMap<String, String> {
    let Ok(dir) = Config::dir().map(|dir| dir.join("icons")) else { return HashMap::new() };
    let files = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path());
    let pictures = files.filter(|path| {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
        ["png", "jpg", "jpeg", "webp"].contains(&extension.as_str())
    });
    pictures.filter_map(|path| Some((path.file_stem()?.to_str()?.to_lowercase(), path.display().to_string()))).collect()
}

/// The agent's process of a session: the Claude Code or the Codex the user installed.
enum Live {
    Claude(ClaudeAgent),
    Codex(CodexAgent),
}

impl Live {
    /// Sends the user's message, with the files that go with it.
    fn send(&self, text: &str, files: &[String]) -> Result<(), String> {
        match self {
            Self::Claude(agent) => agent.send(text, files).map_err(|e| e.to_string()),
            Self::Codex(agent) => agent.send(text, files).map_err(|e| e.to_string()),
        }
    }

    /// Hands it a message the user wrote while it works.
    fn follow_up(&self, message: FollowUp) {
        // Fails only if the process is gone, which ends the turn.
        match self {
            Self::Claude(agent) => drop(agent.follow_up(message)),
            Self::Codex(agent) => drop(agent.follow_up(message)),
        }
    }

    /// Stops the turn in progress; what was handed over and not read yet is
    /// dropped when `cancel_queued`. (Codex keeps none: the app sends it again.)
    fn interrupt(&self, cancel_queued: bool) {
        match self {
            Self::Claude(agent) => agent.interrupt(cancel_queued),
            Self::Codex(agent) => agent.interrupt(),
        }
    }

    /// Answers the approval `id`.
    fn decide(&self, id: &str, verdict: Verdict) {
        match self {
            Self::Claude(agent) => agent.decide(id, verdict),
            Self::Codex(agent) => agent.decide(id, verdict),
        }
    }

    /// Asks what the context holds, of an agent that can say.
    fn ask_context(&self) {
        if let Self::Claude(agent) = self {
            agent.ask_context();
        }
    }

    /// Ends the process, waiting for it.
    fn close(&self) {
        match self {
            Self::Claude(agent) => agent.close(),
            Self::Codex(agent) => agent.close(),
        }
    }
}

/// Lets an agent's process go on another thread: closing it waits for
/// the process, which the UI must not. The hold on the worktree it worked
/// in, if any, ends once it has.
/// Runs `command` in a terminal the user sees, its window named `title`.
fn in_terminal(title: &str, command: &atroxabot_core::TerminalCommand) -> Result<(), String> {
    platform::run_in_terminal(title, &command.program, &command.args, &command.env).map_err(|e| e.to_string())
}

fn retire(live: Live, lease: Option<worktree::Lease>) {
    let _ = std::thread::Builder::new().name("atroxabot-retire".into()).spawn(move || {
        live.close();
        drop(lease);
    });
}

/// Ends a turn that could not start, with `problem` as the reply the user reads.
fn fail(mut emit: impl FnMut(AgentEvent), problem: String) {
    emit(AgentEvent::ReplyStarted);
    emit(AgentEvent::ReplyEnded {
        raw: Vec::new(),
        wire: atroxabot_core::Wire::default(),
        model: String::new(),
        usage: atroxabot_core::Usage::default(),
        cost: 0.0,
        problem: problem.clone(),
    });
    emit(AgentEvent::TurnEnded { problem: Some(problem) });
}

/// The client for an API connection: Messages for a Claude API one
/// (whatever its address), and what the address says otherwise.
fn client_for(connection: &Connection) -> Client {
    let client = Client::new(&connection.base_url, connection.api_key.clone());
    if connection.kind == ConnectionKind::ClaudeApi { client.speaking_messages() } else { client }
}

/// The zoom after the primary modifier plus `key`, or `None` if it is not
/// a zoom key: `+` (or `=`, unshifted on many layouts) in, `-` out, `0` back
/// to 100%.
fn zoomed(zoom: u16, key: &Key) -> Option<u16> {
    let Key::Character(c) = key else { return None };
    let zoom = match c.as_str() {
        "+" | "=" => zoom.saturating_add(ZOOM_STEP),
        "-" => zoom.saturating_sub(ZOOM_STEP),
        "0" => 100,
        _ => return None,
    };
    Some(zoom.clamp(ZOOM.0, ZOOM.1))
}

/// Whether `key` is the letter `letter`, in either case.
fn is_key(key: &Key, letter: &str) -> bool {
    matches!(key, Key::Character(c) if c.eq_ignore_ascii_case(letter))
}

/// The smallest the window may be at `zoom` (percent): the interface is
/// drawn larger, so the window is too, and the layout fits as at 100%;
/// but never more than most of `screen`, so a zoomed window still fits.
fn min_size(zoom: u16, screen: Option<winit::monitor::MonitorHandle>) -> LogicalSize<f64> {
    let z = f64::from(zoom) / 100.0;
    let (mut w, mut h) = (f64::from(MIN_WINDOW_SIZE.0) * z, f64::from(MIN_WINDOW_SIZE.1) * z);
    if let Some(screen) = screen {
        let room: LogicalSize<f64> = screen.size().to_logical(screen.scale_factor());
        (w, h) = (w.min(room.width * 0.9), h.min(room.height * 0.9));
    }
    LogicalSize::new(w, h)
}

/// Logical size and maximized state of the window from the config's
/// `window` (`1200x800`, `1200x800 maximized`); the first-start size when
/// absent or unreadable.
fn window_state(saved: Option<&str>) -> ((u32, u32), bool) {
    let saved = saved.unwrap_or_default();
    let (size, state) = saved.split_once(' ').unwrap_or((saved, ""));
    let size = size.split_once('x').and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    let (w, h) = size.unwrap_or(WINDOW_SIZE);
    // Never smaller than allowed, nor absurdly large from an edited file.
    ((w.clamp(MIN_WINDOW_SIZE.0, 16_384), h.clamp(MIN_WINDOW_SIZE.1, 16_384)), state == "maximized")
}

/// A file write for the [`Writer`] thread.
enum Job {
    SaveSession(Box<Session>),
    DeleteSession(String),
    SaveConfig(Box<Config>),
    SaveAgent(Records, Box<Agent>),
    DeleteAgent(Records, String),
    /// Write the usage log from the saved sessions, unless there is one already.
    StartUsageLog,
    /// Add a response's usage to the log.
    LogUsage(Box<Record>),
    /// Keep the accounts' plan limits.
    SaveLimits(HashMap<String, Limits>),
    /// List the saved sessions and send them back, with how many files
    /// could not be read.
    ListSessions(mpsc::Sender<(Vec<SessionSummary>, usize)>),
}

/// Tells the user about a write that failed; called on the writer thread.
type Report = Arc<dyn Fn(String) + Send + Sync>;

impl Job {
    /// Performs the job against `store` (`None`: sessions are not saved).
    /// A failure is logged and handed to `report`, for the user.
    fn run(self, store: Option<&mut SessionStore>, report: &dyn Fn(String)) {
        let result = match (self, store) {
            (Self::SaveSession(session), Some(store)) => store.save(&session).map_err(|e| ("save the session", e)),
            (Self::DeleteSession(id), Some(store)) => store.delete(&id).map_err(|e| ("delete the session", e)),
            (Self::SaveSession(_) | Self::DeleteSession(_) | Self::StartUsageLog, None) => Ok(()),
            (Self::SaveConfig(config), _) => config.save().map_err(|e| ("save the settings", e)),
            (Self::SaveAgent(agents, agent), _) => agents.save(&agent.id, &*agent).map_err(|e| ("save the agent", e)),
            (Self::DeleteAgent(agents, id), _) => agents.delete(&id).map_err(|e| ("delete the agent", e)),
            (Self::StartUsageLog, Some(store)) => Ledger::path()
                .and_then(|path| match Ledger::read(&path) {
                    Some(_) => Ok(()),
                    None => Ledger::collect(&store.all()).write(&path),
                })
                .map_err(|e| ("start the usage log", e)),
            (Self::LogUsage(record), _) => {
                Ledger::path().and_then(|path| Ledger::append(&path, &record)).map_err(|e| ("log what was used", e))
            }
            (Self::SaveLimits(limits), _) => stats::save_limits(&limits).map_err(|e| ("keep the plan limits", e)),
            (Self::ListSessions(reply), store) => {
                let listed = match store.map(SessionStore::list) {
                    Some(Ok((sessions, errors))) => {
                        for e in &errors {
                            log::error(format!("skipping unreadable session: {e}"));
                        }
                        (sessions, errors.len())
                    }
                    Some(Err(e)) => {
                        log::error(format!("cannot list sessions: {e}"));
                        (Vec::new(), 1)
                    }
                    None => (Vec::new(), 0),
                };
                // The receiver only disappears if the app is exiting.
                let _ = reply.send(listed);
                Ok(())
            }
        };
        if let Err((what, e)) = result {
            log::error(format!("could not {what}: {e}"));
            report(format!("Could not {what}: {e}"));
        }
    }
}

/// Owns the session store on a background thread and performs every file
/// write in the order it was queued, so the UI never waits on the disk.
/// Dropping it finishes the queue first, so nothing is lost on exit.
struct Writer {
    queue: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    /// Used on the calling thread if the writer thread could not start.
    inline: Option<SessionStore>,
    /// Told about every write that fails.
    report: Report,
}

impl Writer {
    /// Starts the thread for sessions in `dir` (`None`: not saved), telling
    /// `report` about every write that fails.
    fn start(dir: Option<PathBuf>, report: impl Fn(String) + Send + Sync + 'static) -> Self {
        let (queue, jobs) = mpsc::channel::<Job>();
        let report: Report = Arc::new(report);
        let (fallback, theirs) = (dir.clone(), Arc::clone(&report));
        let spawned = std::thread::Builder::new().name("atroxabot-writer".into()).spawn(move || {
            let mut store = dir.map(SessionStore::at);
            for job in jobs {
                job.run(store.as_mut(), &*theirs);
            }
        });
        match spawned {
            Ok(thread) => Self { queue: Some(queue), thread: Some(thread), inline: None, report },
            Err(e) => {
                log::error(format!("cannot start the writer thread, saving on the UI thread: {e}"));
                Self { queue: None, thread: None, inline: fallback.map(SessionStore::at), report }
            }
        }
    }

    /// Queues `job`.
    fn send(&mut self, job: Job) {
        let job = match &self.queue {
            Some(queue) => match queue.send(job) {
                Ok(()) => return,
                // The thread is gone; still don't lose the write.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        job.run(self.inline.as_mut(), &*self.report);
    }

    /// The saved sessions, newest first, and how many files could not be
    /// read. Waits for the writes queued before it, which only happens at startup.
    fn list_sessions(&mut self) -> (Vec<SessionSummary>, usize) {
        let (reply, sessions) = mpsc::channel();
        self.send(Job::ListSessions(reply));
        sessions.recv().unwrap_or_default()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the queue ends the thread once everything is written.
        self.queue = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The OS window decoration style matching `scheme`.
fn window_theme(scheme: Scheme) -> Theme {
    if scheme.is_light() { Theme::Light } else { Theme::Dark }
}

/// Gives the window the app icon for its title bar and taskbar entry.
///
/// Windows loads the sizes `build.rs` embedded, picked for the display's
/// scale; X11 gets the 256px PNG. macOS ignores window icons and Wayland
/// takes them from a `.desktop` file, so both keep the default.
fn set_icon(window: &Window) {
    #[cfg(windows)]
    {
        use winit::platform::windows::{IconExtWindows, WindowExtWindows};
        let scale = window.scale_factor();
        let load = |side: f64| {
            let side = (side * scale).round() as u32;
            winit::window::Icon::from_resource(1, Some(winit::dpi::PhysicalSize::new(side, side))).ok()
        };
        window.set_window_icon(load(16.0));
        window.set_taskbar_icon(load(32.0));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Ok((rgba, w, h)) = image::decode_png(include_bytes!("../assets/icon/256.png"))
        && let Ok(icon) = winit::window::Icon::from_rgba(rgba, w, h)
    {
        window.set_window_icon(Some(icon));
    }
    #[cfg(target_os = "macos")]
    let _ = window;
}

/// Minimal executor for wgpu's initialisation futures.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keys_step_and_clamp() {
        let key = |c: &str| Key::Character(c.into());
        assert_eq!(zoomed(100, &key("=")), Some(110));
        assert_eq!(zoomed(100, &key("-")), Some(90));
        assert_eq!(zoomed(150, &key("0")), Some(100));
        assert_eq!(zoomed(ZOOM.1, &key("+")), Some(ZOOM.1), "clamped");
        assert_eq!(zoomed(100, &key("k")), None);
    }

    #[test]
    fn the_smallest_window_grows_with_the_zoom() {
        let size = |zoom| {
            let size = min_size(zoom, None);
            (size.width.round() as u32, size.height.round() as u32)
        };
        assert_eq!(size(100), MIN_WINDOW_SIZE);
        assert_eq!(size(150), (1230, 840));
        assert_eq!(size(50), (410, 280));
    }

    #[test]
    fn window_state_survives_odd_input() {
        assert_eq!(window_state(None), (WINDOW_SIZE, false));
        assert_eq!(window_state(Some("1440x900 maximized")), ((1440, 900), true));
        assert_eq!(window_state(Some("10x99999")), ((MIN_WINDOW_SIZE.0, 16_384), false), "clamped");
        assert_eq!(window_state(Some("nonsense")), (WINDOW_SIZE, false));
    }

    #[test]
    fn claude_api_connections_speak_messages_anywhere() {
        let proxy = Connection {
            kind: ConnectionKind::ClaudeApi,
            base_url: "https://proxy.example/v1".into(),
            ..Connection::default()
        };
        assert!(format!("{:?}", client_for(&proxy)).contains("anthropic: true"));
        let other = Connection { kind: ConnectionKind::OpenAi, ..proxy };
        assert!(format!("{:?}", client_for(&other)).contains("anthropic: false"));
    }
}
