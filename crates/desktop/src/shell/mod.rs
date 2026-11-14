//! The app's one screen: agents and their sessions in the sidebar, the open
//! session on the canvas beside it, the side panel (usage and git) right of
//! that, and the agent editor, the settings, the statistics, the spotlight,
//! a file's diff and a picture opened whole as modals over all of it.
//!
//! The shell owns what the user sees and edits (the config, the agents, the
//! open sessions) and asks the app for everything that needs a thread, a
//! file or a device through [`Action`]s. What comes back (engine events,
//! loaded sessions, model lists, call events) is applied here.

mod agent_form;
mod call;
mod change;
mod composer;
mod find;
mod limits;
mod menu;
mod panel;
mod picture;
mod plan;
mod question;
mod queue;
mod settings;
mod sidebar;
mod skills;
#[cfg(test)]
mod snapshot;
mod spotlight;
mod stats;
mod thread;
mod update;
mod warm;
mod widgets;
mod worktrees;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use arboard::Clipboard;
use atroxabot_core::stats::Ledger;
use atroxabot_core::{
    Agent, AgentEvent, CALL_ENDED, CALL_STARTED, ClaudeError, ClaudeInfo, ClaudeMcpServer, CodexError, CodexInfo,
    Config, Connection, ConnectionKind, Error, LimitWindow, Limits, Model, SearchHit, Session, SessionSummary, Verdict,
    unix_now,
};
use atroxabot_voice::CallEvent;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

use self::agent_form::AgentForm;
use self::call::CallView;
use self::composer::Composer;
use self::menu::{Menu, MenuState, folder_name};
use self::panel::{DiffView, GitView};
use self::question::QuestionForm;
use self::settings::{Settings, Tab};
use self::skills::SkillsView;
use self::spotlight::{Spotlight, Target};
use self::stats::StatsView;
use self::thread::Thread;
use self::update::Updater;
use self::widgets::{Scroll, line_field};
use crate::app::Action;
use crate::editor::Editor;
use crate::orb::{self, Mood};
use crate::paint::{Color, Painter, Rect, fade, mix};
use crate::theme::{self, Scheme};
use crate::ui::{ButtonStyle, Ui, button, edit_key, icon, icon_button, id};

/// The model a Claude API connection uses when an agent names none.
const DEFAULT_CLAUDE: &str = "claude-opus-5-5";
/// Most of an earlier conversation handed to a Claude account that takes it over, in bytes.
const TAKEOVER_BUDGET: usize = 24_000;

/// What is open over the screen.
enum Modal {
    /// An agent being created or edited.
    Agent(Box<AgentForm>),
    /// The settings.
    Settings(Box<Settings>),
    /// The search over sessions, agents and things to do.
    Spotlight(Box<Spotlight>),
    /// The usage statistics.
    Stats(Box<StatsView>),
    /// A changed file's diff.
    Diff(Box<DiffView>),
    /// A picture of the conversation, whole: the file it is.
    Picture(String),
}

/// The models an API connection offers, as far as the app knows.
#[derive(Debug, Clone, Default)]
enum Models {
    /// Being fetched.
    #[default]
    Loading,
    /// What the provider listed.
    Ready(Vec<Model>),
    /// Why they could not be listed.
    Failed(String),
}

/// The MCP servers Claude Code gives a Claude account's sessions (its
/// claude.ai connectors among them), as far as it was asked.
#[derive(Debug, Clone)]
pub(super) enum Connectors {
    /// Being asked.
    Loading,
    /// The servers and how each is doing.
    Ready(Vec<ClaudeMcpServer>),
    /// Why Claude Code could not say.
    Failed(String),
}

/// A command-line agent the app runs as installed: Claude Code or Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cli {
    /// Claude Code, for Claude accounts.
    Claude,
    /// The Codex CLI, for Codex accounts.
    Codex,
}

/// What a Claude or Codex account says while its tool's installer is open and the tool is not found yet.
const INSTALLING: &str = "Installing: finish in the installer's window";

/// What is known of a Claude account: what Claude Code said when asked.
#[derive(Debug, Clone)]
enum Account {
    /// Being asked.
    Checking,
    /// Claude Code is not installed.
    Missing,
    /// It could not be asked; why.
    Failed(String),
    /// Its models and who is signed in.
    Ready(Box<ClaudeInfo>),
}

/// What is known of a Codex account: what Codex said when asked.
#[derive(Debug, Clone)]
enum CodexAccount {
    /// Being asked.
    Checking,
    /// Codex is not installed.
    Missing,
    /// It could not be asked; why.
    Failed(String),
    /// Its models and who is signed in.
    Ready(Box<CodexInfo>),
}

/// Something the user is told, on a card under the top bar, until they dismiss it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    /// What it says.
    text: String,
}

/// The whole user interface.
pub struct Shell {
    /// The user's settings; the app saves them when asked to.
    pub config: Config,
    agents: Vec<Agent>,
    /// Every saved session, in the order the sidebar lists them: the
    /// user's own, a new one on top.
    sessions: Vec<SessionSummary>,
    /// Sessions opened since the app started, by id.
    threads: HashMap<String, Thread>,
    /// The selected agent.
    agent: Option<String>,
    /// The open session, one of the selected agent's.
    current: Option<String>,
    /// The session that was open when the clock was last looked at: one
    /// that is no longer has just been left (`warm.rs`).
    looked: Option<String>,
    modal: Option<Modal>,
    /// The side panel floats over the session (the window has no room to
    /// set it beside it): Esc and a click outside it close it.
    panel_floats: bool,
    composer: Composer,
    menu: Option<MenuState>,
    /// Model lists of API connections, by connection id.
    models: HashMap<String, Models>,
    /// What Claude Code says of each Claude account, by connection id.
    claude: HashMap<String, Account>,
    /// What Codex says of each Codex account, by connection id.
    codex: HashMap<String, CodexAccount>,
    /// The tools whose installer was opened and that have not been found since.
    installing: HashSet<Cli>,
    /// Problems to tell the user about, oldest first.
    notices: Vec<Notice>,
    call: Option<CallView>,
    sidebar_scroll: Scroll,
    /// The repository of the folder the open session works in, for the side panel.
    git: GitView,
    /// The open session's title while it is being changed in the top bar.
    renaming: Option<Editor>,
    /// The user's own pictures of providers, by the name each is filed under.
    icons: HashMap<String, String>,
    /// Where each Claude account's plan limits stand, as last reported, by connection id.
    limits: HashMap<String, Limits>,
    /// How asking each account where its plan limits stand goes, by connection id (`plan.rs`).
    checks: HashMap<String, plan::Check>,
    /// The answers being given to the question the agent asks.
    question: Option<QuestionForm>,
    /// A session whose delete button was clicked once: again deletes it.
    confirm_delete: Option<String>,
    /// The last message was spoken, so the next typed one says the call ended.
    was_on_call: bool,
    /// The folder the next new session will work in.
    pending_folder: String,
    /// Whether it will work there in a git worktree of its own.
    pending_worktree: bool,
    /// How each session's worktree stood when it was last looked at, by session.
    worktrees: HashMap<String, atroxabot_core::worktree::State>,
    /// Worktree folders left without a session that were kept (they hold changes).
    orphans: Vec<atroxabot_core::worktree::Orphan>,
    /// How much disk each worktree's folder takes, once the settings asked.
    sizes: HashMap<String, u64>,
    /// The worktrees are being looked at, and when that was last asked for.
    sweeping: bool,
    swept: Option<Instant>,
    /// The session the mouse holds in the sidebar, and whether it has been
    /// moved far enough to be a drag and not a click.
    drag: Option<(String, bool)>,
    /// The skill library, and the repositories looked in.
    skills: SkillsView,
    /// How far updating the app has come: only ever as far as the user clicked.
    updater: Updater,
    /// Each Claude account's MCP servers and connectors, by connection id,
    /// once the MCP settings asked for them.
    connectors: HashMap<String, Connectors>,
    /// The message being sent goes on after a usage limit stopped its session (`limits.rs`).
    going_on: bool,
}

impl Shell {
    /// The interface over the saved `config`, `agents` and `sessions`
    /// (newest first, as the store lists them).
    pub fn new(config: Config, mut agents: Vec<Agent>, mut sessions: Vec<SessionSummary>) -> Self {
        agents.sort_by_key(|a| a.created);
        // The order the user dragged them into; one started since stays before them, newest first.
        let places: HashMap<&str, usize> = config.session_order.iter().map(String::as_str).zip(1..).collect();
        sessions.sort_by_key(|session| places.get(session.id.as_str()).copied().unwrap_or(0));
        let agent = config.last_agent.clone().filter(|id| agents.iter().any(|a| a.id == *id));
        let agent = agent.or_else(|| agents.first().map(|a| a.id.clone()));
        // With nobody to talk to yet, the first thing to do is make someone.
        let modal = agents.is_empty().then(|| Modal::Agent(Box::new(AgentForm::new(None))));
        Self {
            config,
            agents,
            sessions,
            threads: HashMap::new(),
            agent,
            current: None,
            looked: None,
            modal,
            panel_floats: false,
            composer: Composer::default(),
            menu: None,
            models: HashMap::new(),
            claude: HashMap::new(),
            codex: HashMap::new(),
            installing: HashSet::new(),
            notices: Vec::new(),
            call: None,
            sidebar_scroll: Scroll::default(),
            git: GitView::default(),
            renaming: None,
            icons: HashMap::new(),
            limits: HashMap::new(),
            checks: HashMap::new(),
            question: None,
            confirm_delete: None,
            was_on_call: false,
            pending_folder: String::new(),
            pending_worktree: false,
            worktrees: HashMap::new(),
            orphans: Vec::new(),
            sizes: HashMap::new(),
            sweeping: false,
            swept: None,
            drag: None,
            skills: SkillsView::default(),
            updater: Updater::default(),
            connectors: HashMap::new(),
            going_on: false,
        }
    }

    /// Opens the selected agent's latest session and asks Claude Code about
    /// every Claude account; call once after `new`.
    pub fn start(&mut self, actions: &mut Vec<Action>) {
        self.skills.start(atroxabot_core::skills::Job::Load, actions);
        for connection in self.config.all_connections() {
            self.probe(&connection, actions);
        }
        if let Some(agent) = self.agent.clone() {
            self.select_agent(&agent, actions);
        }
        // What was merged or left behind since the last run is cleaned up.
        self.sweep_worktrees(actions);
    }

    /// What a skill library job came to.
    pub fn skills_report(&mut self, report: atroxabot_core::skills::Report) {
        if let Some(problem) = self.skills.report(report) {
            self.notify(problem);
        }
    }

    /// The user's own pictures of providers: image files in the `icons`
    /// folder, by name (`claude`, `anthropic`, `openrouter`, …).
    pub fn set_icons(&mut self, icons: HashMap<String, String>) {
        self.icons = icons;
    }

    /// The accounts' plan limits as they were last reported, kept from earlier runs.
    pub fn set_limits(&mut self, limits: HashMap<String, Limits>) {
        self.limits = limits;
    }

    /// Asks Claude Code (or Codex) what the account `connection` offers.
    fn probe(&mut self, connection: &Connection, actions: &mut Vec<Action>) {
        match connection.kind {
            ConnectionKind::ClaudeAgent => {
                self.claude.entry(connection.id.clone()).or_insert(Account::Checking);
                actions.push(Action::ProbeClaude(Box::new(connection.clone())));
            }
            ConnectionKind::Codex => {
                self.codex.entry(connection.id.clone()).or_insert(CodexAccount::Checking);
                actions.push(Action::ProbeCodex(Box::new(connection.clone())));
            }
            ConnectionKind::ClaudeApi | ConnectionKind::OpenAi => {}
        }
    }

    /// Codex answered (or could not) about the Codex account `connection`.
    /// Where its plan's limits stand comes with the answer.
    pub fn codex_info(&mut self, connection: &str, result: Result<CodexInfo, CodexError>, actions: &mut Vec<Action>) {
        self.codex_checked(connection, matches!(&result, Ok(info) if info.limits.is_some()));
        let account = match result {
            Ok(info) => {
                if let Some(limits) = &info.limits {
                    let limits = atroxabot_core::codex_limits(limits);
                    merge_limits(self.limits.entry(connection.to_owned()).or_default(), limits);
                    actions.push(Action::SaveLimits(self.limits.clone()));
                }
                self.installing.remove(&Cli::Codex);
                CodexAccount::Ready(Box::new(info))
            }
            Err(CodexError::NotFound) => CodexAccount::Missing,
            Err(e) => CodexAccount::Failed(e.to_string()),
        };
        self.codex.insert(connection.to_owned(), account);
    }

    /// Whether the Codex account `connection` is known to be signed in.
    fn codex_signed_in(&self, connection: &str) -> bool {
        matches!(self.codex.get(connection), Some(CodexAccount::Ready(info)) if info.account.is_some())
    }

    /// Claude Code answered (or could not) about the Claude account `connection`.
    pub fn claude_info(&mut self, connection: &str, result: Result<ClaudeInfo, ClaudeError>) {
        let account = match result {
            Ok(info) => {
                self.installing.remove(&Cli::Claude);
                Account::Ready(Box::new(info))
            }
            Err(ClaudeError::NotFound) => Account::Missing,
            Err(e) => Account::Failed(e.to_string()),
        };
        self.claude.insert(connection.to_owned(), account);
    }

    /// The installer of `cli` is open in a terminal: what waits for it says
    /// so until it is found, which the app looks for again when the user
    /// comes back to its window ([`Shell::focused`]).
    pub fn installing(&mut self, cli: Cli) {
        self.installing.insert(cli);
    }

    /// Whether the installer of `cli` was opened and `cli` is not found yet.
    fn is_installing(&self, cli: Cli) -> bool {
        self.installing.contains(&cli)
    }

    /// Claude Code said (or could not say) which MCP servers a Claude account's sessions get.
    pub fn connectors_loaded(&mut self, connection: &str, result: Result<Vec<ClaudeMcpServer>, ClaudeError>) {
        let connectors = match result {
            Ok(servers) => Connectors::Ready(servers),
            Err(ClaudeError::NotFound) => Connectors::Failed("Claude Code is not installed.".to_owned()),
            Err(e) => Connectors::Failed(e.to_string()),
        };
        self.connectors.insert(connection.to_owned(), connectors);
    }

    /// The window has the user's attention again: an account they may just
    /// have signed in to (in the terminal the app opened) is asked about anew.
    pub fn focused(&mut self, actions: &mut Vec<Action>) {
        // The files may have changed while the user was elsewhere, and a session's work been merged.
        self.refresh_git(actions);
        self.sweep_worktrees_again(actions);
        // And the plan's limits: a limit may have been reset on claude.ai.
        self.check_shown_limits(actions);
        // Only while the settings are open: that is where signing in starts.
        if !matches!(self.modal, Some(Modal::Settings(_))) {
            return;
        }
        // A connector may have been added on claude.ai meanwhile: the MCP settings ask again.
        self.connectors.retain(|_, connectors| matches!(connectors, Connectors::Loading));
        for connection in self.config.all_connections() {
            let signed_in =
                matches!(self.claude.get(&connection.id), Some(Account::Ready(info)) if !info.account.email.is_empty());
            let checking = matches!(self.claude.get(&connection.id), Some(Account::Checking));
            if connection.kind == ConnectionKind::ClaudeAgent && !signed_in && !checking {
                actions.push(Action::ProbeClaude(Box::new(connection)));
            } else if connection.kind == ConnectionKind::Codex
                && !self.codex_signed_in(&connection.id)
                && !matches!(self.codex.get(&connection.id), Some(CodexAccount::Checking))
            {
                actions.push(Action::ProbeCodex(Box::new(connection)));
            }
        }
    }

    /// Shows a problem until the user dismisses it. An account that ran
    /// out of usage is no such problem: the conversation it stopped says
    /// so, and the tray above its message box says when it goes on.
    pub fn notify(&mut self, text: String) {
        // Said again, it is the same card.
        self.notices.retain(|notice| notice.text != text);
        self.notices.push(Notice { text });
        // Old ones make way: three are plenty to read.
        let extra = self.notices.len().saturating_sub(3);
        self.notices.drain(..extra);
    }

    /// Whether something is moving and the next frame should follow at once.
    pub fn is_busy(&self) -> bool {
        self.call.is_some() || self.threads.values().any(|thread| thread.busy)
    }

    fn selected_agent(&self) -> Option<&Agent> {
        self.agent.as_deref().and_then(|id| self.agents.iter().find(|a| a.id == id))
    }

    /// The selected agent as the open session thinks with it: through the
    /// provider, with the model and at the effort the session keeps. A
    /// session still being read shows what the sidebar knows of it.
    fn agent_in_use(&self) -> Option<Agent> {
        let mut agent = self.selected_agent()?.clone();
        if let Some(thread) = self.thread() {
            self.apply_choice(&thread.session, &mut agent);
        }
        Some(agent)
    }

    /// Whether the open session's messages are still being read: nothing
    /// of it is changed or saved until they are in, or it would be saved
    /// over them.
    fn loading(&self) -> bool {
        self.thread().is_some_and(|thread| !thread.loaded)
    }

    /// Makes the provider, model and effort of `agent` (as
    /// [`Shell::agent_in_use`] gave it) the open session's, and the agent's
    /// own for the sessions it starts next. Its other sessions keep theirs.
    fn choose_thinking(&mut self, agent: Agent, actions: &mut Vec<Action>) {
        if self.loading() {
            return;
        }
        if let Some(thread) = self.thread_mut() {
            thread.session.keep_choice(&agent);
            actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        }
        self.update_agent(agent, actions);
        if let Some(session) = self.current.clone() {
            self.touch_summary(&session);
        }
    }

    /// Has the message box show what was written for the open session, or
    /// for the selected agent's next one.
    fn show_draft(&mut self) {
        let owner = match (&self.current, &self.agent) {
            (Some(session), _) => session.clone(),
            (None, Some(agent)) => composer::next_of(agent),
            (None, None) => return,
        };
        self.composer.show(&owner);
    }

