//! The command-line agents the accounts run: Claude Code, Codex and
//! Antigravity. Whether each is installed here and in which version, and its
//! row in the settings, which installs or updates it. Both are its maker's
//! own doing, in a terminal the user sees: the app only starts them, and
//! only because the user clicked.

use atroxabot_core::{Connection, ConnectionKind};

use super::widgets::row;
use super::{Account, AntigravityAccount, Cli, CodexAccount, Shell};
use crate::app::Action;
use crate::paint::{Painter, Rect};
use crate::ui::{ButtonStyle, Ui, button, icon};

impl Cli {
    /// Every tool, in the order the settings list them.
    pub(super) const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Antigravity];

    /// What the tool is called.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
        }
    }

    /// Who makes it: theirs is the installer that puts it here.
    fn maker(self) -> &'static str {
        match self {
            Self::Claude => "Anthropic",
            Self::Codex => "OpenAI",
            Self::Antigravity => "Google",
        }
    }

    /// What its accounts are called, and its mark is filed under.
    pub(super) fn family(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
        }
    }

    /// What the user types to run it in their terminal.
    pub(super) fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
        }
    }

    /// The kind of account that runs it.
    pub(super) fn kind(self) -> ConnectionKind {
        match self {
            Self::Claude => ConnectionKind::ClaudeAgent,
            Self::Codex => ConnectionKind::Codex,
            Self::Antigravity => ConnectionKind::Antigravity,
        }
    }

    /// Whether it has a command that updates it. Google documents none for
    /// Antigravity, so the app offers none.
    fn updates(self) -> bool {
        !matches!(self, Self::Antigravity)
    }
}

/// How a tool stands on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Tool {
    /// Not looked for yet.
    Unknown,
    /// Not installed.
    Missing,
    /// Installed, in this version (empty until an account of it has answered).
    Installed(String),
}

impl Shell {
    /// How `cli` stands here: what its accounts answered, and without an
    /// answer, whether it was found on disk.
    pub(super) fn tool(&self, cli: Cli) -> Tool {
        let (version, missing) = match cli {
            Cli::Claude => (
                self.claude.values().find_map(|account| match account {
                    Account::Ready(info) => Some(info.version.clone()),
                    _ => None,
                }),
                self.claude.values().any(|account| matches!(account, Account::Missing)),
            ),
            Cli::Codex => (
                self.codex.values().find_map(|account| match account {
                    CodexAccount::Ready(info) => Some(info.version.clone()),
                    _ => None,
                }),
                self.codex.values().any(|account| matches!(account, CodexAccount::Missing)),
            ),
            Cli::Antigravity => (
                self.antigravity.values().find_map(|account| match account {
                    AntigravityAccount::Ready(info) => Some(info.version.clone()),
                    _ => None,
                }),
                self.antigravity.values().any(|account| matches!(account, AntigravityAccount::Missing)),
            ),
        };
        match (version, self.found.get(&cli)) {
            (Some(version), _) => Tool::Installed(version),
            (None, Some(true)) => Tool::Installed(String::new()),
            (None, Some(false)) => Tool::Missing,
            (None, None) if missing => Tool::Missing,
            (None, None) => Tool::Unknown,
        }
    }

    /// Which tools were found on disk just now. One whose installer the user
    /// opened here and that has no account yet gets the one they have in
    /// their terminal, so what is left to do is signing in.
    pub fn tools_found(&mut self, found: [(Cli, bool); 3], actions: &mut Vec<Action>) {
        for (cli, there) in found {
            self.found.insert(cli, there);
            if !there {
                continue;
            }
            let asked_for = self.installing.remove(&cli);
            let kind = cli.kind();
            let accounts: Vec<Connection> =
                self.config.all_connections().into_iter().filter(|account| account.kind == kind).collect();
            if asked_for && accounts.is_empty() {
                self.add_own_account(kind, actions);
                actions.push(Action::SaveConfig);
            }
            // An account that said its tool was missing is being asked again: it says so meanwhile.
            for account in &accounts {
                self.missing_no_more(account);
            }
        }
    }