    fn thread(&self) -> Option<&Thread> {
        self.current.as_deref().and_then(|id| self.threads.get(id))
    }

    fn thread_mut(&mut self) -> Option<&mut Thread> {
        self.current.as_deref().and_then(|id| self.threads.get_mut(id))
    }

    /// The connection `agent` thinks through, if it still exists.
    fn connection(&self, agent: &Agent) -> Option<Connection> {
        self.config.connection(&agent.connection)
    }

    /// The folder the open session works in, or the next one will.
    fn folder_in_use(&self) -> String {
        self.thread().map_or_else(|| self.pending_folder.clone(), |thread| thread.session.workdir.clone())
    }

    /// Makes `folder` the open session's (or the next one's) and remembers it.
    fn set_folder(&mut self, folder: &str, actions: &mut Vec<Action>) {
        if self.thread().is_some_and(|thread| thread.busy) {
            self.notify("Wait for the reply to finish, or stop it, before changing the folder.".into());
            return;
        }
        if self.worktree_in_use().is_some() {
            self.notify(
                "This session works in its own worktree. Start a new session to work in another folder.".into(),
            );
            return;
        }
        if self.loading() {
            return;
        }
        match self.thread_mut() {
            Some(thread) => {
                folder.clone_into(&mut thread.session.workdir);
                actions.push(Action::SaveSession(Box::new(thread.session.clone())));
            }
            None => folder.clone_into(&mut self.pending_folder),
        }
        self.config.use_folder(folder);
        actions.push(Action::SaveConfig);
        // The agent's next session starts where this one works.
        if let Some(mut agent) = self.selected_agent().cloned() {
            folder.clone_into(&mut agent.workdir);
            self.update_agent(agent, actions);
        }
    }

    /// A folder was picked for the session (or the picker was cancelled).
    pub fn folder_picked(&mut self, folder: Option<String>, actions: &mut Vec<Action>) {
        if let Some(folder) = folder {
            self.set_folder(&folder, actions);
        }
    }

    /// A picture was picked for the agent editor (or the picker was cancelled).
    pub fn picture_picked(&mut self, picture: Option<String>) {
        if let (Some(Modal::Agent(form)), Some(picture)) = (&mut self.modal, picture) {
            form.set_picture(&picture);
        }
    }

    /// Selects `agent` and opens its latest session (or an empty one).
    fn select_agent(&mut self, agent: &str, actions: &mut Vec<Action>) {
        (self.menu, self.renaming) = (None, None);
        if self.agent.as_deref() != Some(agent) {
            // A call is with one agent: choosing another hangs up.
            self.end_call(actions);
            self.agent = Some(agent.to_owned());
            self.config.last_agent = Some(agent.to_owned());
            actions.push(Action::SaveConfig);
        }
        self.pending_folder = self.selected_agent().map(|agent| agent.workdir.clone()).unwrap_or_default();
        self.pending_worktree = self.selected_agent().is_some_and(|agent| agent.worktree);
        if let Some(latest) = self.latest_session(agent).map(|session| session.id.clone()) {
            self.open_session(&latest, actions);
        } else {
            self.current = None;
            self.want_models_in_use(actions);
        }
    }

    /// Opens a saved session, reading its messages if they are not in
    /// memory. It shows the provider, model and effort it was left with at
    /// once: the sidebar knows them.
    fn open_session(&mut self, session: &str, actions: &mut Vec<Action>) {
        (self.menu, self.renaming) = (None, None);
        // The process of the one being left stays up a while (`warm.rs`): coming back answers at once.
        if !self.threads.contains_key(session) {
            let known = self.sessions.iter().find(|known| known.id == session).cloned();
            let known = known.unwrap_or_else(|| SessionSummary { id: session.to_owned(), ..SessionSummary::default() });
            self.threads.insert(session.to_owned(), Thread::loading(&known));
            actions.push(Action::LoadSession(session.to_owned()));
        }
        self.current = Some(session.to_owned());
        self.want_models_in_use(actions);
    }

    /// Asks for the models of the provider the open session (or the next
    /// one) thinks through, which may be another than its agent's own.
    fn want_models_in_use(&mut self, actions: &mut Vec<Action>) {
        if let Some(connection) = self.agent_in_use().and_then(|agent| self.connection(&agent)) {
            self.want_models(&connection, actions);
        }
    }

    /// Starts over with the selected agent: the next message begins a
    /// session, in the folder the one being left worked in.
    fn new_session(&mut self) {
        self.renaming = None;
        if let Some(previous) = self.current.take()
            && let Some(thread) = self.threads.get(&previous)
        {
            // Where the one being left worked: for one in a worktree, the folder that stands for.
            self.pending_folder = thread.session.origin();
            self.pending_worktree = thread.session.worktree.is_some();
        }
    }

    /// Asks for an API `connection`'s models unless they are known already.
    fn want_models(&mut self, connection: &Connection, actions: &mut Vec<Action>) {
        // An account's models come from its agent, with what else it says of itself.
        if connection.kind.is_account() || self.models.contains_key(&connection.id) {
            return;
        }
        self.models.insert(connection.id.clone(), Models::Loading);
        actions.push(Action::LoadModels(Box::new(connection.clone())));
    }

    /// A model list arrived (or failed) for the connection `id`.
    pub fn models_loaded(&mut self, connection: &str, result: Result<Vec<Model>, Error>) {
        let models = match result {
            Ok(models) => Models::Ready(models),
            Err(e) => Models::Failed(e.to_string()),
        };
        self.models.insert(connection.to_owned(), models);
    }

    /// The model called `model` on `connection`, with its prices and context
    /// window, when the provider listed it.
    fn model_info(&self, connection: &str, model: &str) -> Option<&Model> {
        match self.models.get(connection) {
            Some(Models::Ready(models)) => models.iter().find(|m| m.id == model),
            _ => None,
        }
    }

    /// A session's messages were read from disk. What the user sent while
    /// they were being read goes now.
    pub fn session_loaded(&mut self, session: &str, result: Result<Session, Error>, actions: &mut Vec<Action>) {
        match result {
            Ok(loaded) => {
                let Some(thread) = self.threads.get_mut(session).filter(|thread| !thread.loaded) else { return };
                thread.session = loaded;
                thread.loaded = true;
                let waiting = thread.queued.take();
                if self.current.as_deref() == Some(session) {
                    self.want_models_in_use(actions);
                }
                if let Some((text, spoken, files)) = waiting {
                    self.send_to(session, &text, &files, (spoken, false), actions);
                }
            }
            Err(e) => {
                let waiting = self.threads.remove(session).and_then(|thread| thread.queued);
                let open = self.current.as_deref() == Some(session);
                if open {
                    self.current = None;
                }
                self.notify(format!("That session could not be opened: {e}"));
                // What was written for it is not lost: it is back in the message box.
                if let Some((text, _, files)) = waiting.filter(|_| open) {
                    self.show_draft();
                    self.composer.restore(&text, files);
                }
            }
        }
    }

    /// Sessions whose messages hold the searched text were found.
    pub fn search_results(&mut self, generation: u64, hits: Vec<SearchHit>) {
        if let Some(Modal::Spotlight(spotlight)) = &mut self.modal {
            spotlight.found(generation, hits);
        }
    }

    /// Opens the statistics and has every session read for them, and every
    /// account asked where its plan's limits stand.
    fn open_stats(&mut self, actions: &mut Vec<Action>) {
        self.modal = Some(Modal::Stats(Box::default()));
        actions.push(Action::LoadStats);
        self.check_every_account(actions);
    }

    /// Every session's usage was read, for the statistics.
    pub fn stats_loaded(&mut self, ledger: Ledger) {
        if let Some(Modal::Stats(stats)) = &mut self.modal {
            stats.loaded(ledger);
        }
    }

    /// Does what a result of the spotlight stands for.
    fn run_target(&mut self, target: Target, actions: &mut Vec<Action>) {
        match target {
            Target::Session { agent, session, find } => {
                self.open_found(&agent, &session, actions);
                // Found by what was said in it: the conversation opens where that stands.
                if !find.is_empty()
                    && let Some(thread) =
                        self.threads.get_mut(&session).filter(|_| self.current.as_deref() == Some(&session))
                {
                    thread.open_find(&find, false, true);
                }
            }
            Target::Agent(agent) => self.select_agent(&agent, actions),
            Target::NewSession => self.new_session(),
            Target::NewAgent => self.modal = Some(Modal::Agent(Box::new(AgentForm::new(None)))),
            Target::Stats => self.open_stats(actions),
            Target::Settings => {
                self.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::General))));
            }
            Target::Panel => self.toggle_panel(actions),
        }
    }

    /// Ends the renaming of the open session, with the new title when
    /// `keep` (and there is one), with the old one otherwise.
    fn finish_rename(&mut self, keep: bool, actions: &mut Vec<Action>) {
        let Some(editor) = self.renaming.take() else { return };
        let title = editor.text().split_whitespace().collect::<Vec<_>>().join(" ");
        let Some(thread) = self
            .thread_mut()
            .filter(|thread| keep && thread.loaded && !title.is_empty() && thread.session.title != title)
        else {
            return;
        };
        thread.session.title.clone_from(&title);
        thread.session.named = true;
        actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        let id = thread.session.id.clone();
        if let Some(summary) = self.sessions.iter_mut().find(|summary| summary.id == id) {
            summary.title = title;
        }
    }

    /// Sends `text`, with the `files` that go with it, to the selected agent
    /// in the open session, starting one if none is open. `spoken` when it
    /// was said on a call. While the agent works, a typed message waits to
    /// be read at its next step (`queue.rs`).
    fn send(&mut self, text: &str, spoken: bool, files: Vec<String>, actions: &mut Vec<Action>) {
        let text = text.trim();
        if text.is_empty() && files.is_empty() {
            return;
        }
        let session = match self.current.clone() {
            Some(session) => session,
            // Nothing to clear, and no session for a message that cannot go anywhere yet.
            None if text == "/clear" => return,
            None => {
                let Some(agent) = self.selected_agent().cloned() else { return };
                let owner = agent.id.clone();
                if self.ready(agent.clone()).is_none() {
                    return;
                }
                let mut session = Session::new(&owner);
                session.workdir.clone_from(&self.pending_folder);
                // Asked for here, made when the message goes.
                session.worktree = self.pending_worktree.then(atroxabot_core::Worktree::default);
                // It keeps the model and effort it starts with.
                session.keep_choice(&agent);
                let id = session.id.clone();
                self.sessions.insert(0, session.summary());
                self.threads.insert(id.clone(), Thread::new(session));
                self.current = Some(id.clone());
                id
            }
        };
        if let Some(call) = &mut self.call {
            call.hold = false;
            if spoken {
                call.session.clone_from(&session);
            }
        }
        // What the user sends is what happens next, not going on by itself after a usage limit.
        self.cancel_resume(&session);
        let Some(thread) = self.threads.get_mut(&session) else { return };
        // What the user sends is for the conversation: a subagent open in its place gives way to it.
        if thread.sub.is_some() {
            thread.open_agent(None);
        }
        if !thread.loaded {
            // Its messages are still being read: it goes once they are in, never over them.
            thread.queued = Some((text.to_owned(), spoken, files));
            return;
        }
        if thread.busy {
            if spoken || thread.stopping {
                // Said while the agent was still stopping: sent once it has.
                thread.queued = Some((text.to_owned(), spoken, files));
            } else if text == "/clear" {
                // Once the turn has stopped.
                self.stop(actions);
                if let Some(thread) = self.threads.get_mut(&session) {
                    thread.queued = Some((text.to_owned(), false, Vec::new()));
                }
            } else {
                self.follow_up(&session, text, files, actions);
            }
            return;
        }
        self.send_to(&session, text, &files, (spoken, false), actions);
    }

    /// The agent ready to answer, with the model and effort it answers
    /// with, and its connection; `None` (with a notice) when it cannot.
    fn ready(&mut self, mut agent: Agent) -> Option<(Agent, Connection)> {
        let Some(connection) = self.connection(&agent) else {
            self.notify(format!("{}'s provider no longer exists. Pick another in the top bar.", agent.name));
            return None;
        };
        if connection.kind == ConnectionKind::ClaudeAgent {
            match self.claude.get(&connection.id) {
                Some(Account::Missing) => {
                    self.notify("Claude Code is not installed: install it in Settings, Providers.".into());
                    return None;
                }
                Some(Account::Ready(info)) if info.account.email.is_empty() => {
                    self.notify(format!("{} is not signed in. Sign in under Settings, Providers.", connection.name));
                    return None;
                }
                _ => {}
            }
        }
        if connection.kind == ConnectionKind::Codex {
            match self.codex.get(&connection.id) {
                Some(CodexAccount::Missing) => {
                    self.notify("Codex is not installed: install it in Settings, Providers.".into());
                    return None;
                }
                Some(CodexAccount::Ready(info)) if info.account.is_none() && info.requires_auth => {
                    self.notify(format!("{} is not signed in. Sign in under Settings, Providers.", connection.name));
                    return None;
                }
                _ => {}
            }
        }
        // An API needs a model named: the provider's first, when the agent names none.
        if agent.model.is_empty() && !connection.kind.is_account() {
            match self.model_choices(&connection).into_iter().next() {
                Some(first) => agent.model = first.value,
                None if connection.kind == ConnectionKind::ClaudeApi => DEFAULT_CLAUDE.clone_into(&mut agent.model),
                None => {
                    self.notify(format!(
                        "Pick a model for {} first: the model menu is under the message box.",
                        agent.name
                    ));
                    return None;
                }
            }
        }
        // It thinks as hard as was chosen, or as its model usually does.
        agent.effort =
            Self::effort_in_use(connection.kind, &self.model_in_use(&connection, &agent.model), &agent.effort);
        Some((agent, connection))
    }

    /// Starts a turn in `session`, whose agent is free, answering `text`
    /// and the `files` that go with it: `spoken` when it was said on a call,
    /// `interrupted` when the agent was stopped for it. Returns whether it
    /// went (a notice says why not).
    fn send_to(
        &mut self,
        session: &str,
        text: &str,
        files: &[String],
        (spoken, interrupted): (bool, bool),
        actions: &mut Vec<Action>,
    ) -> bool {
        // The app's own command; every other one is the provider's to run.
        if text == "/clear" {
            self.clear_session(session, actions);
            return true;
        }
        // Nothing is sent in a session whose messages are not in memory: it would be saved without them.
        let Some(thread) = self.threads.get(session).filter(|thread| thread.loaded) else { return false };
        // A session that names no agent is the selected one's.
        let owner = Some(thread.session.agent.as_str()).filter(|agent| !agent.is_empty()).or(self.agent.as_deref());
        let Some(mut agent) = self.agents.iter().find(|agent| Some(agent.id.as_str()) == owner).cloned() else {
            return false;
        };
        self.apply_choice(&thread.session, &mut agent);
        let chosen = agent.clone();
        let Some((agent, connection)) = self.ready(agent) else { return false };
        let compact = text.split_whitespace().next() == Some("/compact");
        if compact && connection.kind == ConnectionKind::Codex {
            self.notify(format!(
                "{} summarises the conversation by itself when its context fills up.",
                connection.name
            ));
            return true;
        }
        // An API session is summarised by the app itself: no message joins the conversation.
        if compact && !connection.kind.is_account() {
            let model = self.model_info(&connection.id, &agent.model).cloned();
            let Some(thread) = self.threads.get_mut(session).filter(|thread| thread.loaded) else { return false };
            thread.begin_turn();
            actions.push(Action::Compact {
                agent: Box::new(agent),
                connection: Box::new(connection),
                model: model.map(Box::new),
                session: Box::new(thread.session.clone()),
            });
            return true;
        }
        // A skill named the way Claude Code takes one: an API agent is told to load it.
        let skill = text.strip_prefix('/').and_then(|rest| rest.split_whitespace().next()).filter(|name| {
            !connection.kind.is_account() && self.skills.installed.iter().any(|skill| skill.name == *name)
        });
        let skill_note =
            skill.map(|name| format!("(The user asks for the skill `{name}`: load it with the `skill` tool first.)"));
        let model = self.model_info(&connection.id, &agent.model).cloned();
        let Some(thread) = self.threads.get_mut(session) else { return false };
        let going_on = self.going_on;
        // The agent is told once when a call begins and once when it is over.
        let announced = self.call.as_mut().is_some_and(|call| std::mem::replace(&mut call.announced, true));
        let call_note = match (spoken, announced, self.was_on_call) {
            _ if going_on => "",
            (true, false, _) => CALL_STARTED,
            (false, _, true) => CALL_ENDED,
            _ => "",
        };
        if !going_on {
            self.was_on_call = spoken;
        }
        let stopped_note = if interrupted { Self::interrupting_note(connection.kind, text) } else { "" };
        // A command (`/compact`) must come first to run: it goes as it is.
        let limit_note = if going_on && !text.trim_start().starts_with('/') { limits::GO_ON } else { "" };
        let note = [call_note, stopped_note, limit_note, skill_note.as_deref().unwrap_or_default()]
            .into_iter()
            .filter(|note| !note.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        // Going on by itself with nothing of the user's: an account is only told to; an API is asked in words.
        let quiet = going_on && text.is_empty() && connection.kind.is_account();
        let text = if going_on && text.is_empty() && !quiet { "Carry on from where you left off." } else { text };
        let mut sent = match (note.is_empty(), text.is_empty()) {
            (true, _) => text.to_owned(),
            (false, true) => note.clone(),
            (false, false) => format!("{note}\n\n{text}"),
        };
        if going_on {
            // A line in the conversation says why it went on, and with which account.
            let ran_on = &thread.session.account;
            let text = if ran_on.is_empty() || *ran_on == connection.id {
                "The usage limit lifted · going on".to_owned()
            } else {
                let former = self.config.connection(ran_on).map(|former| former.name).unwrap_or_default();
                format!("{former} is out of usage · going on with {}", connection.name)
            };
            thread.session.entries.push(atroxabot_core::Entry::Note { text });
        }
        // An account (Claude's or Codex's) that never saw this conversation is told what was said.
        let foreign = thread.session.remote.is_empty() || thread.session.account != connection.id;
        if connection.kind.is_account() && foreign {
            let earlier = thread.session.digest(TAKEOVER_BUDGET);
            if !earlier.is_empty() {
                sent = format!(
                    "(You are taking over a conversation. What was said so far:)\n\n{earlier}\n\n(Now the user says:)\n\n{sent}"
                );
            }
            // A process may still run on the account it started with (it was
            // busy when the account was switched): this message, and the
            // limits it reports, belong to the new one, which starts its own.
            if !thread.session.account.is_empty() && thread.session.account != connection.id {
                thread.background_lost();
                actions.push(Action::CloseLive(session.to_owned()));
            }
            thread.session.remote.clear();
            thread.session.account.clone_from(&connection.id);
        }
        // A session that kept no provider (or whose provider is gone) keeps what it answers with now.
        if thread.session.provider != chosen.connection {
            thread.session.keep_choice(&chosen);
        }
        thread.sent_on.clone_from(&connection.id);
        // An account's process takes another model or effort as it runs (the app asks it to); whether it
        // guesses the next message it keeps from its start, so another choice there needs a new one.
        let guesses = connection.kind == ConnectionKind::ClaudeAgent && !self.config.hide_suggestions;
        if connection.kind.is_account() && thread.guessing.is_some_and(|guessing| guessing != guesses) {
            // What it runs in the background goes with it.
            thread.background_lost();
            actions.push(Action::CloseLive(session.to_owned()));
        }
        thread.guessing = Some(guesses);
        thread.live = connection.kind.is_account();
        if quiet {
            thread.session.updated = unix_now();
        } else {
            thread.session.prompt(text, &note);
            thread.session.attach(files);
        }
        thread.begin_turn();
        actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        actions.push(Action::Send {
            agent: Box::new(agent),
            connection: Box::new(connection),
            model: model.map(Box::new),
            session: Box::new(thread.session.clone()),
            text: sent,
        });
        self.touch_summary(session);
        true
    }

    /// `/clear`: the session keeps its place, its name in the sidebar and
    /// its folder, and starts over: its messages are deleted and the agent
    /// begins with an empty context (a Claude account's process too). A
    /// title the user gave it stays; one taken from the first message goes,
    /// so the next message names it again.
    fn clear_session(&mut self, session: &str, actions: &mut Vec<Action>) {
        let Some(thread) = self.threads.get_mut(session).filter(|thread| thread.loaded && !thread.busy) else {
            return;
        };
        let old = &thread.session;
        let fresh = Session {
            id: old.id.clone(),
            agent: old.agent.clone(),
            title: if old.named { old.title.clone() } else { String::new() },
            named: old.named,
            created: old.created,
            workdir: old.workdir.clone(),
            worktree: old.worktree.clone(),
            window: old.window,
            provider: old.provider.clone(),
            model: old.model.clone(),
            effort: old.effort.clone(),
            ..Session::new(&old.agent)
        };
        *thread = Thread::new(fresh);
        actions.push(Action::CloseLive(session.to_owned()));
        // What the agent left running belongs to the conversation that is gone.
        actions.push(Action::StopJobs(session.to_owned()));
        actions.push(Action::SaveSession(Box::new(thread.session.clone())));
        self.touch_summary(session);
    }

    /// Refreshes the sidebar's entry for `session` from its messages. It
    /// stays where it is: sessions at work must not trade places.
    fn touch_summary(&mut self, session: &str) {
        // One still being read has no messages to sum up yet.
        let loaded = self.threads.get(session).filter(|thread| thread.loaded);
        let Some(summary) = loaded.map(|thread| thread.session.summary()) else { return };
        match self.sessions.iter_mut().find(|known| known.id == session) {
            Some(known) => *known = summary,
            None => self.sessions.insert(0, summary),
        }
    }

    /// The session of `agent` that changed last.
    fn latest_session(&self, agent: &str) -> Option<&SessionSummary> {
        self.sessions.iter().filter(|session| session.agent == agent).max_by_key(|session| session.updated)
    }

    /// Moves `session` to the `place`th spot among its agent's sessions, as a drag in the sidebar does.
    fn move_session(&mut self, session: &str, place: usize) {
        let Some(from) = self.sessions.iter().position(|s| s.id == session) else { return };
        let agent = &self.sessions[from].agent;
        let mut spots = self.sessions.iter().enumerate().filter(|(_, s)| s.agent == *agent).map(|(index, _)| index);
        // Taken out and put in where the one at that spot is: before it going up, after it going down.
        if let Some(to) = spots.clone().nth(place).or_else(|| spots.next_back())
            && to != from
        {
            let moved = self.sessions.remove(from);
            self.sessions.insert(to, moved);
        }
    }

    /// The sessions' order is the user's from now on: it is kept with the settings.
    fn keep_order(&mut self, actions: &mut Vec<Action>) {
        self.config.session_order = self.sessions.iter().map(|session| session.id.clone()).collect();
        actions.push(Action::SaveConfig);
    }

    /// Stops the open session's turn, and what it was saying.
    fn stop(&mut self, actions: &mut Vec<Action>) {
        let Some(thread) = self.thread_mut().filter(|thread| thread.busy) else { return };
        thread.queued = None;
        thread.stopping = true;
        "Stopping…".clone_into(&mut thread.status);
        actions.push(Action::Stop(thread.session.id.clone()));
        if let Some(call) = &mut self.call {
            call.hold = true;
            actions.push(Action::StopSpeaking);
            actions.push(Action::FinishReply);
        }
    }

    /// The user talked over the agent: its turn stops, and the rest of its
    /// reply stays unspoken. The call itself already went quiet.
    fn barge_in(&mut self, actions: &mut Vec<Action>) {
        let Some(call) = &mut self.call else { return };
        call.hold = true;
        if let Some(thread) = self.threads.get_mut(&call.session).filter(|thread| thread.busy) {
            thread.stopping = true;
            actions.push(Action::Stop(thread.session.id.clone()));
        }
    }

    /// Answers the open session's oldest pending approval.
    fn decide(&mut self, verdict: Verdict, actions: &mut Vec<Action>) {
        let Some(thread) = self.thread_mut() else { return };
        if thread.asks.is_empty() {
            return;
        }
        let ask = thread.asks.remove(0);
        actions.push(Action::Decide { session: thread.session.id.clone(), ask: ask.id, verdict });
    }

    /// Something happened in the turn of `session`.
    pub fn agent_event(&mut self, session: &str, event: AgentEvent, actions: &mut Vec<Action>) {
        let on_call = self.call.as_ref().is_some_and(|call| call.session == session);
        let speaking = on_call && self.call.as_ref().is_some_and(|call| !call.hold);
        let suggesting = self.suggesting();
        let Some(thread) = self.threads.get_mut(session) else { return };
        thread.session.apply(&event);
        // What a subagent's response used is logged too, apart: it is no reply of the session's.
        if let AgentEvent::Sub { event: inner, .. } = &event
            && let AgentEvent::ReplyEnded { model, usage, cost, .. } = inner.as_ref()
        {
            let reply = atroxabot_core::Reply {
                model: model.clone(),
                usage: *usage,
                cost: *cost,
                at: unix_now(),
                ..atroxabot_core::Reply::default()
            };
            let spent = Session {
                id: thread.session.id.clone(),
                agent: thread.session.agent.clone(),
                entries: vec![atroxabot_core::Entry::Reply(reply)],
                ..Session::default()
            };
            if let Some(mut record) = Ledger::collect(std::slice::from_ref(&spent)).records.pop()
                && record.input + record.output > 0
            {
                record.provider = ran_on(thread, &self.agents);
                record.provider_name = self.config.connection(&record.provider).map(|c| c.name).unwrap_or_default();
                actions.push(Action::LogUsage(Box::new(record)));
            }
        }
        // What a response used goes to the usage log, which outlives the session.
        if matches!(event, AgentEvent::ReplyEnded { .. })
            && let Some(mut record) = Ledger::collect(std::slice::from_ref(&thread.session)).records.pop()
            && (record.input + record.output > 0 || !record.tools.is_empty())
        {
            record.provider = ran_on(thread, &self.agents);
            record.provider_name = self.config.connection(&record.provider).map(|c| c.name).unwrap_or_default();
            actions.push(Action::LogUsage(Box::new(record)));
        }
        // Claude Code started a turn by itself: a follow-up of its own queue, or a background task that ended.
        if !thread.busy && matches!(event, AgentEvent::Working | AgentEvent::ReplyStarted | AgentEvent::FollowUp { .. })
        {
            thread.begin_turn();
        }
        let (mut resend, mut give_back, mut send_waiting, mut out_of_usage) = (None, false, None, false);
        let ended = matches!(event, AgentEvent::TurnEnded { .. });
        match event {
            AgentEvent::Text(text) if speaking => actions.push(Action::Say(text)),
            AgentEvent::Permission(ask) => {
                thread.asks.push(ask);
                actions.push(Action::Attention);
            }
            AgentEvent::PermissionGone(id) => thread.asks.retain(|ask| ask.id != id),
            AgentEvent::Limits(limits) => {
                // They are those of the account the turn runs on.
                let account = thread.session.account.clone();
                thread.refused |= limits.status == "rejected" && thread.busy;
                merge_limits(self.limits.entry(account).or_default(), limits);
                actions.push(Action::SaveLimits(self.limits.clone()));
                return;
            }
            // Its usage limit stops the turn: what happens next is decided when it has ended.
            AgentEvent::LimitReached => thread.limited = true,
            AgentEvent::Status(status) => thread.status = status,
            AgentEvent::Summarized(_) => thread.invalidate(),
            AgentEvent::Remote(_) => actions.push(Action::SaveSession(Box::new(thread.session.clone()))),
            // Claude Code says what its context holds after the turn, which saved the session already.
            AgentEvent::Context(_) if !thread.busy => {
                actions.push(Action::SaveSession(Box::new(thread.session.clone())));
            }
            // Taken in: it is in the conversation now.
            AgentEvent::FollowUp { id, .. } => thread.follow_ups.retain(|waiting| waiting.message.id != id),
            AgentEvent::Queued(count) => thread.more = count,
            AgentEvent::Background { id, what, ended } => thread.background_task(id, what, ended),
            // Only between turns: one that started since makes it stale.
            AgentEvent::Suggestion(text) if suggesting && !thread.busy => thread.suggestion = Some(text),
            AgentEvent::TurnEnded { problem } => {
                let (hurried, stopped) = (std::mem::take(&mut thread.hurry), thread.stopping);
                // The account's usage limit stopped it: its agent said so, or it was refused and the turn failed.
                let refused = std::mem::take(&mut thread.refused) && problem.is_some();
                let out = (std::mem::take(&mut thread.limited) || refused) && !stopped;
                if !out {
                    thread.tries = 0;
                }
                thread.end_turn(problem);
                actions.push(Action::SaveSession(Box::new(thread.session.clone())));
                // A Claude account's process is asked what its context holds now (an API engine said so itself).
                actions.push(Action::AskContext(session.to_owned()));
                // The turn may have committed, or merged: how its worktree stands is read again.
                if let Some(worktree) = thread.session.worktree.clone().filter(atroxabot_core::Worktree::made) {
                    let job = atroxabot_core::worktree::Job::State { session: session.to_owned(), worktree };
                    actions.push(Action::Worktree(job));
                }
                if speaking {
                    actions.push(Action::FinishReply);
                }
                resend = thread.queued.take();
                if std::mem::take(&mut thread.more) > 0 {
                    // Claude Code answers what was sent meanwhile next, in a turn of its own.
                    thread.begin_turn();
                } else {
                    actions.push(Action::Attention);
                    // What the turn did not take in was not read: it goes again, or back to the user.
                    for waiting in &mut thread.follow_ups {
                        waiting.sent = false;
                    }
                    if out {
                        // What waits stays in the tray: it goes when the session goes on.
                        out_of_usage = true;
                    } else if !thread.follow_ups.is_empty() {
                        if stopped && !hurried { give_back = true } else { send_waiting = Some(hurried) }
                    }
                }
            }
            AgentEvent::Disconnected(why) => {
                // The conversation says it only when a turn was cut short: the log keeps every unexpected end.
                if let Some(why) = why {
                    crate::log::error(format!("session {session}: the agent's process ended: {why}"));
                }
                actions.push(Action::CloseLive(session.to_owned()));
                thread.live = false;
                // Gone while a turn of its queue was to come: it never will.
                if thread.busy {
                    thread.end_turn(Some("The agent's process stopped unexpectedly.".to_owned()));
                    thread.more = 0;
                    for waiting in &mut thread.follow_ups {
                        waiting.sent = false;
                    }
                }
                // What it ran in the background went with it: the conversation says so.
                if thread.background_lost() {
                    actions.push(Action::SaveSession(Box::new(thread.session.clone())));
                }
            }
            _ => {}
        }
        if give_back {
            self.restore_follow_ups(session);
        }
        self.touch_summary(session);
        // The turn may have changed files: the side panel reads the repository again.
        if ended && self.current.as_deref() == Some(session) {
            self.refresh_git(actions);
        }
        if out_of_usage {
            self.limit_stopped(session, actions);
        }
        if let Some((text, spoken, files)) = resend
            && self.current.as_deref() == Some(session)
        {
            self.send(&text, spoken, files, actions);
        }
        if let Some(interrupted) = send_waiting {
            self.send_follow_ups(session, interrupted, actions);
        }
        // While its tools run, the agent is handed what the user sent meanwhile.
        self.hand_over(session, actions);
    }

    /// Stores a changed agent; sessions of it that are idle start their
    /// Claude Agent again with the new settings on the next message.
    fn update_agent(&mut self, agent: Agent, actions: &mut Vec<Action>) {
        for thread in self.threads.values_mut().filter(|t| t.session.agent == agent.id && !t.at_work()) {
            thread.live = false;
            actions.push(Action::CloseLive(thread.session.id.clone()));
        }
        match self.agents.iter_mut().find(|a| a.id == agent.id) {
            Some(existing) => *existing = agent.clone(),
            None => self.agents.push(agent.clone()),
        }
        actions.push(Action::SaveAgent(Box::new(agent)));
    }

    /// Saves `agent` (new or changed) and selects it.
    fn save_agent(&mut self, mut agent: Agent, actions: &mut Vec<Action>) {
        agent.updated = unix_now();
        let id = agent.id.clone();
        match self.agents.iter().find(|old| old.id == id).cloned() {
            // Given another provider: the sessions it starts next think through it, with its model where
            // that provider offers it. The sessions it has keep their own.
            Some(old) if old.connection != agent.connection => {
                let to = std::mem::replace(&mut agent.connection, old.connection);
                (agent.model, agent.effort) = (old.model, old.effort);
                let agent = self.moved(agent, &to);
                if let Some(connection) = self.config.connection(&to) {
                    self.want_models(&connection, actions);
                }
                self.update_agent(agent, actions);
            }
            _ => self.update_agent(agent, actions),
        }
        self.select_agent(&id, actions);
    }

    /// Deletes an agent with all its sessions.
    fn delete_agent(&mut self, agent: &str, actions: &mut Vec<Action>) {
        let sessions: Vec<String> = self.sessions.iter().filter(|s| s.agent == agent).map(|s| s.id.clone()).collect();
        for session in sessions {
            // Uncommitted work in a worktree is not thrown away unseen: such a folder is kept and listed.
            self.delete_session(&session, false, actions);
        }
        self.agents.retain(|a| a.id != agent);
        actions.push(Action::DeleteAgent(agent.to_owned()));
        self.agent = None;
        self.current = None;
        match self.agents.first().map(|a| a.id.clone()) {
            Some(next) => self.select_agent(&next, actions),
            None => self.modal = Some(Modal::Agent(Box::new(AgentForm::new(None)))),
        }
    }

    /// Deletes a session and stops whatever it was doing. Its worktree
    /// goes with it: with its uncommitted changes when `discard` (the user
    /// was told of them), and kept as a folder to look at otherwise.
    fn delete_session(&mut self, session: &str, discard: bool, actions: &mut Vec<Action>) {
        if self.call.as_ref().is_some_and(|call| call.session == session) {
            self.end_call(actions);
        }
        actions.push(Action::Stop(session.to_owned()));
        actions.push(Action::CloseLive(session.to_owned()));
        actions.push(Action::DeleteSession(session.to_owned()));
        let worktree = self.sessions.iter().find(|known| known.id == session).and_then(|known| known.worktree.clone());
        if let Some(worktree) = worktree {
            let job = atroxabot_core::worktree::Job::Remove { session: session.to_owned(), worktree, discard };
            actions.push(Action::Worktree(job));
        }
        self.worktrees.remove(session);
        self.threads.remove(session);
        self.sessions.retain(|s| s.id != session);
        self.composer.forget(session);
        if self.current.as_deref() == Some(session) {
            self.current = None;
        }
    }

    /// Opens the session `session` of `agent`, as from a search result.
    fn open_found(&mut self, agent: &str, session: &str, actions: &mut Vec<Action>) {
        if self.agents.iter().any(|a| a.id == agent) {
            self.select_agent(agent, actions);
            self.open_session(session, actions);
        }
    }

    /// The colour `agent` wears.
    fn color_of(agent: &Agent) -> Color {
        theme::agent_color(&agent.name, &agent.color)
    }

    /// Draws the whole interface into `view`.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, scheme: Scheme, actions: &mut Vec<Action>) {
        let t = p.theme;
        p.rect(view, t.panel, 0.0);
        let body = Rect::new(view.x, view.y + theme::TITLE_HEIGHT, view.w, view.h - theme::TITLE_HEIGHT);
        let sidebar = Rect::new(body.x, body.y, theme::SIDEBAR_WIDTH.min(body.w * 0.4), body.h);
        // The side panel takes its width from the session, where the window has it to give;
        // where it has not, the panel floats over the session, under its top bar.
        let room = body.w - sidebar.w - theme::GAP;
        let docked = room >= panel::WIDTH + 520.0;
        let side = if self.config.panel && docked { panel::WIDTH + theme::GAP } else { 0.0 };
        let canvas = Rect::new(sidebar.right(), body.y, room - side, body.h - theme::GAP);
        let floating = (self.config.panel && !docked).then(|| {
            let w = panel::WIDTH.min(canvas.w - 2.0 * theme::GAP);
            let top = canvas.y + theme::HEADER_HEIGHT;
            Rect::new(canvas.right() - w - theme::GAP, top, w, canvas.bottom() - top - theme::GAP)
        });
        self.panel_floats = floating.is_some();
        // A modal takes the mouse from the whole screen; an open list, or else the floating panel, from what is under it.
        let list = self.menu.as_ref().map(|menu| menu.rect);
        // A modal opened by this frame's click must not take that same click for one outside it.
        let modal_was_open = self.modal.is_some();
        ui.blocker = if self.modal.is_some() { Some(view) } else { list.or(floating) };
        // The brand, where a title would be: on macOS the traffic lights sit there.
        if crate::titlebar::CUSTOM {
            let mid = view.y + theme::TITLE_HEIGHT * 0.5 + 1.0;
            p.logo(crate::logo::Logo::App, (view.x + 26.0, mid), 22.0, t.text);
            p.label("AtroxaBot", theme::LABEL, view.x + 42.0, mid - 9.0, t.text_muted);
        }
        self.draw_sidebar(p, ui, sidebar, actions);

        let lifted = Rect::new(canvas.x, canvas.y + 2.0, canvas.w, canvas.h);
        p.shadow(lifted, fade(t.shadow, 0.35), theme::CANVAS_RADIUS, 18.0);
        p.rect(canvas, t.bg, theme::CANVAS_RADIUS);
        let clip = p.push_clip(canvas);
        if self.call.is_some() {
            self.draw_call(p, ui, canvas, actions);
        } else {
            self.draw_chat(p, ui, canvas, actions);
        }
        p.set_clip(clip);
        if side > 0.0 {
            let panel = Rect::new(canvas.right() + theme::GAP, canvas.y, panel::WIDTH, canvas.h);
            self.draw_panel(p, ui, panel, actions);
        } else if let Some(floating) = floating {
            ui.blocker = if self.modal.is_some() { Some(view) } else { list };
            self.draw_panel(p, ui, floating, actions);
            // A click outside it puts it away, but not one on the top bar, whose button does that itself.
            let header = Rect::new(canvas.x, canvas.y, canvas.w, theme::HEADER_HEIGHT);
            let outside = !floating.contains(ui.mouse) && !header.contains(ui.mouse);
            if ui.pressed && outside && self.menu.is_none() && self.modal.is_none() && !modal_was_open {
                self.toggle_panel(actions);
            }
        }

        ui.blocker = list;
        let released = ui.released;
        ui.released = released && modal_was_open;
        self.draw_modal(p, ui, view, scheme, actions);
        ui.released = released;
        ui.blocker = None;
        self.draw_menu(p, ui, actions);
        self.draw_notices(p, ui, canvas);
        // A tooltip lies over everything.
        crate::ui::draw_tip(p, ui, view);
    }

    /// The open modal, over the screen.
    fn draw_modal(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, scheme: Scheme, actions: &mut Vec<Action>) {
        let Some(mut modal) = self.modal.take() else { return };
        let keep = match &mut modal {
            Modal::Agent(form) => match form.draw(p, ui, view, self, actions) {
                None => true,
                Some(agent_form::Done::Saved(agent)) => {
                    self.save_agent(*agent, actions);
                    false
                }
                Some(agent_form::Done::Deleted(agent)) => {
                    self.delete_agent(&agent, actions);
                    false
                }
                Some(agent_form::Done::Cancelled) => false,
            },
            Modal::Settings(settings) => !settings.draw(p, ui, view, self, scheme, actions),
            Modal::Spotlight(spotlight) => match spotlight.draw(p, ui, view, self) {
                None => true,
                Some(spotlight::Outcome::Close) => false,
                Some(spotlight::Outcome::Run(target)) => {
                    self.run_target(target, actions);
                    false
                }
            },
            Modal::Stats(stats) => !stats.draw(p, ui, view, self),
            Modal::Diff(diff) => !diff.draw(p, ui, view),
            Modal::Picture(path) => !picture::draw(p, ui, view, path),
        };
        // Closing it puts away a list it opened; `delete_agent` may have opened another modal.
        if keep {
            self.modal = Some(modal);
        } else {
            self.menu = None;
        }
    }

    /// The session page: a header, the conversation and the composer.
    fn draw_chat(&mut self, p: &mut Painter, ui: &mut Ui, canvas: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        let Some(agent) = self.agent_in_use() else {
            self.draw_first_run(p, ui, canvas);
            return;
        };
        let color = Self::color_of(&agent);
        let header = Rect::new(canvas.x, canvas.y, canvas.w, theme::HEADER_HEIGHT);
        let busy = self.thread().is_some_and(|thread| thread.busy);

        // On a dark canvas the agent's colour glows faintly from the top.
        if !t.is_light() {
            let wash = Rect::new(canvas.x, canvas.y, canvas.w, 260.0_f32.min(canvas.h));
            p.gradient(wash, fade(color, 0.07), fade(color, 0.0), theme::CANVAS_RADIUS);
        }
        // The composer first: the conversation fills what it leaves. A click
        // in the sidebar this frame may have opened another session.
        self.show_draft();
        let composer_top = self.draw_composer(p, ui, canvas, &agent, actions);
        let area = Rect::new(canvas.x, header.bottom(), canvas.w, composer_top - header.bottom());
        let clip = p.push_clip(area);
        let empty = self.thread().is_none_or(|thread| thread.loaded && thread.session.entries.is_empty());
        if empty {
            self.draw_welcome(p, ui, area, &agent, color);
        } else if let Some(session) = self.current.clone()
            && let Some(thread) = self.threads.get_mut(&session)
        {
            thread.draw(p, ui, area, (&agent.name, color), actions);
            // A picture that was clicked opens whole, over the screen.
            if let Some(picture) = thread.clicked_picture.take() {
                self.modal = Some(Modal::Picture(picture));
            }
            // A file a turn changed that was clicked opens to what the turn did to it.
            let changed = thread.clicked_change.take().and_then(|(reply, path)| thread.run_change(reply, &path));
            if let Some(file) = changed {
                self.modal = Some(Modal::Diff(Box::new(panel::DiffView::of_turn(&file))));
            }
        }
        p.set_clip(clip);
        self.draw_slash(p, ui, actions);
        self.draw_header(p, ui, header, &agent, (color, busy), actions);
    }

    /// The bar over a session: who, the session's title (a click renames
    /// it), and at the right the provider it thinks through, the folder it
    /// works in, the call and the side panel.
    fn draw_header(
        &mut self,
        p: &mut Painter,
        ui: &mut Ui,
        header: Rect,
        agent: &Agent,
        (color, busy): (Color, bool),
        actions: &mut Vec<Action>,
    ) {
        let t = p.theme;
        let mid = header.y + header.h * 0.5;
        let mood = if busy { Mood::Thinking } else { Mood::Idle };
        orb::agent(p, &agent.portrait, (header.x + 30.0, mid), 26.0, color, mood, ui.motion());
        let name_w = p.label(&agent.name, theme::HEADING, header.x + 52.0, mid - 10.0, t.text);

        let mut right = header.right() - 12.0;
        let panel = Rect::new(right - 30.0, mid - 15.0, 30.0, 30.0);
        if self.config.panel {
            p.rect(panel, t.hover, theme::RADIUS_SM);
        }
        if icon_button(p, ui, panel, icon::PANEL, true) {
            self.toggle_panel(actions);
        }
        right = panel.x - 2.0;
        let phone = Rect::new(right - 30.0, mid - 15.0, 30.0, 30.0);
        if icon_button(p, ui, phone, icon::PHONE, true) {
            self.start_call(actions);
        }
        right = phone.x - 6.0;

        // Where the session works: a click picks another folder. One in a
        // worktree shows the folder that stands for, on a branch of its own.
        let folder = self.origin_in_use();
        let label = folder_label(&folder);
        let glyph = if self.wants_worktree() { icon::BRANCH } else { icon::FOLDER };
        // On a narrow session the chips' names give way, so the agent's name keeps its place.
        let names = (header.w * 0.16).clamp(60.0, 220.0);
        let width = 58.0 + p.layout(label, theme::SMALL, None).width().min(names);
        let folder_chip =
            self.toolbar_button(p, ui, (right - width, mid - 15.0), (Some(glyph), label, names), Menu::Folder);
        right = folder_chip.x - 6.0;

        // The provider: a click swaps it.
        let connection = self.connection(agent);
        let name = connection.as_ref().map_or("No provider", |connection| connection.name.as_str());
        let mut label = p.layout(name, theme::SMALL, None);
        label.truncate(p.fonts, names.min(180.0));
        let chip = Rect::new(right - label.width() - 58.0, mid - 15.0, label.width() + 58.0, 30.0);
        let open = self.menu_open(Menu::Provider);
        let hovered = ui.hovered(chip);
        let hover = ui.anim(id("provider-chip"), f32::from(u8::from(hovered || open)));
        p.bordered(
            chip,
            mix(fade(t.surface, 0.0), t.hover, hover),
            15.0,
            1.0,
            mix(t.border_strong, t.border_focus, hover),
        );
        let ink = mix(t.text_muted, t.text, hover);
        match &connection {
            Some(connection) => self.provider_mark(p, connection, (chip.x + 18.0, mid), 15.0, ink),
            None => icon::draw(p, icon::ALERT, (chip.x + 18.0, mid), 14.0, ink),
        }
        p.text(&label, chip.x + 32.0, mid - label.height() * 0.5, ink);
        icon::draw(p, icon::CHEVRON_DOWN, (chip.right() - 15.0, mid), 13.0, fade(ink, 0.7));
        if hovered {
            ui.cursor = CursorIcon::Pointer;
            if ui.clicked(chip) {
                self.toggle_menu(Menu::Provider, chip);
            }
        }
        right = chip.x - 14.0;

        let left = header.x + 52.0 + name_w + 12.0;
        self.draw_title(p, ui, Rect::new(left, mid - 14.0, right - left, 28.0), actions);
    }

    /// The open session's title, in `area` of the top bar. A click turns it
    /// into a field: Enter or a click elsewhere keeps what was typed, Esc
    /// the old title.
    fn draw_title(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, actions: &mut Vec<Action>) {
        let t = p.theme;
        if area.w < 80.0 {
            return;
        }
        if let Some(editor) = &mut self.renaming {
            let typed = p.layout(editor.text(), theme::SMALL, None).width();
            let field = Rect::new(area.x, area.y, (typed + 34.0).clamp(180.0_f32.min(area.w), area.w), area.h);
            p.bordered(field, t.surface, theme::RADIUS_SM, 1.0, t.border_focus);
            let inner = Rect::new(field.x + 9.0, field.y, field.w - 18.0, field.h);
            line_field(p, ui, inner, editor, ("Name this session", theme::SMALL), true);
            if ui.pressed && !ui.hovered(field) {
                self.finish_rename(true, actions);
            }
            return;
        }
        let Some(title) = self.thread().map(|thread| thread.session.title.clone()) else { return };
        let mut text = p.layout(if title.is_empty() { "Untitled" } else { &title }, theme::SMALL, None);
        text.truncate(p.fonts, area.w - 18.0);
        let rect = Rect::new(area.x, area.y, text.width() + 18.0, area.h);
        let hovered = ui.hovered(rect);
        let hover = ui.anim(id("session-title"), f32::from(u8::from(hovered)));
        p.rect(rect, fade(t.hover, hover), theme::RADIUS_SM);
        p.text(&text, rect.x + 9.0, rect.y + (rect.h - text.height()) * 0.5 + 0.5, mix(t.text_faint, t.text, hover));
        if hovered {
            ui.cursor = CursorIcon::Text;
            if ui.clicked(rect) {
                let mut editor = Editor::default();
                editor.insert(&title);
                editor.select_all();
                self.renaming = Some(editor);
                ui.last_edit = ui.time;
            }
        }
    }

    /// The canvas before there is any agent: an invitation to make one.
    fn draw_first_run(&mut self, p: &mut Painter, ui: &mut Ui, canvas: Rect) {
        let t = p.theme;
        let center = canvas.x + canvas.w * 0.5;
        let top = canvas.y + canvas.h * 0.5 - 130.0;
        p.logo(crate::logo::Logo::App, (center, top + 44.0), 150.0, t.text);
        let title = p.layout("Make your first agent", theme::TITLE, None);
        p.text(&title, center - title.width() * 0.5, top + 110.0, t.text);
        let about =
            "Give it a name and say what it is for. You can chat with it, call it, and give it a folder to work in.";
        let about = p.layout(about, theme::BODY, Some(440.0));
        p.text_aligned(&about, center - 220.0, top + 148.0, crate::text::Align::Center, 440.0, t.text_muted);
        let create = Rect::new(center - 80.0, top + 162.0 + about.height(), 160.0, 38.0);
        if button(p, ui, create, "New agent", ButtonStyle::Primary, true) {
            self.modal = Some(Modal::Agent(Box::new(AgentForm::new(None))));
        }
    }

    /// The canvas of an agent nobody has written to yet: its face and how it works.
    fn draw_welcome(&mut self, p: &mut Painter, ui: &mut Ui, area: Rect, agent: &Agent, color: Color) {
        let t = p.theme;
        let center = area.x + area.w * 0.5;
        let top = area.y + (area.h * 0.5 - 150.0).max(24.0);
        let appear = ui.appear(id(("welcome", &agent.id)));
        let face = (center, top + 48.0 - 8.0 * (1.0 - appear));
        orb::agent(p, &agent.portrait, face, 84.0, color, Mood::Idle, ui.motion());
        let name = p.layout(&agent.name, theme::TITLE, None);
        p.text(&name, center - name.width() * 0.5, top + 112.0, fade(t.text, appear));
        let mut y = top + 112.0 + name.height() + 6.0;
        if !agent.tagline.trim().is_empty() {
            let tagline = p.layout(agent.tagline.trim(), theme::BODY, Some((area.w - 64.0).min(520.0)));
            p.text_aligned(&tagline, center - 260.0, y, crate::text::Align::Center, 520.0, fade(t.text_muted, appear));
            y += tagline.height() + 6.0;
        }
        // Three facts worth knowing before the first message.
        let connection = self.connection(agent);
        let thinks = connection.as_ref().map_or_else(
            || "No provider".to_owned(),
            |connection| format!("{} · {}", connection.name, self.model_in_use(connection, &agent.model).label),
        );
        let folder = self.origin_in_use();
        let folder = if folder.is_empty() { "Your home folder".to_owned() } else { folder };
        let folder = if self.wants_worktree() { format!("{folder}, in a worktree of its own") } else { folder };
        let facts =
            [(icon::SPARKLES, thinks), (icon::FOLDER, folder), (icon::SHIELD, agent.autonomy.label().to_owned())];
        y += 18.0;
        for (glyph, fact) in facts {
            let mut line = p.layout(&fact, theme::SMALL, None);
            line.truncate(p.fonts, (area.w - 120.0).min(520.0));
            let x = center - f32::midpoint(line.width(), 24.0);
            // What it thinks through wears that provider's mark.
            match connection.as_ref().filter(|_| glyph == icon::SPARKLES) {
                Some(connection) => {
                    self.provider_mark(p, connection, (x + 7.0, y + 9.0), 14.0, fade(t.text_faint, appear));
                }
                None => icon::draw(p, glyph, (x + 7.0, y + 9.0), 14.0, fade(t.text_faint, appear)),
            }
            p.text(&line, x + 24.0, y, fade(t.text_faint, appear));
            y += 26.0;
        }
    }

    /// Problems, stacked under the header until dismissed.
    fn draw_notices(&mut self, p: &mut Painter, ui: &mut Ui, canvas: Rect) {
        let t = p.theme;
        let width = 460.0_f32.min(canvas.w - 32.0);
        let x = canvas.x + ((canvas.w - width) * 0.5).round();
        let mut y = canvas.y + theme::HEADER_HEIGHT + 8.0;
        let mut close = None;
        for (index, notice) in self.notices.iter().enumerate() {
            let text = p.layout(&notice.text, theme::SMALL, Some(width - 60.0));
            let card = Rect::new(x, y, width, text.height() + 24.0);
            p.shadow(Rect::new(card.x, card.y + 4.0, card.w, card.h), t.shadow, theme::RADIUS, 14.0);
            p.bordered(card, t.surface, theme::RADIUS, 1.0, fade(t.danger, 0.5));
            icon::draw(p, icon::ALERT, (card.x + 18.0, card.y + 21.0), 15.0, t.danger);
            p.text(&text, card.x + 34.0, card.y + 12.0, t.text);
            if icon_button(p, ui, Rect::new(card.right() - 32.0, card.y + 7.0, 26.0, 26.0), icon::X, true) {
                close = Some(index);
            }
            y = card.bottom() + 8.0;
        }
        if let Some(index) = close {
            self.notices.remove(index);
        }
    }

    /// A key was pressed.
    pub fn key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<Clipboard>,
        actions: &mut Vec<Action>,
    ) {
        let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
        self.show_draft();
        if self.menu.is_some() {
            self.menu_key(event, mods, clipboard, actions);
            return;
        }
        let escape = matches!(event.logical_key, Key::Named(NamedKey::Escape));
        let enter = matches!(event.logical_key, Key::Named(NamedKey::Enter));
        if let Some(mut modal) = self.modal.take() {
            let mut run = None;
            let close = match &mut modal {
                Modal::Agent(form) => form.key(event, mods, clipboard),
                Modal::Settings(settings) => settings.key(event, mods, clipboard),
                Modal::Spotlight(spotlight) => match spotlight.key(event, mods, clipboard, self, actions) {
                    None => false,
                    Some(spotlight::Outcome::Close) => true,
                    Some(spotlight::Outcome::Run(target)) => {
                        run = Some(target);
                        true
                    }
                },
                Modal::Stats(_) | Modal::Diff(_) | Modal::Picture(_) => escape,
            };
            if !close {
                self.modal = Some(modal);
            }
            if let Some(target) = run {
                self.run_target(target, actions);
            }
            return;
        }
        // Shortcuts of the screen itself.
        if primary && let Key::Character(c) = &event.logical_key {
            match c.as_str() {
                "," => {
                    self.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::General))));
                    return;
                }
                "k" | "K" => {
                    self.modal = Some(Modal::Spotlight(Box::default()));
                    return;
                }
                "n" | "N" if mods.shift_key() => {
                    self.modal = Some(Modal::Agent(Box::new(AgentForm::new(None))));
                    return;
                }
                "n" | "N" => {
                    self.new_session();
                    return;
                }
                "f" | "F" if self.renaming.is_none() => {
                    self.find();
                    return;
                }
                _ => {}
            }
        }
        if let Some(editor) = &mut self.renaming {
            // The session's title, being changed in the top bar.
            if escape || enter {
                self.finish_rename(enter, actions);
            } else {
                edit_key(editor, event, mods, clipboard);
            }
        } else if self.find_key(event, mods, clipboard) {
            // The bar that finds text in the conversation had the keyboard, or Esc put it away.
        } else if escape && self.panel_floats && self.config.panel {
            // The side panel floating over the session goes first.
            self.toggle_panel(actions);
        } else if self.call.is_some() {
            if escape {
                self.end_call(actions);
            } else if matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("m")) {
                self.toggle_mute(actions);
            }
        } else {
            self.chat_key(event, mods, clipboard, actions);
        }
    }

    /// Whether the bar that finds text in the open conversation has the keyboard.
    pub(super) fn finding(&self) -> bool {
        self.call.is_none() && self.thread().and_then(|thread| thread.find.as_ref()).is_some_and(|find| find.focused)
    }

    /// Ctrl+F: opens that bar over the open conversation, or gives it the
    /// keyboard again. What is marked in the conversation (a line of it at
    /// most) is what it looks for first.
    fn find(&mut self) {
        if self.call.is_some() {
            return;
        }
        let Some(thread) = self.thread_mut().filter(|thread| thread.loaded && !thread.session.entries.is_empty())
        else {
            return;
        };
        let marked = thread.selected_text().filter(|marked| !marked.contains('\n') && marked.chars().count() <= 80);
        match (&mut thread.find, marked) {
            (Some(find), None) => {
                find.focused = true;
                find.editor.select_all();
            }
            (_, marked) => thread.open_find(&marked.unwrap_or_default(), true, false),
        }
    }

    /// A key while that bar is open: its own while it has the keyboard,
    /// and Esc puts it away either way. Returns whether the key was taken.
    fn find_key(&mut self, event: &KeyEvent, mods: ModifiersState, clipboard: &mut Option<Clipboard>) -> bool {
        if self.call.is_some() {
            return false;
        }
        let escape = matches!(event.logical_key, Key::Named(NamedKey::Escape));
        let Some(thread) = self.thread_mut() else { return false };
        let Some(find) = &mut thread.find else { return false };
        if find.focused {
            if find.key(event, mods, clipboard) == find::Typed::Closed {
                thread.find = None;
            }
            true
        } else if escape {
            thread.find = None;
            true
        } else {
            false
        }
    }

    /// A key on the session page: approvals first, then the composer.
    fn chat_key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<Clipboard>,
        actions: &mut Vec<Action>,
    ) {
        // A question of the agent's takes the keys until it is answered.
        if let Some(ask) = self.thread().and_then(|thread| thread.asks.first()).cloned()
            && self.asks_question(&ask)
        {
            let verdict = self.question.as_mut().and_then(|form| form.key(event, mods, clipboard, &ask.input));
            if let Some(verdict) = verdict {
                self.question = None;
                self.decide(verdict, actions);
            }
            return;
        }
        let asking = self.thread().is_some_and(|thread| !thread.asks.is_empty());
        let empty = self.composer.editor.text().is_empty();
        let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
        let nothing_marked = self.composer.editor.selection().is_empty();
        if self.slash_key(event) || self.mention_key(event) {
            // A folder that was taken lists what is in it.
            self.mention_typed(actions);
            return;
        }
        let guessed = self.suggestion_shown().is_some();
        match &event.logical_key {
            // The message box shows what the agent guesses comes next: Tab writes it in, Esc puts it away.
            Key::Named(NamedKey::Tab) if guessed && !mods.shift_key() => self.take_suggestion(),
            Key::Named(NamedKey::Escape) if guessed && !asking => self.drop_suggestion(),
            // A paste with no text on the clipboard brings what else is there: files, or a picture.
            Key::Character(c) if primary && c.eq_ignore_ascii_case("v") => {
                if clipboard.is_none() {
                    *clipboard = Clipboard::new().ok();
                }
                let words = clipboard.as_mut().and_then(|clipboard| clipboard.get_text().ok());
                if words.is_none_or(|words| words.is_empty()) {
                    actions.push(Action::PasteFiles);
                } else {
                    self.composer.key(event, mods, clipboard);
                }
            }
            // Copy takes what is selected in the conversation, unless the draft has a selection.
            Key::Character(c) if primary && c.eq_ignore_ascii_case("c") && nothing_marked => {
                if let Some(text) = self.thread().and_then(Thread::selected_text) {
                    actions.push(Action::Copy(text));
                }
            }
            // In an empty message box the arrows go through what was sent before, as in a terminal.
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown))
                if !primary && !mods.shift_key() && !mods.alt_key() && self.recall(*key == NamedKey::ArrowUp) => {}
            // With nothing typed, Enter and Esc answer a waiting approval.
            Key::Named(NamedKey::Enter) if asking && empty => self.decide(Verdict::Allow, actions),
            Key::Named(NamedKey::Escape) if asking => self.deny(actions),
            Key::Named(NamedKey::Escape) => self.stop(actions),
            // While the agent works, Enter puts the message in line for its next step,
            // and Ctrl/Cmd+Enter has it read at once.
            Key::Named(NamedKey::Enter) if !mods.shift_key() => {
                let busy = self.thread().is_some_and(|thread| thread.busy);
                let (text, files) = self.take_draft();
                self.send(&text, false, files, actions);
                if busy
                    && primary
                    && let Some(session) = self.current.clone()
                {
                    self.hurry(&session, actions);
                }
            }
            _ => self.composer.key(event, mods, clipboard),
        }
        self.mention_typed(actions);
    }

    /// Text committed by an input method.
    pub fn ime_commit(&mut self, text: &str, actions: &mut Vec<Action>) {
        self.show_draft();
        let (answering, finding) = (self.answering(), self.finding());
        match &mut self.modal {
            Some(Modal::Agent(form)) => form.commit(text),
            Some(Modal::Settings(settings)) => settings.commit(text),
            Some(Modal::Spotlight(spotlight)) => spotlight.insert(text, actions),
            Some(Modal::Stats(_) | Modal::Diff(_) | Modal::Picture(_)) => {}
            None => match &mut self.renaming {
                Some(editor) => editor.insert(text),
                None if answering => {
                    if let Some(form) = &mut self.question {
                        form.insert(text);
                    }
                }
                None if finding => {
                    let open = self.current.as_deref().and_then(|id| self.threads.get_mut(id));
                    if let Some(find) = open.and_then(|thread| thread.find.as_mut()) {
                        find.editor.insert(text);
                    }
                }
                None => self.composer.commit(text),
            },
        }
        self.mention_typed(actions);
    }

    /// Text an input method is composing, with its cursor.
    pub fn ime_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) {
        if self.modal.is_none() && self.renaming.is_none() && !self.finding() {
            self.composer.preedit = if text.is_empty() { None } else { Some((text, cursor)) };
        }
    }

    /// Where the caret is, for the input method's window.
    pub fn ime_area(&self) -> Option<Rect> {
        match &self.modal {
            Some(Modal::Agent(form)) => form.caret(),
            Some(Modal::Settings(settings)) => settings.caret(),
            Some(_) => None,
            None => self.composer.caret,
        }
    }

    /// Something happened on the call.
    pub fn call_event(&mut self, event: CallEvent, actions: &mut Vec<Action>) {
        let Some(call) = &mut self.call else { return };
        match event {
            CallEvent::State(state) => call.state = state,
            CallEvent::Level { input, output } => (call.input, call.output) = (input, output),
            CallEvent::Utterance(text) => {
                call.heard.clone_from(&text);
                self.send(&text, true, Vec::new(), actions);
            }
            CallEvent::BargeIn => self.barge_in(actions),
            CallEvent::Problem(text) => self.notify(text),
            CallEvent::Ended(reason) => {
                self.end_call(actions);
                if let Some(reason) = reason {
                    self.notify(format!("The call ended: {reason}"));
                }
            }
        }
    }
}