    /// The account `connection` no longer says its tool is missing: it was found.
    fn missing_no_more(&mut self, connection: &Connection) {
        let id = &connection.id;
        match connection.kind {
            ConnectionKind::ClaudeAgent => {
                if matches!(self.claude.get(id), Some(Account::Missing)) {
                    self.claude.insert(id.clone(), Account::Checking);
                }
            }
            ConnectionKind::Codex => {
                if matches!(self.codex.get(id), Some(CodexAccount::Missing)) {
                    self.codex.insert(id.clone(), CodexAccount::Checking);
                }
            }
            ConnectionKind::Antigravity => {
                if matches!(self.antigravity.get(id), Some(AntigravityAccount::Missing)) {
                    self.antigravity.insert(id.clone(), AntigravityAccount::Checking);
                }
            }
            ConnectionKind::ClaudeApi | ConnectionKind::OpenAi => {}
        }
    }

    /// Whether the account the user has in their terminal (the tool as they
    /// run it there, with no folder of the app's) is on the list.
    pub(super) fn has_own_account(&self, kind: ConnectionKind) -> bool {
        match kind {
            ConnectionKind::ClaudeAgent => !self.config.hide_claude_agent,
            _ => self.config.connections.iter().any(|account| account.kind == kind && account.config_dir.is_empty()),
        }
    }

    /// Puts the account the user has in their terminal on the list, and asks
    /// its tool about it. The caller saves the settings.
    pub(super) fn add_own_account(&mut self, kind: ConnectionKind, actions: &mut Vec<Action>) {
        let account = match kind {
            // Claude Code's own is built in: it only left the list.
            ConnectionKind::ClaudeAgent => {
                self.config.hide_claude_agent = false;
                self.config.connection(Connection::CLAUDE_AGENT)
            }
            ConnectionKind::Codex => Some(Connection::codex(kind.label())),
            ConnectionKind::Antigravity => Some(Connection::antigravity(kind.label())),
            ConnectionKind::ClaudeApi | ConnectionKind::OpenAi => None,
        };
        let Some(account) = account else { return };
        self.read_anew(&account, actions);
        if kind != ConnectionKind::ClaudeAgent {
            self.config.connections.push(account);
        }
    }

    /// The updater of `cli` is open in a terminal: its accounts are asked
    /// again when the user comes back to the window ([`Shell::focused`]),
    /// which says its new version.
    pub fn updating(&mut self, cli: Cli) {
        self.updated.insert(cli);
    }

    /// The row of `cli` in the settings: whether it is installed and in
    /// which version, with the button that opens its maker's installer or
    /// has it update itself (`loud` makes installing the row's call to
    /// action). Returns where what follows starts.
    pub(super) fn tool_row(
        &self,
        p: &mut Painter,
        ui: &mut Ui,
        (x, y, w): (f32, f32, f32),
        (cli, loud): (Cli, bool),
        actions: &mut Vec<Action>,
    ) -> f32 {
        let rect = Rect::new(x, y, w, 58.0);
        let (name, family) = (cli.name(), cli.family());
        let installing = self.is_installing(cli);
        let tool = self.tool(cli);
        // Under its accounts' own heading, whose it is needs no saying.
        let whose = if loud { String::new() } else { format!("For {family} accounts. ") };
        let (title, detail) = match &tool {
            Tool::Installed(version) => {
                let title = if version.is_empty() { name.to_owned() } else { format!("{name} {version}") };
                let updates = if cli == Cli::Claude { "Updates itself; Update does it now." } else { "" };
                (title, format!("{whose}{updates}").trim_end().to_owned())
            }
            Tool::Missing if installing => (
                format!("Installing {name}"),
                "Finish in the installer's window. This updates when you come back.".to_owned(),
            ),
            Tool::Missing => (
                format!("{name} is not installed"),
                format!("{whose}Install opens {}'s own installer in a terminal.", cli.maker()),
            ),
            Tool::Unknown => (format!("Looking for {name}…"), String::new()),
        };
        let offer = match tool {
            Tool::Installed(_) if cli.updates() => Some(("Update", ButtonStyle::Secondary, Action::UpdateTool(cli))),
            Tool::Missing => {
                let style = if loud && !installing { ButtonStyle::Primary } else { ButtonStyle::Secondary };
                Some(("Install", style, Action::Install(cli)))
            }
            _ => None,
        };
        row(p, rect, (icon::TERMINAL, None), &title, &detail, if offer.is_some() { 108.0 } else { 0.0 });
        self.picture_of(p, &family.to_lowercase(), Rect::new(rect.x + 12.0, rect.y + 12.0, 34.0, 34.0));
        if let Some((label, style, action)) = offer {
            let at = Rect::new(rect.right() - 96.0, rect.y + 13.0, 84.0, 32.0);
            if button(p, ui, at, label, style, true) {
                actions.push(action);
            }
        }
        rect.bottom() + 8.0
    }
}