/// The provider the turn of `thread` runs on, for the usage log: the one
/// its message went to, whatever the session or its agent was given since.
fn ran_on(thread: &Thread, agents: &[Agent]) -> String {
    let own = || agents.iter().find(|agent| agent.id == thread.session.agent).map(|agent| &agent.connection);
    let known = [&thread.sent_on, &thread.session.provider].into_iter().find(|provider| !provider.is_empty());
    known.or_else(own).cloned().unwrap_or_default()
}

/// Takes what a turn just reported of an account's limits into what is
/// `known` of them. A turn only reports the windows its model counts
/// against (the weekly one for Fable only on a Fable turn), so a window
/// not named this time keeps what it last said.
fn merge_limits(known: &mut Limits, new: Limits) {
    (known.at, known.status, known.resets) = (new.at, new.status, new.resets);
    for mut window in new.windows {
        match known.windows.iter_mut().find(|old| old.name == window.name) {
            Some(old) => {
                // A reply does not say which model a week of its own is for; a check did.
                if window.label.is_empty() {
                    window.label = std::mem::take(&mut old.label);
                }
                *old = window;
            }
            None => known.windows.push(window),
        }
    }
    order_windows(&mut known.windows);
}

/// Puts a plan's windows in the order they are shown: the short window
/// first, then the week, then the week's per-model ones.
fn order_windows(windows: &mut [LimitWindow]) {
    windows.sort_by_key(|window| (window.name != "five_hour", window.name != "seven_day", window.name.clone()));
}

/// When `then` is, seen from `now`: "now", "in 12m", "in 3h 12m", "in 2d 4h".
fn until(now: u64, then: u64) -> String {
    let minutes = then.saturating_sub(now) / 60;
    match minutes {
        0 => "now".to_owned(),
        1..60 => format!("in {minutes}m"),
        60..1440 => format!("in {}h {}m", minutes / 60, minutes % 60),
        _ => format!("in {}d {}h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// A count of tokens, short: "840", "12.4k", "3.1M".
fn compact(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1e6)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1e3)
    } else {
        count.to_string()
    }
}

/// An amount in USD: "$0.42", or "$128" from a hundred on.
fn money(usd: f64) -> String {
    if usd >= 100.0 { format!("${usd:.0}") } else { format!("${usd:.2}") }
}

/// "now", "5m", "3h", "2d" or "4w": how long ago `then` was.
fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        86_400..604_800 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

/// When a turn started, for "12s".
fn elapsed(since: Option<Instant>) -> u64 {
    since.map_or(0, |since| since.elapsed().as_secs())
}

/// A folder as a chip shows it: its name, or "Home" for none.
fn folder_label(folder: &str) -> &str {
    if folder.is_empty() { "Home" } else { folder_name(folder) }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::Entry;

    use super::*;

    #[test]
    fn times_and_usage_read_well() {
        let times = (ago(100, 100), ago(100, 30), ago(7300, 100), ago(200_000, 0));
        assert_eq!(times, ("now".into(), "1m".into(), "2h".into(), "2d".into()));
        assert_eq!(ago(0, 100), "now", "a clock that went back");
        assert_eq!((compact(840), compact(12_400), compact(3_100_000)), ("840".into(), "12.4k".into(), "3.1M".into()));
        assert_eq!((money(0.4249), money(128.4)), ("$0.42".into(), "$128".into()));
        // A turn on another model adds its window; the others keep what they last said.
        let window = |name: &str, used: f64| atroxabot_core::LimitWindow {
            name: name.into(),
            used,
            resets: 9,
            label: String::new(),
        };
        let mut known = Limits::default();
        let fable =
            vec![window("seven_day_overage_included", 0.42), window("seven_day", 0.61), window("five_hour", 0.28)];
        merge_limits(&mut known, Limits { at: 1, status: "allowed".into(), resets: 9, windows: fable });
        merge_limits(
            &mut known,
            Limits { at: 2, status: "allowed_warning".into(), resets: 9, windows: vec![window("five_hour", 0.3)] },
        );
        let shown: Vec<(&str, f64)> = known.windows.iter().map(|w| (w.name.as_str(), w.used)).collect();
        assert_eq!(shown, [("five_hour", 0.3), ("seven_day", 0.61), ("seven_day_overage_included", 0.42)]);
        assert_eq!((known.status.as_str(), known.at), ("allowed_warning", 2));
        let times = [until(100, 90), until(0, 720), until(0, 11_520), until(0, 187_200)];
        assert_eq!(times, ["now", "in 12m", "in 3h 12m", "in 2d 4h"]);
        assert_eq!((folder_label(""), folder_label("C:\\code\\parser")), ("Home", "parser"));
    }

    fn signed_in(shell: &mut Shell) {
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        shell.claude_info(Connection::CLAUDE_AGENT, Ok(info));
    }

    #[test]
    fn a_turn_runs_from_prompt_to_saved_reply() {
        let agent = Agent::new("Scout");
        let mut shell = Shell::new(Config::default(), vec![agent.clone()], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::ProbeClaude(_))), "Claude Code is asked what it offers");
        signed_in(&mut shell);
        shell.set_folder("/work", &mut actions);
        shell.send("Find the bug", false, Vec::new(), &mut actions);
        let session = shell.current.clone().expect("a session was started");
        let sent = |a: &Action, expected: &str| matches!(a, Action::Send { text, session, .. } if text == expected && session.workdir == "/work");
        assert!(actions.iter().any(|a| sent(a, "Find the bug")), "the session works in the chosen folder");
        assert!(shell.is_busy() && shell.sessions[0].title == "Find the bug");
        assert_eq!(shell.config.recent_folders, ["/work"]);

        // A second message while busy waits for the turn to end.
        shell.send("and the other one", false, Vec::new(), &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::Text("Found it.".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(actions.iter().any(|a| sent(a, "and the other one")));
        assert_eq!(shell.sessions.len(), 1);

        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("It broke.".into()) }, &mut actions);
        assert!(!shell.is_busy());
        shell.delete_agent(&agent.id, &mut actions);
        assert!(shell.sessions.is_empty() && matches!(shell.modal, Some(Modal::Agent(_))), "nobody left: make someone");
    }

    #[test]
    fn sessions_keep_their_place_until_the_user_moves_them() {
        let (scout, other) = (Agent::new("Scout"), Agent::new("Other"));
        let summary = |id: &str, agent: &Agent, updated| SessionSummary {
            id: id.into(),
            agent: agent.id.clone(),
            updated,
            ..SessionSummary::default()
        };
        // As the store lists them: newest first.
        let saved = vec![
            summary("c", &scout, 30),
            summary("x", &other, 25),
            summary("b", &scout, 20),
            summary("a", &scout, 10),
        ];
        let order = |shell: &Shell| shell.sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>().join(" ");
        let mut shell = Shell::new(Config::default(), vec![scout.clone(), other.clone()], saved.clone());
        let mut actions = Vec::new();
        signed_in(&mut shell);
        shell.start(&mut actions);
        assert_eq!(shell.current.as_deref(), Some("c"), "the one that changed last opens");

        // A session at work stays where it is, however much it says.
        shell
            .threads
            .insert("a".into(), Thread::new(Session { id: "a".into(), agent: scout.id.clone(), ..Session::default() }));
        shell.agent_event("a", AgentEvent::Text("Still here.".into()), &mut actions);
        assert_eq!(order(&shell), "c x b a");
        assert_eq!(shell.sessions[3].preview, "Still here.", "though what it shows is current");

        // A drag moves one among its agent's sessions only, down or up, and never past the ends.
        shell.move_session("c", 2);
        assert_eq!(order(&shell), "x b a c");
        shell.move_session("c", 0);
        assert_eq!(order(&shell), "x c b a");
        shell.move_session("b", 9);
        assert_eq!(order(&shell), "x c a b");
        shell.move_session("gone", 0);
        actions.clear();
        shell.keep_order(&mut actions);
        assert!(matches!(&actions[..], [Action::SaveConfig]));
        assert_eq!(shell.config.session_order, ["x", "c", "a", "b"]);

        // The next start has them in that order, with one started since on top; the latest still opens.
        let mut saved = saved;
        saved.insert(0, summary("new", &scout, 40));
        saved[1].updated = 5;
        let mut again = Shell::new(shell.config.clone(), vec![scout, other], saved);
        assert_eq!(order(&again), "new x c a b");
        again.start(&mut Vec::new());
        assert_eq!(again.current.as_deref(), Some("new"));
    }

    /// Draws one frame of `shell` at 1440 by 900 with the mouse at `mouse`,
    /// released there this frame when `click`.
    fn frame(shell: &mut Shell, ui: &mut Ui, mouse: (f32, f32), click: bool) {
        (ui.mouse, ui.press_pos, ui.released) = (mouse, mouse, click);
        paint(shell, ui);
    }

    /// Draws one frame of `shell` at 1440 by 900 with the mouse as `ui` has it.
    fn paint(shell: &mut Shell, ui: &mut Ui) {
        let fonts = crate::text::Fonts::load();
        let (mut atlas, mut images) = (crate::text::GlyphAtlas::default(), crate::image::ImageAtlas::default());
        let mut instances = Vec::new();
        let view = Rect::new(0.0, 0.0, 1440.0, 900.0);
        let palette = Scheme::Black.palette();
        ui.begin(0.016, 1.0);
        let mut p = Painter::new(&fonts, (&mut atlas, &mut images), &mut instances, 1.0, view, palette);
        shell.draw(&mut p, ui, view, Scheme::Black, &mut Vec::new());
        ui.end();
    }

    #[test]
    fn a_session_is_dragged_to_its_place_and_a_click_still_opens_one() {
        let scout = Agent::new("Scout");
        let summary = |id: &str, updated| SessionSummary {
            id: id.into(),
            agent: scout.id.clone(),
            title: id.into(),
            updated,
            ..SessionSummary::default()
        };
        let saved = vec![summary("c", 30), summary("b", 20), summary("a", 10)];
        let mut shell = Shell::new(Config::default(), vec![scout.clone()], saved);
        let order = |shell: &Shell| shell.sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>().join(" ");
        let mut ui = Ui::default();
        // The sessions hang under their agent and "New session": rows of 30, the first from 154 down.
        let row = |index: u8| (120.0, 168.0 + 30.0 * f32::from(index));

        // Held and moved a hair, it is still a click: nothing moves.
        ui.mouse = row(0);
        ui.press(1.0);
        paint(&mut shell, &mut ui);
        ui.mouse.1 += 2.0;
        paint(&mut shell, &mut ui);
        assert!(matches!(&shell.drag, Some((session, false)) if session == "c"));
        // Carried down two rows, the others make room; let go, that is its place.
        ui.mouse = row(2);
        paint(&mut shell, &mut ui);
        paint(&mut shell, &mut ui);
        assert_eq!(order(&shell), "b a c");
        ui.mouse = row(1);
        paint(&mut shell, &mut ui);
        assert_eq!(order(&shell), "b c a");
        (ui.down, ui.released) = (false, true);
        paint(&mut shell, &mut ui);
        assert_eq!(shell.config.session_order, ["b", "c", "a"], "the order is kept with the settings");
        assert!(shell.drag.is_none() && shell.current.is_none(), "letting go opens nothing");

        // A plain click opens the session under it, and leaves the order alone.
        ui.mouse = row(0);
        ui.press(3.0);
        paint(&mut shell, &mut ui);
        (ui.down, ui.released) = (false, true);
        paint(&mut shell, &mut ui);
        assert_eq!((shell.current.as_deref(), order(&shell).as_str()), (Some("b"), "b c a"));
        assert!(shell.drag.is_none());
    }

    #[test]
    fn a_modal_survives_the_click_that_opens_it() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut ui = Ui::default();
        frame(&mut shell, &mut ui, (-1.0, -1.0), false);
        assert!(shell.modal.is_none());
        // Settings, the first icon at the foot of the sidebar.
        frame(&mut shell, &mut ui, (23.0, 877.0), true);
        assert!(matches!(shell.modal, Some(Modal::Settings(_))), "the click opens the settings");
        frame(&mut shell, &mut ui, (23.0, 877.0), false);
        assert!(shell.modal.is_some(), "and they stay open");
        // A click inside leaves them open; one outside closes them.
        frame(&mut shell, &mut ui, (720.0, 450.0), true);
        assert!(shell.modal.is_some());
        frame(&mut shell, &mut ui, (1400.0, 880.0), true);
        assert!(shell.modal.is_none(), "a click outside closes them");

        // The same for the agent editor, opened from the plus over the agents.
        frame(&mut shell, &mut ui, (237.0, 52.0), true);
        assert!(matches!(shell.modal, Some(Modal::Agent(_))), "the plus opens the editor");
        frame(&mut shell, &mut ui, (237.0, 52.0), false);
        assert!(shell.modal.is_some());
        frame(&mut shell, &mut ui, (1400.0, 880.0), true);

        // And for the spotlight, the statistics and the side panel's switch.
        frame(&mut shell, &mut ui, (207.0, 52.0), true);
        assert!(matches!(shell.modal, Some(Modal::Spotlight(_))), "the magnifier opens the spotlight");
        frame(&mut shell, &mut ui, (207.0, 52.0), false);
        assert!(shell.modal.is_some());
        frame(&mut shell, &mut ui, (1400.0, 880.0), true);
        frame(&mut shell, &mut ui, (55.0, 877.0), true);
        assert!(matches!(shell.modal, Some(Modal::Stats(_))), "the chart opens the statistics");
        frame(&mut shell, &mut ui, (1400.0, 880.0), false);
        frame(&mut shell, &mut ui, (1400.0, 880.0), true);
        assert!(shell.modal.is_none() && !shell.config.panel);
        frame(&mut shell, &mut ui, (1405.0, 62.0), true);
        assert!(shell.config.panel, "the last icon of the top bar opens the side panel");
    }

    #[test]
    fn a_session_is_renamed_and_its_effort_is_never_left_open() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        info.models = vec![atroxabot_core::ClaudeModel {
            value: "default".into(),
            resolved: "claude-sonnet-5-5".into(),
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            ..atroxabot_core::ClaudeModel::default()
        }];
        shell.claude_info(Connection::CLAUDE_AGENT, Ok(info));
        shell.send("Find the bug", false, Vec::new(), &mut actions);
        let effort = actions.iter().find_map(|a| match a {
            Action::Send { agent, .. } => Some(agent.effort.clone()),
            _ => None,
        });
        assert_eq!(effort.as_deref(), Some("high"), "Sonnet starts at high; nothing is left to an auto level");

        let rename = |shell: &mut Shell, text: &str, keep: bool| {
            let mut editor = Editor::default();
            editor.insert(text);
            shell.renaming = Some(editor);
            let mut actions = Vec::new();
            shell.finish_rename(keep, &mut actions);
            actions.iter().any(|a| matches!(a, Action::SaveSession(_)))
        };
        assert!(rename(&mut shell, "  Parser   bug ", true), "the new title is saved");
        assert_eq!(
            (shell.sessions[0].title.as_str(), shell.thread().unwrap().session.title.as_str()),
            ("Parser bug", "Parser bug")
        );
        assert!(
            !rename(&mut shell, "Dropped", false) && !rename(&mut shell, "   ", true),
            "Esc and an empty name keep the old one"
        );
        assert_eq!(shell.sessions[0].title, "Parser bug");
    }

    #[test]
    fn what_a_reply_used_is_logged_apart_from_its_session() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        signed_in(&mut shell);
        shell.send("Find the bug", false, Vec::new(), &mut actions);
        let session = shell.current.clone().expect("a session");
        actions.clear();
        let call =
            atroxabot_core::ToolUse { id: "t".into(), name: "Read".into(), ..atroxabot_core::ToolUse::default() };
        shell.agent_event(&session, AgentEvent::ToolCall(call), &mut actions);
        let ended = AgentEvent::ReplyEnded {
            raw: Vec::new(),
            wire: atroxabot_core::Wire::default(),
            model: "claude-opus-5-5".into(),
            usage: atroxabot_core::Usage::new(1_200, 40),
            cost: 0.02,
            problem: String::new(),
        };
        shell.agent_event(&session, ended, &mut actions);
        let logged: Vec<_> =
            actions.iter().filter_map(|a| if let Action::LogUsage(record) = a { Some(record) } else { None }).collect();
        assert_eq!(logged.len(), 1, "one line per reply");
        let record = logged[0];
        assert_eq!((record.provider.as_str(), record.provider_name.as_str()), (Connection::CLAUDE_AGENT, "Claude"));
        assert_eq!((record.input, record.output, record.prompts, record.tools.len()), (1_200, 40, 1, 1));
        assert_eq!(record.session, session);
        // A turn that used nothing (a command, a dropped connection) logs nothing.
        actions.clear();
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.send("/compact", false, Vec::new(), &mut actions);
        actions.clear();
        let empty = AgentEvent::ReplyEnded {
            raw: Vec::new(),
            wire: atroxabot_core::Wire::default(),
            model: String::new(),
            usage: atroxabot_core::Usage::default(),
            cost: 0.0,
            problem: String::new(),
        };
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        shell.agent_event(&session, empty, &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::LogUsage(_))));
        // Limits are kept as they come in.
        let limits = Limits { at: 5, status: "allowed".into(), ..Limits::default() };
        shell.agent_event(&session, AgentEvent::Limits(limits), &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::SaveLimits(kept) if kept.len() == 1)));
    }

    #[test]
    fn a_turn_claude_code_starts_by_itself_shows_as_working() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        signed_in(&mut shell);
        shell.send("Start the build in the background", false, Vec::new(), &mut actions);
        let session = shell.current.clone().unwrap();
        shell.agent_event(&session, AgentEvent::Text("Started.".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(!shell.is_busy());
        // The build ends; Claude Code says so, starts a turn of its own and thinks before it writes.
        let note = "A background task finished: Build it";
        shell.agent_event(&session, AgentEvent::Note(note.into()), &mut actions);
        shell.agent_event(&session, AgentEvent::Working, &mut actions);
        assert!(shell.is_busy(), "working from the moment Claude Code says so");
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        shell.agent_event(&session, AgentEvent::Status("Thinking…".into()), &mut actions);
        let thread = &shell.threads[&session];
        assert!(thread.busy && thread.status == "Thinking…", "working, and saying what at");
        let entries = &thread.session.entries;
        assert!(matches!(&entries[entries.len() - 2], Entry::Note { text } if text == note), "why it went on");
        assert!(matches!(entries.last(), Some(Entry::Reply(reply)) if reply.blocks.is_empty()));
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(!shell.is_busy());
    }

    #[test]
    fn another_claude_account_is_told_what_was_said() {
        let agent = Agent::new("Scout");
        let mut config = Config::default();
        config.connections.push(Connection {
            id: "work".into(),
            config_dir: "/accounts/work".into(),
            ..Connection::claude_agent()
        });
        let mut shell = Shell::new(config, vec![agent], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        signed_in(&mut shell);
        shell.send("Hello", false, Vec::new(), &mut actions);
        let session = shell.current.clone().unwrap();
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::Text("Hi there.".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // The same account simply carries on.
        actions.clear();
        shell.send("More", false, Vec::new(), &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Send { text, .. } if text == "More")));
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // The other account has no history of it: it gets the gist, and its own session.
        shell.choose(Menu::Provider, "work", &mut actions);
        let mut info = ClaudeInfo::default();
        info.account.email = "me@work.example".into();
        shell.claude_info("work", Ok(info));
        actions.clear();
        shell.send("And now?", false, Vec::new(), &mut actions);
        let taken_over = actions.iter().find_map(|a| match a {
            Action::Send { text, session, .. } => Some((text.clone(), session.remote.clone(), session.account.clone())),
            _ => None,
        });
        let (text, remote, account) = taken_over.expect("the message is sent");
        assert!(text.contains("User: Hello") && text.contains("You: Hi there.") && text.ends_with("And now?"));
        assert_eq!((remote.as_str(), account.as_str()), ("", "work"));
    }

    #[test]
    fn each_session_keeps_its_model_effort_and_draft() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let mut info = ClaudeInfo::default();
        info.account.email = "me@example.com".into();
        let model = |value: &str, resolved: &str| atroxabot_core::ClaudeModel {
            value: value.into(),
            resolved: resolved.into(),
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            ..atroxabot_core::ClaudeModel::default()
        };
        info.models = vec![model("default", "claude-sonnet-5-5"), model("haiku", "claude-haiku-5-5")];
        shell.claude_info(Connection::CLAUDE_AGENT, Ok(info));
        let sent = |actions: &[Action]| {
            actions.iter().find_map(|a| match a {
                Action::Send { agent, .. } => Some((agent.model.clone(), agent.effort.clone())),
                _ => None,
            })
        };

        // The first session answers with Haiku at low effort.
        shell.send("One", false, Vec::new(), &mut actions);
        let first = shell.current.clone().unwrap();
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.choose(Menu::Model, "haiku", &mut actions);
        let mut agent = shell.agent_in_use().unwrap();
        agent.effort = "low".into();
        shell.choose_thinking(agent, &mut actions);
        shell.show_draft();
        shell.composer.editor.insert("half a thought");

        // A second one is set back to the default model; the first keeps its own.
        shell.new_session();
        shell.show_draft();
        assert!(shell.composer.editor.text().is_empty(), "a new session starts with an empty message box");
        actions.clear();
        shell.send("Two", false, Vec::new(), &mut actions);
        assert_eq!(sent(&actions), Some(("haiku".into(), "low".into())), "a new session starts with the last choice");
        let second = shell.current.clone().unwrap();
        shell.agent_event(&second, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.choose(Menu::Model, "default", &mut actions);
        assert_eq!(shell.agent_in_use().unwrap().model, "");

        shell.open_session(&first, &mut actions);
        shell.show_draft();
        let agent = shell.agent_in_use().unwrap();
        assert_eq!((agent.model.as_str(), agent.effort.as_str()), ("haiku", "low"));
        assert_eq!(shell.composer.editor.text(), "half a thought");
        actions.clear();
        shell.send("Three", false, Vec::new(), &mut actions);
        assert_eq!(sent(&actions), Some(("haiku".into(), "low".into())));
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);

        shell.open_session(&second, &mut actions);
        actions.clear();
        shell.send("Four", false, Vec::new(), &mut actions);
        assert_eq!(sent(&actions), Some((String::new(), "low".into())), "Sonnet, at the effort it was left with");
        // Its process was started with Haiku: it takes the new model as it runs, and is not closed for it.
        assert!(!actions.iter().any(|a| matches!(a, Action::CloseLive(_))));
    }

    /// A shell whose agent (on a Claude account) is answering "Fix the bug": the session's id.
    /// What Claude Code runs in the background keeps its process up when the user looks elsewhere.
    #[test]
    fn a_session_running_something_in_the_background_keeps_its_process() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let first = working(&mut shell, &mut actions);
        let task = |id: &str, ended: bool| AgentEvent::Background {
            id: id.into(),
            what: if ended { String::new() } else { format!("Watch build {id}") },
            ended,
        };
        shell.agent_event(&first, task("b1", false), &mut actions);
        shell.agent_event(&first, task("b2", false), &mut actions);
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.agent_event(&first, task("b1", true), &mut actions);
        let running =
            |shell: &Shell| shell.threads[&first].background.iter().map(|t| t.what.clone()).collect::<Vec<_>>();
        assert_eq!(running(&shell), ["Watch build b2"]);
        let closes = |actions: &[Action]| actions.iter().any(|a| matches!(a, Action::CloseLive(s) if *s == first));

        // Leaving it for a new session, or for another one, leaves its process be.
        actions.clear();
        shell.new_session();
        shell.send("Meanwhile", false, Vec::new(), &mut actions);
        let second = shell.current.clone().unwrap();
        shell.agent_event(&second, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.open_session(&first, &mut actions);
        shell.open_session(&second, &mut actions);
        assert!(!closes(&actions), "it still runs b2");

        // Once nothing runs, it is closed like any other left with nothing to do: after a while.
        shell.agent_event(&first, task("b2", true), &mut actions);
        shell.open_session(&first, &mut actions);
        shell.open_session(&second, &mut actions);
        assert!(!closes(&actions));
        shell.rest(unix_now() + 24 * 60 * 60, &mut actions);
        assert!(closes(&actions));

        // A process that ends with a task running says so in the conversation.
        shell.agent_event(&first, task("b3", false), &mut actions);
        let notes = |shell: &Shell| {
            shell.threads[&first].session.entries.iter().filter(|e| matches!(e, Entry::Note { .. })).count()
        };
        let before = notes(&shell);
        shell.agent_event(&first, AgentEvent::Disconnected(None), &mut actions);
        assert_eq!(notes(&shell), before + 1);
        assert!(shell.threads[&first].background.is_empty());
    }

    #[test]
    fn the_app_wakes_when_the_open_sessions_cache_runs_out() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let session = working(&mut shell, &mut actions);
        shell.config.panel = true;
        let ended = AgentEvent::ReplyEnded {
            raw: Vec::new(),
            wire: atroxabot_core::Wire::Messages,
            model: "claude-x".into(),
            usage: atroxabot_core::Usage { input_tokens: 250_000, output_tokens: 10, ..Default::default() },
            cost: 0.0,
            problem: String::new(),
        };
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        shell.agent_event(&session, ended, &mut actions);
        let until = unix_now() + 3_000;
        shell.agent_event(&session, AgentEvent::Cached(until), &mut actions);
        assert_eq!(shell.cache_turns(), None, "at work, every request keeps it");
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert_eq!(shell.cache_turns(), Some(until));
        assert!(shell.next_wake().is_some_and(|at| at <= until));
        // Nothing to wake for once it ran out, or while the panel that says so is closed.
        shell.threads.get_mut(&session).expect("open").session.cached_until = 1;
        assert_eq!(shell.cache_turns(), None);
        shell.threads.get_mut(&session).expect("open").session.cached_until = until;
        shell.config.panel = false;
        assert_eq!(shell.cache_turns(), None);
    }

    /// Another model goes to the process that runs: it is not closed for it, and what it runs stays.
    #[test]
    fn another_model_keeps_the_process_and_what_it_runs() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let session = working(&mut shell, &mut actions);
        let watch = AgentEvent::Background { id: "b1".into(), what: "Watch the build".into(), ended: false };
        shell.agent_event(&session, watch, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.choose(Menu::Model, "haiku", &mut actions);
        actions.clear();
        shell.send("And now?", false, Vec::new(), &mut actions);
        let model = actions.iter().find_map(|a| match a {
            Action::Send { agent, .. } => Some(agent.model.clone()),
            _ => None,
        });
        assert_eq!(model.as_deref(), Some("haiku"));
        assert!(!actions.iter().any(|a| matches!(a, Action::CloseLive(_))));
        assert_eq!(shell.threads[&session].background.len(), 1, "the build is still watched");
    }

    #[test]
    fn claude_guesses_the_next_message_and_tab_takes_it() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let session = working(&mut shell, &mut actions);
        let guess = |text: &str| AgentEvent::Suggestion(text.into());
        // During a turn a guess is stale before it could be read.
        shell.agent_event(&session, guess("Too early"), &mut actions);
        assert_eq!(shell.suggestion_shown(), None);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.agent_event(&session, guess("Run the tests"), &mut actions);
        assert_eq!(shell.suggestion_shown(), Some("Run the tests"));
        // Only while the box is empty.
        shell.composer.editor.insert("Also");
        assert_eq!(shell.suggestion_shown(), None);
        shell.composer.editor.take();
        assert_eq!(shell.suggestion_shown(), Some("Run the tests"));
        // Tab writes it in, to send or to change; it does not come back once the box is emptied.
        shell.take_suggestion();
        assert_eq!(shell.composer.editor.text(), "Run the tests");
        shell.composer.editor.take();
        assert_eq!(shell.suggestion_shown(), None);
        // A new turn makes the last guess stale.
        shell.agent_event(&session, guess("Commit it"), &mut actions);
        actions.clear();
        shell.send("Run them", false, Vec::new(), &mut actions);
        assert_eq!(shell.suggestion_shown(), None);
        assert!(!actions.iter().any(|a| matches!(a, Action::CloseLive(_))), "the same process goes on");
        // Turned off, what was guessed goes, and the next message starts a process that does not guess.
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.agent_event(&session, guess("Commit it"), &mut actions);
        shell.set_suggesting(false);
        assert_eq!(shell.suggestion_shown(), None);
        shell.agent_event(&session, guess("Push it"), &mut actions);
        assert_eq!(shell.suggestion_shown(), None, "the old process may still guess: it is not shown");
        actions.clear();
        shell.send("Commit it", false, Vec::new(), &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::CloseLive(closed) if *closed == session)));
    }

    /// Claude Code is missing; its installer is opened; the account says so
    /// until Claude Code is found, which the app looks for when the user
    /// comes back to the settings.
    #[test]
    fn an_opened_installer_is_waited_for_until_its_tool_is_found() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let claude = shell.config.connection(Connection::CLAUDE_AGENT).expect("the first account");
        shell.claude_info(&claude.id, Err(ClaudeError::NotFound));
        assert_eq!(shell.provider_detail(&claude), "Claude Code is not installed");
        shell.installing(Cli::Claude);
        assert_eq!(shell.provider_detail(&claude), INSTALLING);
        // Back in the settings, it is looked for again; found, the account just needs signing in.
        shell.modal = Some(Modal::Settings(Box::new(settings::Settings::new(settings::Tab::Providers))));
        let mut actions = Vec::new();
        shell.focused(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::ProbeClaude(asked) if asked.id == claude.id)));
        shell.claude_info(&claude.id, Ok(ClaudeInfo::default()));
        assert!(!shell.is_installing(Cli::Claude));
        assert_eq!(shell.provider_detail(&claude), "Not signed in");
    }

    /// A subagent left open in the conversation's place gives way to it
    /// when the user sends something: their message and the reply show.
    #[test]
    fn sending_goes_back_to_the_conversation() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let session = working(&mut shell, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("Out of usage".into()) }, &mut actions);
        shell.thread_mut().expect("open").open_agent(Some("old-agent".into()));
        shell.send("continue", false, Vec::new(), &mut actions);
        assert_eq!(shell.thread().expect("open").sub, None, "a new turn");
        // Sent while the agent works, it waits in the tray, and the conversation shows too.
        shell.thread_mut().expect("open").open_agent(Some("old-agent".into()));
        shell.send("and the tests", false, Vec::new(), &mut actions);
        assert_eq!(shell.thread().expect("open").sub, None, "a follow-up");
    }

    /// Draws `frames` frames of `shell`, as [`paint`] does: time for the view to ease where it is headed.
    fn paints(shell: &mut Shell, ui: &mut Ui, frames: usize) {
        let fonts = crate::text::Fonts::load();
        let (mut atlas, mut images) = (crate::text::GlyphAtlas::default(), crate::image::ImageAtlas::default());
        let view = Rect::new(0.0, 0.0, 1440.0, 900.0);
        for _ in 0..frames {
            let mut instances = Vec::new();
            ui.begin(0.016, 1.0);
            let mut p =
                Painter::new(&fonts, (&mut atlas, &mut images), &mut instances, 1.0, view, Scheme::Black.palette());
            shell.draw(&mut p, ui, view, Scheme::Black, &mut Vec::new());
            ui.end();
        }
    }

    #[test]
    fn words_are_found_in_the_conversation_and_brought_into_view() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        shell.find();
        assert!(!shell.finding(), "nothing is open to look through");
        let session = working(&mut shell, &mut actions);
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        for n in 0..80 {
            let said = format!("Finding {n}: the **Parser** reads this part well enough.\n\n");
            shell.agent_event(&session, AgentEvent::Text(said), &mut actions);
        }
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        let mut ui = Ui::default();
        paints(&mut shell, &mut ui, 2);
        let progress = |shell: &Shell| shell.thread().and_then(Thread::progress).expect("it scrolls");
        let found = |shell: &Shell| shell.thread().and_then(|thread| thread.find.as_ref()).map(find::Find::total);
        assert!((progress(&shell) - 1.0).abs() < 0.001, "it opens at its end");

        // Ctrl+F gives the bar the keyboard; what is typed there is found whatever its case.
        shell.find();
        assert!(shell.finding());
        let typed = |shell: &mut Shell, words: &str| {
            let find = shell.thread_mut().and_then(|thread| thread.find.as_mut()).expect("open");
            find.editor.take();
            find.editor.insert(words);
        };
        typed(&mut shell, "FINDING 3:");
        paints(&mut shell, &mut ui, 60);
        assert_eq!(found(&shell), Some(1));
        assert!(progress(&shell) < 0.2, "the view went up to it: {}", progress(&shell));
        // Every place is counted; the one before the first is the last.
        typed(&mut shell, "parser");
        paints(&mut shell, &mut ui, 2);
        assert_eq!(found(&shell), Some(80));
        shell.thread_mut().and_then(|thread| thread.find.as_mut()).expect("open").step(true);
        paints(&mut shell, &mut ui, 60);
        assert!(progress(&shell) > 0.8, "the last one is near the end: {}", progress(&shell));
        // The bar stays while the user reads on.
        assert!(shell.thread().is_some_and(|thread| thread.find.is_some()));

        // Found by the search window in the message as it was written (Markdown's marks are not shown):
        // the conversation opens at that message, and the keyboard stays with the message box.
        shell.thread_mut().expect("open").open_find("the **parser** reads", false, true);
        paints(&mut shell, &mut ui, 60);
        assert_eq!(found(&shell), Some(0));
        assert!(!shell.finding());
        assert!(progress(&shell) < 0.2, "at the message that holds it: {}", progress(&shell));
        // Another session has no bar of this one's.
        shell.new_session();
        assert!(!shell.finding() && shell.thread().is_none());
    }

    /// A subagent opened after it finished shows its end, as the conversation does.
    #[test]
    fn an_opened_subagent_shows_its_end() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        let session = working(&mut shell, &mut actions);
        let input =
            serde_json::json!({ "description": "Look around", "prompt": "Read it all", "subagent_type": "Explore" });
        let call = atroxabot_core::ToolUse {
            id: "a1".into(),
            name: "Agent".into(),
            input: input.to_string(),
            ..atroxabot_core::ToolUse::default()
        };
        shell.agent_event(&session, AgentEvent::ToolCall(call), &mut actions);
        for n in 0..80 {
            let said = AgentEvent::Text(format!(
                "Finding {n}: the parser reads this part well enough.

"
            ));
            shell.agent_event(&session, AgentEvent::Sub { parent: "a1".into(), event: Box::new(said) }, &mut actions);
        }
        let done = AgentEvent::ToolResult { id: "a1".into(), output: "Done".into(), is_error: false };
        shell.agent_event(&session, done, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        shell.thread_mut().expect("open").open_agent(Some("a1".into()));
        let mut ui = Ui::default();
        paint(&mut shell, &mut ui);
        paint(&mut shell, &mut ui);
        assert_eq!(shell.thread().expect("open").sub_progress(), Some(1.0), "at its end, not its start");
    }

    fn working(shell: &mut Shell, actions: &mut Vec<Action>) -> String {
        signed_in(shell);
        shell.send("Fix the bug", false, Vec::new(), actions);
        let session = shell.current.clone().expect("a session");
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), actions);
        session
    }

    fn sent_texts(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| if let Action::Send { text, .. } = a { Some(text.clone()) } else { None })
            .collect()
    }

    fn handed(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| if let Action::FollowUp { message, .. } = a { Some(message.text.clone()) } else { None })
            .collect()
    }

    fn call(id: &str) -> AgentEvent {
        AgentEvent::ToolCall(atroxabot_core::ToolUse { id: id.into(), name: "Bash".into(), ..Default::default() })
    }

    fn result(id: &str) -> AgentEvent {
        AgentEvent::ToolResult { id: id.into(), output: "ok".into(), is_error: false }
    }

    fn taken(shell: &Shell, session: &str, text: &str) -> AgentEvent {
        let id = shell.threads[session].follow_ups[0].message.id.clone();
        AgentEvent::FollowUp { id, text: text.into(), note: String::new(), files: Vec::new() }
    }

    #[test]
    fn a_message_sent_while_the_agent_works_is_read_at_its_next_step() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        actions.clear();
        // Written while it thinks: it waits, and nothing is started.
        shell.send("Also run the tests", false, Vec::new(), &mut actions);
        assert!(sent_texts(&actions).is_empty() && handed(&actions).is_empty());
        assert_eq!(shell.threads[&session].follow_ups.len(), 1);
        // Its tools run: the agent is handed it, to read once they are done.
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        shell.agent_event(&session, call("t1"), &mut actions);
        assert_eq!(handed(&actions), ["Also run the tests"]);
        actions.clear();
        shell.agent_event(&session, result("t1"), &mut actions);
        shell.agent_event(&session, call("t2"), &mut actions);
        assert!(handed(&actions).is_empty(), "handed over once");
        // Taken in: it leaves the line and stands in the conversation where the agent read it.
        let event = taken(&shell, &session, "Also run the tests");
        shell.agent_event(&session, event, &mut actions);
        let thread = &shell.threads[&session];
        assert!(thread.follow_ups.is_empty() && thread.busy);
        assert!(
            matches!(thread.session.entries.last(), Some(Entry::User { text, .. }) if text == "Also run the tests")
        );
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(sent_texts(&actions).is_empty() && !shell.threads[&session].busy);
    }

    #[test]
    fn what_was_not_read_goes_next_or_back_to_the_message_box() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        // The turn ends before a tool ran: what waits is the next message, two as one.
        shell.send("First", false, Vec::new(), &mut actions);
        shell.send("Second", false, vec!["C:\\shots\\a.png".into()], &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert_eq!(sent_texts(&actions), ["First\n\nSecond"]);
        assert!(shell.threads[&session].busy && shell.threads[&session].follow_ups.is_empty());
        let last = shell.threads[&session].session.entries.last();
        assert!(matches!(last, Some(Entry::User { files, .. }) if files.len() == 1));

        // Claude Code answers one it was handed after this turn: the agent stays at work for it.
        shell.agent_event(&session, call("t1"), &mut actions);
        shell.send("Then commit", false, Vec::new(), &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::Queued(1), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert!(sent_texts(&actions).is_empty() && shell.threads[&session].busy, "not sent twice");
        let event = taken(&shell, &session, "Then commit");
        shell.agent_event(&session, event, &mut actions);
        assert!(shell.threads[&session].follow_ups.is_empty());

        // Stopped: one handed over and one still waiting both come back to be edited, and nothing runs.
        shell.agent_event(&session, call("t2"), &mut actions);
        shell.send("Use the other branch", false, Vec::new(), &mut actions);
        shell.agent_event(&session, result("t2"), &mut actions);
        shell.send("and rebase", false, Vec::new(), &mut actions);
        shell.composer.editor.insert("half-written");
        actions.clear();
        shell.stop(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Stop(_))));
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("aborted".into()) }, &mut actions);
        assert!(sent_texts(&actions).is_empty() && !shell.threads[&session].busy);
        assert_eq!(shell.composer.editor.text(), "Use the other branch\n\nand rebase\n\nhalf-written");
        assert!(shell.threads[&session].follow_ups.is_empty());
    }

    #[test]
    fn send_now_stops_the_step_and_the_agent_reads_it_at_once() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        shell.agent_event(&session, AgentEvent::Text("Thinking it over".into()), &mut actions);
        shell.send("Stop, use the other file", false, Vec::new(), &mut actions);
        actions.clear();
        shell.hurry(&session, &mut actions);
        assert_eq!(handed(&actions), ["Stop, use the other file"]);
        assert!(actions.iter().any(|a| matches!(a, Action::Interrupt(s) if *s == session)));
        assert!(!actions.iter().any(|a| matches!(a, Action::Stop(_))), "what was sent is kept");
        // Claude Code answers it right after the interrupted step.
        shell.agent_event(&session, AgentEvent::Queued(1), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: Some("aborted".into()) }, &mut actions);
        let thread = &shell.threads[&session];
        assert!(thread.busy && !thread.hurry && !thread.stopping);
        let reply =
            thread.session.entries.iter().rev().find_map(|e| if let Entry::Reply(r) = e { Some(r) } else { None });
        assert!(reply.is_some_and(|reply| reply.problem.is_empty()), "being stopped is no failure");

        // When the engine only stopped (an API turn), the shell sends it as the next message.
        let event = taken(&shell, &session, "Stop, use the other file");
        shell.agent_event(&session, event, &mut actions);
        shell.send("Quick", false, Vec::new(), &mut actions);
        shell.hurry(&session, &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        assert_eq!(sent_texts(&actions), ["Quick"], "a Claude account is not told: Claude Code marks it");
    }

    #[test]
    fn clear_empties_the_session_in_place() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::Text("Found it.".into()), &mut actions);
        // Asked while it works: the turn stops first.
        actions.clear();
        shell.send("/clear", false, Vec::new(), &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Stop(_))));
        assert!(!shell.threads[&session].session.entries.is_empty());
        actions.clear();
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        let thread = &shell.threads[&session];
        assert!(
            thread.session.entries.is_empty() && thread.session.remote.is_empty() && thread.session.title.is_empty()
        );
        assert_eq!((shell.current.as_deref(), shell.sessions.len()), (Some(session.as_str()), 1), "the same session");
        assert!(actions.iter().any(|a| matches!(a, Action::CloseLive(s) if *s == session)), "a fresh context");
        let saved =
            |a: &Action| matches!(a, Action::SaveSession(saved) if saved.id == session && saved.entries.is_empty());
        assert!(actions.iter().any(saved));
        // The next message starts the conversation over, in that session.
        actions.clear();
        shell.send("Hello again", false, Vec::new(), &mut actions);
        assert_eq!(sent_texts(&actions), ["Hello again"]);
        assert_eq!(shell.threads[&session].session.title, "Hello again");
    }

    #[test]
    fn an_api_session_is_summarised_by_the_app_and_takes_skills_by_name() {
        let mut config = Config::default();
        config.connections.push(Connection { id: "or".into(), kind: ConnectionKind::OpenAi, ..Connection::default() });
        let agent = Agent { connection: "or".into(), model: "deepseek-chat".into(), ..Agent::new("Scout") };
        let mut shell = Shell::new(config, vec![agent], Vec::new());
        shell.skills.installed.push(atroxabot_core::skills::Skill {
            name: "pdf".into(),
            description: "Read and fill PDF forms.\nMore.".into(),
            ..atroxabot_core::skills::Skill::default()
        });
        let mut actions = Vec::new();
        shell.send("/pdf fill the form", false, Vec::new(), &mut actions);
        let sent = actions.iter().find_map(|a| match a {
            Action::Send { session, .. } => session.entries.last().cloned(),
            _ => None,
        });
        assert!(
            matches!(&sent, Some(atroxabot_core::Entry::User { note, .. }) if note.contains("skill `pdf`")),
            "{sent:?}"
        );
        let session = shell.current.clone().expect("a session");
        shell.agent_event(&session, AgentEvent::Text("Filled.".into()), &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        actions.clear();
        shell.send("/compact", false, Vec::new(), &mut actions);
        assert!(matches!(&actions[..], [Action::Compact { .. }]), "no message is sent");
        let thread = &shell.threads[&session];
        assert!(thread.busy && thread.session.entries.len() == 2, "nothing joins the conversation");
        // The list a slash opens names both.
        shell.threads.get_mut(&session).unwrap().busy = false;
        shell.composer.editor.insert("/");
        let listed: Vec<String> = shell.slash_matches().into_iter().map(|(name, _)| name).collect();
        assert!(listed.contains(&"compact".to_owned()) && listed.contains(&"pdf".to_owned()), "{listed:?}");
    }

    #[test]
    fn clear_keeps_a_title_the_user_gave() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        let mut editor = Editor::default();
        editor.insert("Parser work");
        shell.renaming = Some(editor);
        shell.finish_rename(true, &mut actions);
        actions.clear();
        shell.send("/clear", false, Vec::new(), &mut actions);
        let cleared = &shell.threads[&session].session;
        assert!(cleared.entries.is_empty());
        assert_eq!((cleared.title.as_str(), cleared.named), ("Parser work", true));
        assert_eq!(shell.sessions[0].title, "Parser work", "the sidebar keeps it too");
        let saved = |a: &Action| matches!(a, Action::SaveSession(saved) if saved.title == "Parser work" && saved.named);
        assert!(actions.iter().any(saved));
        // The next message does not rename it.
        shell.send("Something else", false, Vec::new(), &mut actions);
        assert_eq!(shell.threads[&session].session.title, "Parser work");
    }

    /// Two Claude accounts, both signed in, and one agent on the first.
    fn two_claude_accounts() -> (Shell, Vec<Action>) {
        let mut config = Config::default();
        config.connections.push(Connection {
            id: "work".into(),
            name: "Work".into(),
            config_dir: "/accounts/work".into(),
            ..Connection::claude_agent()
        });
        let mut shell = Shell::new(config, vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        signed_in(&mut shell);
        let mut info = ClaudeInfo::default();
        info.account.email = "me@work.example".into();
        shell.claude_info("work", Ok(info));
        (shell, actions)
    }

    /// Where each message sent went: the account, the Claude session it resumes, and whether it was a takeover.
    fn sent_to(actions: &[Action]) -> Vec<(String, String, bool)> {
        let sent = |action: &Action| match action {
            Action::Send { text, session, .. } => {
                Some((session.account.clone(), session.remote.clone(), text.contains("taking over")))
            }
            _ => None,
        };
        actions.iter().filter_map(sent).collect()
    }

    #[test]
    fn each_session_keeps_the_provider_it_was_left_on() {
        let (mut shell, mut actions) = two_claude_accounts();
        shell.send("One", false, Vec::new(), &mut actions);
        let first = shell.current.clone().unwrap();
        shell.agent_event(&first, AgentEvent::Remote("r1".into()), &mut actions);
        shell.agent_event(&first, AgentEvent::Text("Answer one.".into()), &mut actions);
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // A second session is moved to the other account from its top bar.
        shell.new_session();
        shell.choose(Menu::Provider, "work", &mut actions);
        assert_eq!(shell.selected_agent().unwrap().connection, "work", "where its next sessions start");
        shell.send("Two", false, Vec::new(), &mut actions);
        let second = shell.current.clone().unwrap();
        shell.agent_event(&second, AgentEvent::Remote("r2".into()), &mut actions);
        shell.agent_event(&second, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // The first is where it was left: the top bar, the panel and its next message say so.
        shell.open_session(&first, &mut actions);
        assert_eq!(shell.agent_in_use().unwrap().connection, Connection::CLAUDE_AGENT);
        assert_eq!(shell.limits_account().as_deref(), Some(Connection::CLAUDE_AGENT));
        actions.clear();
        shell.send("Three", false, Vec::new(), &mut actions);
        let resumed = (Connection::CLAUDE_AGENT.to_owned(), "r1".to_owned(), false);
        assert_eq!(sent_to(&actions), [resumed], "its own conversation, not a digest of it on another account");
        shell.agent_event(&first, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // And so is the second.
        shell.open_session(&second, &mut actions);
        assert_eq!(shell.agent_in_use().unwrap().connection, "work");
        actions.clear();
        shell.send("Four", false, Vec::new(), &mut actions);
        assert_eq!(sent_to(&actions), [("work".to_owned(), "r2".to_owned(), false)]);
        shell.agent_event(&second, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // Another provider in the agent's editor is for the sessions it starts next.
        let edited = Agent { connection: Connection::CLAUDE_AGENT.into(), ..shell.selected_agent().unwrap().clone() };
        shell.save_agent(edited, &mut actions);
        shell.open_session(&second, &mut actions);
        assert_eq!(shell.agent_in_use().unwrap().connection, "work");
        shell.new_session();
        assert_eq!(shell.agent_in_use().unwrap().connection, Connection::CLAUDE_AGENT);

        // A session whose provider was deleted thinks as its agent does.
        shell.config.connections.retain(|connection| connection.id != "work");
        shell.open_session(&second, &mut actions);
        assert_eq!(shell.agent_in_use().unwrap().connection, Connection::CLAUDE_AGENT);
    }

    #[test]
    fn what_is_sent_while_a_session_is_read_waits_for_its_messages() {
        let (mut shell, mut actions) = two_claude_accounts();
        // A saved session with a history, left on the other account with a model of its own.
        let mut saved = Session::new(&shell.agents[0].id);
        saved.title = "Old work".into();
        saved.prompt("Earlier question", "");
        (saved.remote, saved.account) = ("r9".into(), "work".into());
        (saved.provider, saved.model, saved.effort) = ("work".into(), "opus".into(), "high".into());
        shell.sessions.insert(0, saved.summary());
        actions.clear();
        shell.open_session(&saved.id, &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::LoadSession(id) if *id == saved.id)));

        // It shows what it was left with at once, not its agent's own.
        let agent = shell.agent_in_use().unwrap();
        assert_eq!((agent.connection.as_str(), agent.model.as_str(), agent.effort.as_str()), ("work", "opus", "high"));

        // Nothing of it is saved before its messages are in: not a message, a title, a folder or a model.
        actions.clear();
        shell.send("Quick question", false, Vec::new(), &mut actions);
        shell.renaming = Some(Editor::default());
        if let Some(editor) = &mut shell.renaming {
            editor.insert("Renamed");
        }
        shell.finish_rename(true, &mut actions);
        shell.set_folder("/tmp/elsewhere", &mut actions);
        shell.toggle_worktree(&mut actions);
        shell.choose(Menu::Model, "fable", &mut actions);
        shell.choose(Menu::Provider, Connection::CLAUDE_AGENT, &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::SaveSession(_) | Action::Send { .. })));
        assert_eq!(shell.selected_agent().unwrap().connection, Connection::CLAUDE_AGENT, "nor is its agent changed");

        // The file is read: the message goes, after everything that was said.
        shell.session_loaded(&saved.id, Ok(saved.clone()), &mut actions);
        let sent = actions.iter().find_map(|a| match a {
            Action::Send { session, text, agent, .. } => Some((session.clone(), text.clone(), agent.model.clone())),
            _ => None,
        });
        let (session, text, model) = sent.expect("the message goes");
        assert_eq!((text.as_str(), model.as_str()), ("Quick question", "opus"));
        assert_eq!((session.entries.len(), session.remote.as_str(), session.title.as_str()), (2, "r9", "Old work"));
        assert!(matches!(&session.entries[1], Entry::User { text, .. } if text == "Quick question"));

        // One that cannot be read gives the message back.
        let mut lost = Session::new(&shell.agents[0].id);
        lost.title = "Gone".into();
        shell.sessions.insert(0, lost.summary());
        shell.open_session(&lost.id, &mut actions);
        shell.send("Are you there?", false, Vec::new(), &mut actions);
        let failed = Err(Error::Io(std::io::Error::other("unreadable")));
        shell.session_loaded(&lost.id, failed, &mut actions);
        assert!(shell.current.is_none() && shell.composer.editor.text() == "Are you there?");
    }

    #[test]
    fn usage_is_logged_under_the_provider_the_turn_ran_on() {
        let (mut shell, mut actions) = two_claude_accounts();
        shell.send("One", false, Vec::new(), &mut actions);
        let session = shell.current.clone().unwrap();
        shell.agent_event(&session, AgentEvent::Remote("r1".into()), &mut actions);
        // Its provider is swapped while the turn runs: what the turn uses is still the first account's.
        shell.choose(Menu::Provider, "work", &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        let ended = AgentEvent::ReplyEnded {
            raw: Vec::new(),
            wire: atroxabot_core::Wire::default(),
            model: "claude-opus-5-5".into(),
            usage: atroxabot_core::Usage::new(1_200, 40),
            cost: 0.02,
            problem: String::new(),
        };
        shell.agent_event(&session, ended, &mut actions);
        let logged: Vec<_> =
            actions.iter().filter_map(|a| if let Action::LogUsage(record) = a { Some(record) } else { None }).collect();
        assert_eq!(logged.len(), 1);
        assert_eq!(
            (logged[0].provider.as_str(), logged[0].provider_name.as_str()),
            (Connection::CLAUDE_AGENT, "Claude")
        );
    }

    #[test]
    fn the_plan_usage_is_of_the_account_the_top_bar_names() {
        let mut config = Config::default();
        config.connections.push(Connection {
            id: "work".into(),
            config_dir: "/accounts/work".into(),
            ..Connection::claude_agent()
        });
        let mut shell = Shell::new(config, vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        signed_in(&mut shell);
        let mut info = ClaudeInfo::default();
        info.account.email = "me@work.example".into();
        shell.claude_info("work", Ok(info));
        shell.send("Hello", false, Vec::new(), &mut actions);
        let session = shell.current.clone().unwrap();
        assert_eq!(shell.limits_account().as_deref(), Some(Connection::CLAUDE_AGENT));

        // Switched while the reply runs: the panel follows at once, before any message goes to it.
        actions.clear();
        shell.choose(Menu::Provider, "work", &mut actions);
        assert_eq!(shell.limits_account().as_deref(), Some("work"));
        assert!(!actions.iter().any(|a| matches!(a, Action::CloseLive(_))), "a busy session's process may finish");
        // What the running reply reports is still the first account's.
        let limits = Limits { at: 5, status: "allowed".into(), ..Limits::default() };
        shell.agent_event(&session, AgentEvent::Limits(limits), &mut actions);
        assert!(shell.limits.contains_key(Connection::CLAUDE_AGENT) && !shell.limits.contains_key("work"));
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);

        // The next message does not go to the process still on the first account.
        actions.clear();
        shell.send("And now?", false, Vec::new(), &mut actions);
        let closed = actions.iter().position(|a| matches!(a, Action::CloseLive(closed) if *closed == session));
        let sent = actions.iter().position(|a| matches!(a, Action::Send { .. }));
        assert!(matches!((closed, sent), (Some(closed), Some(sent)) if closed < sent), "closed, then sent");
        // On the same account it simply carries on.
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        actions.clear();
        shell.send("More", false, Vec::new(), &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::CloseLive(_))));
    }

    #[test]
    fn a_no_takes_what_is_written_as_its_reason() {
        use atroxabot_core::Ask;
        let ask = |id: &str| AgentEvent::Permission(Ask { id: id.into(), tool: "Bash".into(), ..Ask::default() });
        let denied = |actions: &[Action]| {
            actions.iter().find_map(|a| match a {
                Action::Decide { ask, verdict: Verdict::Deny(why), .. } => Some((ask.clone(), why.clone())),
                _ => None,
            })
        };
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        let session = working(&mut shell, &mut actions);
        // Nothing is asked: a no answers nothing, and what is written stays.
        shell.composer.editor.insert("a thought");
        actions.clear();
        shell.deny(&mut actions);
        assert!(actions.is_empty() && shell.composer.editor.text() == "a thought");
        shell.composer.editor.take();

        // Nothing written: a bare no.
        shell.agent_event(&session, ask("a1"), &mut actions);
        shell.deny(&mut actions);
        assert_eq!(denied(&actions), Some(("a1".into(), String::new())));

        // What is written goes with it, without the space around it; the files stay for a message.
        shell.agent_event(&session, ask("a2"), &mut actions);
        shell.agent_event(&session, ask("a3"), &mut actions);
        shell.composer.editor.insert("  use pnpm, not npm\n");
        shell.composer.files.push("notes.md".into());
        actions.clear();
        shell.deny(&mut actions);
        assert_eq!(denied(&actions), Some(("a2".into(), "use pnpm, not npm".into())));
        assert!(shell.composer.editor.text().is_empty() && shell.composer.files.len() == 1);
        assert!(sent_texts(&actions).is_empty() && shell.threads[&session].follow_ups.is_empty());
        // The next one waits for its own answer: the reason went once.
        actions.clear();
        shell.deny(&mut actions);
        assert_eq!(denied(&actions), Some(("a3".into(), String::new())));
        assert!(shell.threads[&session].asks.is_empty());
    }

    /// Codex takes no words with a no: they follow it as a message.
    #[test]
    fn a_reason_reaches_codex_as_a_message() {
        use atroxabot_core::Ask;
        let (mut shell, mut actions) = on_codex();
        let account = shell.config.connections[0].clone();
        let CodexAccount::Ready(info) = shell.codex[&account.id].clone() else { panic!("known") };
        let info = CodexInfo { account: Some(atroxabot_codex_account("me@example.com", "plus")), ..*info };
        shell.codex_info(&account.id, Ok(info), &mut actions);
        shell.send("Install it", false, Vec::new(), &mut actions);
        let session = shell.current.clone().expect("a session");
        shell.agent_event(&session, AgentEvent::Remote("thread-1".into()), &mut actions);
        let ask = Ask { id: "a1".into(), tool: "shell".into(), ..Ask::default() };
        shell.agent_event(&session, AgentEvent::Permission(ask), &mut actions);
        shell.composer.editor.insert("use pnpm");
        actions.clear();
        shell.deny(&mut actions);
        let bare = |a: &Action| matches!(a, Action::Decide { verdict: Verdict::Deny(why), .. } if why.is_empty());
        assert!(actions.iter().any(bare));
        assert!(shell.composer.editor.text().is_empty());
        let waiting: Vec<_> = shell.threads[&session].follow_ups.iter().map(|m| m.message.text.clone()).collect();
        assert_eq!(waiting, ["use pnpm"], "read at its next step");
    }

    /// A shell whose one agent thinks through a Codex account that is signed in.
    fn on_codex() -> (Shell, Vec<Action>) {
        let account = Connection::codex("Codex");
        let agent = Agent { connection: account.id.clone(), ..Agent::new("Scout") };
        let mut config = Config::default();
        config.connections.push(account.clone());
        let mut shell = Shell::new(config, vec![agent], Vec::new());
        let mut actions = Vec::new();
        shell.start(&mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::ProbeCodex(asked) if asked.id == account.id)));
        let model = |id: &str, default_effort: &str, is_default, hidden| atroxabot_core::CodexModel {
            id: id.into(),
            name: id.to_uppercase(),
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            default_effort: default_effort.into(),
            is_default,
            hidden,
            ..atroxabot_core::CodexModel::default()
        };
        let info = CodexInfo {
            version: "0.167.1".into(),
            models: vec![
                model("astra", "medium", false, false),
                model("sol", "low", true, false),
                model("old", "", false, true),
            ],
            requires_auth: true,
            ..CodexInfo::default()
        };
        shell.codex_info(&account.id, Ok(info), &mut actions);
        (shell, actions)
    }

    #[test]
    fn a_codex_account_answers_with_its_own_models_once_signed_in() {
        let (mut shell, mut actions) = on_codex();
        let account = shell.config.connections[0].clone();
        // What Codex's own picker lists, its default first, each at the effort Codex gives it.
        let choices = shell.model_choices(&account);
        let listed: Vec<(&str, &str)> = choices.iter().map(|c| (c.value.as_str(), c.usual.as_str())).collect();
        assert_eq!(listed, [("sol", "low"), ("astra", "medium")]);
        assert_eq!(shell.provider_detail(&account), "Not signed in");
        // Signed out, a message does not go: a notice says where to sign in.
        actions.clear();
        shell.send("Hello", false, Vec::new(), &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::Send { .. })) && shell.current.is_none());
        assert!(shell.notices.last().is_some_and(|notice| notice.text.contains("is not signed in")));

        // Signed in (with where the plan's limits stand): it goes, with Codex's default model at its effort.
        let CodexAccount::Ready(info) = shell.codex[&account.id].clone() else { panic!("known") };
        let signed_in = atroxabot_codex_account("me@example.com", "plus");
        let window = atroxabot_core::CodexLimits::default();
        let info = CodexInfo { account: Some(signed_in), limits: Some(window), ..*info };
        shell.codex_info(&account.id, Ok(info), &mut actions);
        assert_eq!(shell.provider_detail(&account), "me@example.com · Plus");
        // On a stream the address stays off the screen; the plan still shows.
        shell.config.hide_emails = true;
        assert_eq!(shell.provider_detail(&account), "Signed in · Plus");
        self::signed_in(&mut shell);
        let claude = shell.config.connection(Connection::CLAUDE_AGENT).expect("built in");
        assert_eq!(shell.provider_detail(&claude), "Signed in");
        shell.config.hide_emails = false;
        assert_eq!(shell.provider_detail(&claude), "me@example.com");
        assert!(actions.iter().any(|a| matches!(a, Action::SaveLimits(kept) if kept.contains_key(&account.id))));
        assert_eq!(shell.limits_account().as_deref(), Some(account.id.as_str()), "the panel shows its plan");
        actions.clear();
        shell.send("Hello", false, Vec::new(), &mut actions);
        let sent = actions.iter().find_map(|a| match a {
            Action::Send { agent, connection, session, .. } => {
                Some((agent.model.clone(), agent.effort.clone(), connection.kind, session.account.clone()))
            }
            _ => None,
        });
        assert_eq!(sent, Some((String::new(), "low".into(), ConnectionKind::Codex, account.id.clone())));
        let session = shell.current.clone().unwrap();
        shell.agent_event(&session, AgentEvent::Remote("thread-1".into()), &mut actions);

        // A message sent while it works is handed over while its tools run, as with Claude.
        shell.send("Also the docs", false, Vec::new(), &mut actions);
        actions.clear();
        shell.agent_event(&session, AgentEvent::ReplyStarted, &mut actions);
        shell.agent_event(&session, call("t1"), &mut actions);
        assert_eq!(handed(&actions), ["Also the docs"]);
        // "Send now" only stops the turn: Codex keeps nothing it was handed, so it goes as the next message.
        shell.send("Stop, the other file", false, Vec::new(), &mut actions);
        actions.clear();
        shell.hurry(&session, &mut actions);
        assert!(handed(&actions).is_empty() && actions.iter().any(|a| matches!(a, Action::Interrupt(_))));
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        let texts = sent_texts(&actions);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].ends_with("Also the docs\n\nStop, the other file") && texts[0].contains("stopped you"));
        // `/compact` is Claude Code's: Codex summarises by itself, and is not sent the word.
        shell.agent_event(&session, AgentEvent::TurnEnded { problem: None }, &mut actions);
        actions.clear();
        shell.send("/compact", false, Vec::new(), &mut actions);
        assert!(sent_texts(&actions).is_empty());
    }

    fn atroxabot_codex_account(email: &str, plan: &str) -> atroxabot_core::CodexAccount {
        atroxabot_core::CodexAccount { kind: "chatgpt".into(), email: email.into(), plan: plan.into() }
    }
}