#[cfg(test)]
mod tests {
    use atroxabot_core::{Agent, CodexError, CodexInfo, Config};

    use super::*;

    fn found(claude: bool, codex: bool, antigravity: bool) -> [(Cli, bool); 3] {
        [(Cli::Claude, claude), (Cli::Codex, codex), (Cli::Antigravity, antigravity)]
    }

    #[test]
    fn a_tool_is_known_without_an_account_of_it() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        assert_eq!(shell.tool(Cli::Codex), Tool::Unknown);
        let mut actions = Vec::new();
        shell.tools_found(found(false, false, true), &mut actions);
        assert_eq!(shell.tool(Cli::Codex), Tool::Missing);
        assert_eq!(shell.tool(Cli::Antigravity), Tool::Installed(String::new()));
        // Found without the app's doing, it gets no account by itself.
        assert!(shell.config.connections.is_empty() && actions.is_empty());
    }

    #[test]
    fn a_tool_installed_from_here_gets_the_terminals_account() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let mut actions = Vec::new();
        shell.tools_found(found(true, false, false), &mut actions);
        shell.installing(Cli::Codex);
        // Still running: nothing yet.
        shell.tools_found(found(true, false, false), &mut actions);
        assert!(shell.is_installing(Cli::Codex) && shell.config.connections.is_empty());
        shell.tools_found(found(true, true, false), &mut actions);
        assert!(!shell.is_installing(Cli::Codex));
        let [account] = &shell.config.connections[..] else { panic!("one account") };
        assert_eq!((account.kind, account.config_dir.as_str()), (ConnectionKind::Codex, ""));
        assert!(shell.has_own_account(ConnectionKind::Codex));
        assert!(actions.iter().any(|a| matches!(a, Action::ProbeCodex(asked) if asked.id == account.id)));
        assert!(actions.iter().any(|a| matches!(a, Action::SaveConfig)));
        // Its answer says the version.
        let id = account.id.clone();
        shell.codex_info(&id, Ok(CodexInfo { version: "0.167.1".into(), ..CodexInfo::default() }), &mut actions);
        assert_eq!(shell.tool(Cli::Codex), Tool::Installed("0.167.1".into()));
        // Found again later, nothing more is added.
        shell.tools_found(found(true, true, false), &mut actions);
        assert_eq!(shell.config.connections.len(), 1);
    }

    #[test]
    fn an_account_whose_tool_was_found_stops_saying_it_is_missing() {
        let mut shell = Shell::new(Config::default(), vec![Agent::new("Scout")], Vec::new());
        let account = Connection::codex("Codex");
        shell.config.connections.push(account.clone());
        shell.codex_info(&account.id, Err(CodexError::NotFound), &mut Vec::new());
        assert_eq!(shell.tool(Cli::Codex), Tool::Missing);
        shell.tools_found(found(false, true, false), &mut Vec::new());
        assert_eq!(shell.provider_detail(&account), "Checking…");
        assert_eq!(shell.tool(Cli::Codex), Tool::Installed(String::new()));
    }
}
