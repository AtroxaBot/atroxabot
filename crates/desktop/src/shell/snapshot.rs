//! Screens drawn offscreen and saved as PNGs, to look at the design in
//! every scheme without opening the app. Run by hand:
//! `ATROXABOT_SNAPSHOTS=<folder> cargo test -p atroxabot snapshots -- --ignored`

use std::path::{Path, PathBuf};

use atroxabot_core::git::{Change, GitFile, GitStatus, Report};
use atroxabot_core::skills::{self, Catalog, Elsewhere, Group, Origin, Skill, Source};
use atroxabot_core::stats::{Ledger, Record, day_of};
use atroxabot_core::update::{Release, Staged, Version};
use atroxabot_core::worktree;
use atroxabot_core::{
    Agent, AgentEvent, Ask, Autonomy, ClaudeError, ClaudeInfo, ClaudeMcpServer, ClaudeModel, CodexAccount, CodexError,
    CodexInfo, CodexModel, Config, Connection, ConnectionKind, ContextPart, LimitWindow, Limits, McpServer, Model,
    PlanCheck, Session, ToolUse, Usage, VoiceSettings, Worktree, unix_now,
};
use atroxabot_voice::CallState;

use super::agent_form::AgentForm;
use super::call::CallView;
use super::menu::Menu;
use super::panel::DiffView;
use super::settings::{Adding, Settings, Tab};
use super::skills::Fetched;
use super::spotlight::Spotlight;
use super::stats::StatsView;
use super::thread::Thread;
use super::update::Updater;
use super::{Cli, Modal, Shell};
use crate::gpu::Offscreen;
use crate::image::ImageAtlas;
use crate::paint::{Painter, Rect};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::Scheme;
use crate::ui::Ui;

/// Logical size of every snapshot, drawn at a scale of one.
const SIZE: (u32, u32) = (1440, 900);

#[test]
#[ignore = "draws on the GPU; run by hand to look at the design"]
fn snapshots() {
    let Some(dir) = std::env::var_os("ATROXABOT_SNAPSHOTS").map(PathBuf::from) else { return };
    let Some(mut gpu) = Offscreen::new() else { return };
    std::fs::create_dir_all(&dir).expect("snapshot folder");
    let fonts = Fonts::load();
    for scheme in Scheme::ALL {
        let mut welcome = shell();
        welcome.current = None;
        welcome.files_picked(vec!["C:\\shots\\error.png".into(), "C:\\code\\parser\\notes.md".into()]);
        let mut asking = shell();
        if let Some(thread) = asking.thread_mut() {
            thread.busy = true;
            thread.asks.push(Ask {
                id: "a".into(),
                tool: "Bash".into(),
                input: "{}".into(),
                summary: "cargo test --workspace".into(),
                reason: "This command is not on the allowed list.".into(),
            });
        }
        let mut model = shell();
        model.open_menu(Menu::Model, Rect::new(474.0, 836.0, 150.0, 30.0));
        let mut folder = shell();
        folder.open_menu(Menu::Folder, Rect::new(1254.0, 47.0, 98.0, 30.0));
        let mut provider = shell();
        provider.open_menu(Menu::Provider, Rect::new(1152.0, 47.0, 96.0, 30.0));
        let mut spotlight = shell();
        spotlight.modal = Some(Modal::Spotlight(Box::default()));
        let mut searching = shell();
        let mut typed = Spotlight::default();
        typed.insert("re", &mut Vec::new());
        searching.modal = Some(Modal::Spotlight(Box::new(typed)));
        let mut stats = shell();
        let seen = |used: [f64; 2]| Limits {
            at: unix_now() - 5_400,
            status: "allowed".into(),
            resets: 0,
            windows: vec![
                LimitWindow {
                    name: "five_hour".into(),
                    used: used[0],
                    resets: unix_now() + 7_300,
                    label: String::new(),
                },
                LimitWindow {
                    name: "seven_day".into(),
                    used: used[1],
                    resets: unix_now() + 250_000,
                    label: String::new(),
                },
            ],
        };
        stats.set_limits(
            [("claude-agent".to_owned(), seen([0.31, 0.61])), ("work".to_owned(), seen([0.08, 0.93]))].into(),
        );
        let mut view = StatsView::default();
        view.loaded(ledger([&stats.agents[0].id, &stats.agents[1].id]));
        stats.modal = Some(Modal::Stats(Box::new(view)));
        let mut loading = shell();
        loading.modal = Some(Modal::Stats(Box::default()));
        let panel = || {
            let mut shell = shell();
            shell.config.panel = true;
            shell.git.folder = shell.work_folder();
            shell.git.status = Some(repository());
            shell
        };
        let mut branch = panel();
        branch.open_menu(Menu::Branch, Rect::new(1120.0, 318.0, 288.0, 36.0));
        let mut plain = panel();
        plain.git.status = None;
        // Its account's limits were last reported a while ago: the panel says since when.
        plain.set_limits([("claude-agent".to_owned(), seen([0.31, 0.61]))].into());
        // A turn with subagents: their cards in the conversation and their list in the side
        // panel, and one of them open.
        let delegate = |open: Option<&str>| {
            let mut delegating = panel();
            let turn = delegating.current.clone().unwrap_or_default();
            delegating.send(
                "Find where escapes are handled and what tests cover them",
                false,
                vec!["C:\\shots\\failing-test.png".into(), "C:\\code\\parser\\notes.md".into()],
                &mut Vec::new(),
            );
            let call = |id: &str, name: &str, input: serde_json::Value| {
                AgentEvent::ToolCall(ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input: input.to_string(),
                    ..ToolUse::default()
                })
            };
            let sub =
                |parent: &str, event: AgentEvent| AgentEvent::Sub { parent: parent.into(), event: Box::new(event) };
            let brief = |kind: &str, about: &str| serde_json::json!({ "subagent_type": kind, "description": about, "prompt": format!("{about}. Report the files and line numbers you find, nothing else.") });
            for event in [
                AgentEvent::Text("I'll look at both sides at once.".into()),
                call("a1", "Agent", brief("Explore", "Find the escape handling")),
                call("a2", "Agent", brief("general-purpose", "List the tokenizer tests")),
                sub("a1", AgentEvent::Text("Searching the tokenizer for escape branches.".into())),
                sub("a1", call("g1", "Grep", serde_json::json!({ "pattern": "'\\\\\\\\'", "path": "src" }))),
                AgentEvent::ToolResult { id: "g1".into(), output: "src/tokenizer.rs:41".into(), is_error: false },
                sub("a1", call("r1", "Read", serde_json::json!({ "file_path": "src/tokenizer.rs" }))),
                sub(
                    "a1",
                    AgentEvent::Text(
                        "The escape branch is at `src/tokenizer.rs:41`; it advances by one character only.".into(),
                    ),
                ),
                AgentEvent::Activity { id: "a1".into(), text: "Reading src/tokenizer.rs · 2 tool calls".into() },
                AgentEvent::Activity { id: "a2".into(), text: "Done · 3 tool calls · 28.8k tokens · 12s".into() },
                AgentEvent::Background { id: "b1".into(), what: "Watch the release build".into(), ended: false },
                AgentEvent::ToolResult {
                    id: "a2".into(),
                    output: "14 tests in tests/tokenizer.rs".into(),
                    is_error: false,
                },
            ] {
                delegating.agent_event(&turn, event, &mut Vec::new());
            }
            let limits = Limits {
                at: unix_now() - 420,
                status: "allowed".into(),
                resets: 0,
                windows: vec![
                    LimitWindow {
                        name: "five_hour".into(),
                        used: 0.34,
                        resets: unix_now() + 11_520,
                        label: String::new(),
                    },
                    LimitWindow {
                        name: "seven_day".into(),
                        used: 0.82,
                        resets: unix_now() + 187_200,
                        label: String::new(),
                    },
                ],
            };
            delegating.agent_event(&turn, AgentEvent::Limits(limits), &mut Vec::new());
            if let Some(thread) = delegating.threads.get_mut(&turn) {
                thread.busy = open.is_none();
                thread.sub = open.map(str::to_owned);
            }
            delegating
        };
        let (delegating, inside) = (delegate(None), delegate(Some("a1")));
        // Claude Code was asked where the plan stands (Fable's week came along), then answered in a new way.
        let mut changed = delegate(None);
        let week = |name: &str, used: f64, label: &str| LimitWindow {
            name: name.into(),
            used,
            resets: unix_now() + 187_200,
            label: label.into(),
        };
        let windows = vec![
            LimitWindow { resets: unix_now() + 11_520, ..week("five_hour", 0.34, "") },
            week("seven_day", 0.82, ""),
            week("seven_day_overage_included", 0.24, "Fable"),
        ];
        let checked = Limits { at: unix_now() - 900, windows, ..Limits::default() };
        let account = Connection::CLAUDE_AGENT;
        changed.limits_checked(account, Ok(PlanCheck::Limits { limits: checked, complete: true }), &mut Vec::new());
        let refused = "control request req_2 failed: Unsupported control request subtype: get_usage";
        changed.limits_checked(account, Err(ClaudeError::Unsupported(refused.into())), &mut Vec::new());
        // The agent asks: one question of two, the first answered.
        let mut asked = shell();
        if let Some(thread) = asked.thread_mut() {
            thread.busy = true;
            thread.asks.push(Ask {
                id: "q".into(),
                tool: "AskUserQuestion".into(),
                input: serde_json::json!({ "questions": [
                    { "question": "How strict should the tokenizer be about unknown escapes?", "header": "Escapes", "multiSelect": false,
                      "options": [
                        { "label": "Reject them", "description": "An unknown escape is an error, with its position." },
                        { "label": "Keep them as written", "description": "The backslash and the character both stay in the string." },
                        { "label": "Drop the backslash", "description": "Only the character after it is kept." } ] },
                    { "question": "Which tests should I add?", "header": "Tests", "multiSelect": true,
                      "options": [{ "label": "Unit tests", "description": "" }, { "label": "A fuzz target", "description": "" }] },
                ]}).to_string(),
                ..Ask::default()
            });
        }
        let mut slash = shell();
        slash.composer.editor.insert("/co");
        // Between turns, Claude Code's guess at the next message waits in the empty message box; a long one is cut.
        let guessing = |guess: &str| {
            let mut guessing = shell();
            if let Some(thread) = guessing.thread_mut() {
                thread.busy = false;
                thread.suggestion = Some(guess.to_owned());
            }
            guessing
        };
        let suggested = guessing("Add a test for an escaped quote at the very end");
        let suggested_long = guessing(
            "Now run the whole test suite, and if anything else fails because of the change, fix that too before you              commit the escape handling",
        );
        // An `@` word: the folder's files that match it.
        let mut mention = shell();
        mention.composer.editor.insert("Why does @tok");
        mention.mention_typed(&mut Vec::new());
        let listed = mention.work_folder();
        mention.files_listed(
            &listed,
            ["src/tokenizer.rs", "tests/tokenizer.rs", "docs/tokens.md", "src/parser/expr.rs"].map(String::from).into(),
        );
        // The agent keeps a task list: the side panel shows it, the conversation the task in hand.
        let tasks = || {
            let mut tasks = panel();
            let turn = tasks.current.clone().unwrap_or_default();
            tasks.send("Fix the escape handling and cover it with tests", false, Vec::new(), &mut Vec::new());
            let subjects = [
                ("Read the tokenizer", ""),
                ("Fix the escape branch", ""),
                (
                    "Add a test for escaped quotes and backslashes at the end of a string",
                    "Cover `\\\"` mid-string and a lone backslash before the closing quote.",
                ),
                ("Run the tests", ""),
            ];
            for (number, (subject, description)) in (1..).zip(subjects) {
                let input = serde_json::json!({ "subject": subject, "description": description, "activeForm": format!("{subject}…") });
                let id = format!("t{number}");
                let created = ToolUse {
                    id: id.clone(),
                    name: "TaskCreate".into(),
                    input: input.to_string(),
                    ..ToolUse::default()
                };
                tasks.agent_event(&turn, AgentEvent::ToolCall(created), &mut Vec::new());
                let output = format!("Task #{number} created successfully: {subject}");
                tasks.agent_event(&turn, AgentEvent::ToolResult { id, output, is_error: false }, &mut Vec::new());
            }
            for (id, status) in [("1", "completed"), ("2", "in_progress")] {
                let input = serde_json::json!({ "taskId": id, "status": status }).to_string();
                let updated = ToolUse { id: format!("u{id}"), name: "TaskUpdate".into(), input, ..ToolUse::default() };
                tasks.agent_event(&turn, AgentEvent::ToolCall(updated), &mut Vec::new());
                let done = AgentEvent::ToolResult { id: format!("u{id}"), output: "Updated".into(), is_error: false };
                tasks.agent_event(&turn, done, &mut Vec::new());
            }
            tasks
        };
        // What the context holds, as Claude Code reports it, under the mouse.
        let mut context = panel();
        if let Some(thread) = context.thread_mut() {
            let part = |name: &str, tokens: u64| ContextPart { name: name.into(), tokens };
            thread.session.window = 200_000;
            thread.session.context = vec![
                part("System prompt", 3_213),
                part("System tools", 10_977),
                part("MCP tools", 604),
                part("MCP server instructions", 1_106),
                part("Memory files", 2_797),
                part("Skills", 5_351),
                part("Messages", 24_162),
            ];
        }
        // Messages sent while the agent works wait above the message box: one handed over, one not yet.
        let mut queued = shell();
        let turn = queued.current.clone().unwrap_or_default();
        queued.send("Make the tokenizer test pass", false, Vec::new(), &mut Vec::new());
        let test = serde_json::json!({ "command": "cargo test tokenizer" }).to_string();
        for event in [
            AgentEvent::ReplyStarted,
            AgentEvent::Text("I'll run the failing test first.".into()),
            AgentEvent::ToolCall(ToolUse { id: "q1".into(), name: "Bash".into(), input: test, ..ToolUse::default() }),
        ] {
            queued.agent_event(&turn, event, &mut Vec::new());
        }
        queued.send("Also check the escaped backslash case", false, Vec::new(), &mut Vec::new());
        let failed = AgentEvent::ToolResult { id: "q1".into(), output: "test result: FAILED".into(), is_error: false };
        queued.agent_event(&turn, failed, &mut Vec::new());
        queued.send("And keep the fix to one function, please", false, Vec::new(), &mut Vec::new());
        // The account ran out mid-turn: its notice, and the session waiting above the message box to go on.
        let mut spent = shell();
        let turn = spent.current.clone().unwrap_or_default();
        spent.send("Then make the other escape tests pass too", false, Vec::new(), &mut Vec::new());
        let lifts = unix_now() + 3_150;
        let window = LimitWindow { name: "five_hour".into(), used: 1.0, resets: lifts, label: String::new() };
        let refused = Limits { at: unix_now(), status: "rejected".into(), resets: lifts, windows: vec![window] };
        for event in [
            AgentEvent::Limits(refused),
            AgentEvent::ReplyStarted,
            AgentEvent::Text("You've hit your limit · resets 7pm (Europe/Amsterdam)".into()),
            AgentEvent::LimitReached,
            AgentEvent::TurnEnded { problem: Some("You've hit your limit · resets 7pm (Europe/Amsterdam)".into()) },
        ] {
            spent.agent_event(&turn, event, &mut Vec::new());
        }
        // What a session does when its account runs out, open under its dropdown.
        let mut out_of_limits = shell();
        out_of_limits.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::Providers))));
        out_of_limits.open_menu(Menu::OutOfLimits, Rect::new(865.0, 264.0, 280.0, 40.0));
        let mut diff = panel();
        let file = repository().files.remove(0);
        diff.modal = Some(Modal::Diff(Box::new(DiffView::new(file.clone()))));
        let repo = diff.git.folder.clone();
        diff.git_report(&repo, Report::Diff { path: file.path, text: Ok(DIFF.into()) });
        // A picture of the conversation, opened whole (the app's own icon stands in for one).
        let mut picture = shell();
        let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon/256.png");
        picture.modal = Some(Modal::Picture(icon.into()));
        let mut autonomy = shell();
        autonomy.modal = Some(Modal::Agent(Box::new(AgentForm::new(autonomy.selected_agent()))));
        autonomy.open_menu(Menu::FormAutonomy, Rect::new(725.0, 472.0, 297.0, 40.0));
        let settings = |tab: Tab| {
            let mut shell = shell();
            shell.modal = Some(Modal::Settings(Box::new(Settings::new(tab))));
            shell
        };
        // Updating the app, a click at a time: a newer version is out, comes down, waits for a restart; and a
        // download that could not be saved.
        let updating = |state: Updater| {
            let mut shell = settings(Tab::General);
            shell.updater = state;
            shell
        };
        let newer = || Version::parse("0.2.0").expect("a version");
        let downloading = Updater::Downloading { release: Release::unnamed(newer()), done: 43, size: 100 };
        let unwritable = "AtroxaBot cannot write to the folder it runs from (C:\\Program Files\\AtroxaBot): Access is \
            denied. Install the newest release instead.";
        // Neither Claude Code nor Codex installed: each account offers its maker's installer; and the
        // General page while Claude Code's installer is open in its terminal.
        let uninstalled = |tab: Tab| {
            let mut shell = settings(tab);
            for account in [Connection::CLAUDE_AGENT, "work"] {
                shell.claude_info(account, Err(ClaudeError::NotFound));
            }
            shell.codex_info("codex", Err(CodexError::NotFound), &mut Vec::new());
            shell
        };
        // The General page at its foot, where Claude Code's row is.
        let foot = |mut shell: Shell| {
            if let Some(Modal::Settings(settings)) = &mut shell.modal {
                settings.at_end = true;
            }
            shell
        };
        let mut installing = foot(uninstalled(Tab::General));
        installing.installing(Cli::Claude);
        // The close button's choices, open under their dropdown.
        let mut closing = settings(Tab::General);
        closing.open_menu(Menu::Close, Rect::new(905.0, 432.0, 240.0, 40.0));
        // Streaming: the accounts show their plans and no addresses, in the settings and the top bar's menu.
        let mut private = settings(Tab::Providers);
        private.config.hide_emails = true;
        let mut private_menu = shell();
        private_menu.config.hide_emails = true;
        private_menu.open_menu(Menu::Provider, Rect::new(1152.0, 47.0, 96.0, 30.0));
        let adding = |tab: Tab, what: Adding| {
            let mut shell = shell();
            let mut settings = Settings::new(tab);
            settings.add(what);
            shell.modal = Some(Modal::Settings(Box::new(settings)));
            shell
        };
        // The skills: two installed (one with an update), Anthropic's repository read, another being read.
        let skilled = |open: Option<usize>| {
            let mut shell = shell();
            let anthropic = skills::Record::default().repositories[0].clone();
            let mine = Source::parse("acme/agent-skills").unwrap_or_else(|_| anthropic.clone());
            let skill = |name: &str, description: &str, digest: &str| Skill {
                name: name.into(),
                description: description.into(),
                path: format!("skills/{name}"),
                digest: digest.into(),
                ..Skill::default()
            };
            let pdf = "Read, fill, merge and split PDF files, and extract their text and tables.";
            let notes = "Write release notes from the commits since the last tag, grouped by area.";
            shell.skills.installed = vec![skill("pdf", pdf, "1"), skill("release-notes", notes, "7")];
            let origin = |source: &Source, name: &str, digest: &str| Origin {
                source: source.clone(),
                path: format!("skills/{name}"),
                digest: digest.into(),
            };
            shell.skills.record.repositories.push(mine.clone());
            shell.skills.record.installed.insert("pdf".into(), origin(&anthropic, "pdf", "1"));
            shell.skills.record.installed.insert("release-notes".into(), origin(&mine, "release-notes", "7"));
            let documents = Group {
                name: "document-skills".into(),
                description: "Create, edit and analyse Word documents, spreadsheets, presentations and PDFs.".into(),
                skills: vec![
                    skill("pdf", pdf, "2"),
                    skill("docx", "Create and edit Word documents, with tracked changes and comments.", "3"),
                    skill("xlsx", "Build spreadsheets with formulas, formatting and charts.", "4"),
                ],
            };
            let examples = Group {
                name: "example-skills".into(),
                description: "Skills that show what a skill can do: design, testing, building tools.".into(),
                skills: vec![
                    skill("frontend-design", "Design distinctive, production-grade web interfaces.", "5"),
                    skill("mcp-builder", "Build MCP servers that give agents well-designed tools.", "6"),
                ],
            };
            let elsewhere = Elsewhere {
                name: "release-tools".into(),
                description: "Changelogs and version bumps.".into(),
                source: mine.clone(),
            };
            let catalog = Catalog {
                name: "anthropic-agent-skills".into(),
                groups: vec![documents, examples],
                elsewhere: vec![elsewhere],
            };
            shell.skills.catalogs.insert(anthropic.clone(), Fetched::Ready(Box::new(catalog)));
            shell.skills.catalogs.insert(mine, Fetched::Loading);
            let mut settings = Settings::new(Tab::Skills);
            if open.is_some() {
                settings.open_skills(anthropic);
            }
            shell.modal = Some(Modal::Settings(Box::new(settings)));
            shell
        };
        // The MCP servers, with what each Claude account brings: its connectors, connected or to connect.
        let mut connected = settings(Tab::Integrations);
        let server = |name: &str, status: &str, source: &str, tools: usize, error: &str| ClaudeMcpServer {
            name: name.into(),
            status: status.into(),
            source: source.into(),
            error: error.into(),
            tools,
        };
        let servers = vec![
            server("claude.ai Fish.Audio", "connected", "claudeai", 53, ""),
            server("claude.ai Claude Docs", "connected", "claudeai", 8, ""),
            server("playwright", "failed", "user", 0, "spawn npx ENOENT"),
            server("claude.ai Gmail", "needs-auth", "claudeai", 0, ""),
            server("claude.ai Google Drive", "needs-auth", "claudeai", 0, ""),
            server("claude.ai Slack", "needs-auth", "claudeai", 0, ""),
            server("claude.ai Linear", "needs-auth", "claudeai", 0, ""),
        ];
        let accounts: Vec<String> = connected
            .config
            .all_connections()
            .into_iter()
            .filter(|connection| connection.kind == ConnectionKind::ClaudeAgent)
            .map(|connection| connection.id)
            .collect();
        for (index, id) in accounts.into_iter().enumerate() {
            let known = if index == 0 {
                super::Connectors::Ready(servers.clone())
            } else {
                super::Connectors::Ready(vec![server("claude.ai Gmail", "connected", "claudeai", 12, "")])
            };
            connected.connectors.insert(id, known);
        }
        // A session in a worktree of its own: its folder menu, its side panel, and the settings' list of
        // what is on disk, with a folder left behind by a session that is gone.
        let in_worktree = || {
            let mut shell = panel();
            let worktree = Worktree {
                repo: "C:\\code\\parser".into(),
                path: "C:\\Users\\me\\.atroxabot\\worktrees\\parser-18e6a58ab5fe8230".into(),
                branch: "atroxabot/the-tokenizer-test-fails-8230".into(),
                base: "main".into(),
                start: "3f1c2aa".into(),
            };
            let session = shell.current.clone().unwrap_or_default();
            shell.worktree_ready(&session, &worktree, &worktree.path, &mut Vec::new());
            shell.git.folder = shell.work_folder();
            shell.git.status =
                Some(GitStatus { branch: worktree.branch.clone(), upstream: String::new(), ..repository() });
            let stands = worktree::State {
                exists: true,
                branch: worktree.branch.clone(),
                changes: 3,
                unmerged: 2,
                landed: false,
            };
            let orphan = worktree::Orphan {
                path: "C:\\Users\\me\\.atroxabot\\worktrees\\website-18e5f0c2aa91d4e0".into(),
                repo: "C:\\code\\website".into(),
                branch: "atroxabot/new-landing-page-d4e0".into(),
                changes: 4,
            };
            let sizes = vec![(worktree.path.clone(), 2_900_000_000), (orphan.path.clone(), 412_000_000)];
            let swept = worktree::Swept {
                states: vec![(session, stands)],
                orphans: vec![orphan],
                ..worktree::Swept::default()
            };
            shell.worktree_report(worktree::Report::Swept(swept), &mut Vec::new());
            shell.worktree_report(worktree::Report::Sizes(sizes), &mut Vec::new());
            shell
        };
        let mut worktree_menu = in_worktree();
        worktree_menu.open_menu(Menu::Folder, Rect::new(934.0, 47.0, 98.0, 30.0));
        let mut worktrees = in_worktree();
        worktrees.config.panel = false;
        worktrees.modal = Some(Modal::Settings(Box::new(Settings::new(Tab::Worktrees))));
        // An agent on a Codex account: its models, each with the efforts Codex lists for it.
        let mut codex_models = shell();
        let on_codex = Agent {
            connection: "codex".into(),
            model: String::new(),
            effort: String::new(),
            ..codex_models.agents[0].clone()
        };
        codex_models.update_agent(on_codex, &mut Vec::new());
        if let Some(thread) = codex_models.thread_mut() {
            thread.session.provider.clear();
        }
        codex_models.open_menu(Menu::Model, Rect::new(474.0, 836.0, 150.0, 30.0));
        let mut editor = shell();
        editor.modal = Some(Modal::Agent(Box::new(AgentForm::new(editor.selected_agent()))));
        let mut call = shell();
        call.call = Some(CallView {
            session: call.current.clone().unwrap_or_default(),
            state: CallState::Speaking,
            input: 0.0,
            output: 0.7,
            heard: "Can you check why the parser test fails?".into(),
            announced: true,
            hold: false,
            muted: false,
        });
        let screens = vec![
            ("first-run", Shell::new(Config::default(), Vec::new(), Vec::new())),
            ("session", shell()),
            ("welcome", welcome),
            ("approval", asking),
            ("menu-model", model),
            ("menu-folder", folder),
            ("menu-provider", provider),
            ("spotlight", spotlight),
            ("spotlight-typed", searching),
            ("stats", stats),
            ("stats-loading", loading),
            ("panel", panel()),
            ("panel-no-git", plain),
            ("subagents", delegating),
            ("plan-changed", changed),
            ("subagent-open", inside),
            ("question", asked),
            ("slash", slash),
            ("mention", mention),
            ("tasks", tasks()),
            ("queued", queued),
            ("limit-wait", spent),
            ("menu-out-of-limits", out_of_limits),
            ("menu-branch", branch),
            ("diff", diff),
            ("picture", picture),
            ("menu-autonomy", autonomy),
            ("settings-general", settings(Tab::General)),
            ("menu-close", closing),
            ("suggestion", suggested),
            ("suggestion-long", suggested_long),
            ("update-available", updating(Updater::Available(Release::unnamed(newer())))),
            ("update-downloading", updating(downloading)),
            ("update-ready", updating(Updater::Ready(Staged::nowhere(newer())))),
            ("update-failed", updating(Updater::Failed(unwritable.into()))),
            ("settings-providers", settings(Tab::Providers)),
            ("settings-providers-private", private),
            ("settings-providers-missing", uninstalled(Tab::Providers)),
            ("settings-general-missing", foot(uninstalled(Tab::General))),
            ("settings-general-installing", installing),
            ("menu-provider-private", private_menu),
            ("settings-voice", settings(Tab::Voice)),
            ("settings-integrations", connected),
            ("settings-skills", skilled(None)),
            ("skills-repository", skilled(Some(0))),
            ("add-provider", adding(Tab::Providers, Adding::PickProvider)),
            ("add-provider-key", adding(Tab::Providers, Adding::Provider(1))),
            ("add-account", adding(Tab::Providers, Adding::ClaudeAccount)),
            ("add-codex-account", adding(Tab::Providers, Adding::CodexAccount)),
            ("settings-worktrees", worktrees),
            ("menu-folder-worktree", worktree_menu),
            ("panel-worktree", in_worktree()),
            ("menu-model-codex", codex_models),
            ("add-voice", adding(Tab::Voice, Adding::Voice(1))),
            ("add-mcp", adding(Tab::Integrations, Adding::Mcp(false))),
            ("agent", editor),
            ("call", call),
        ];
        // Tooltips: the mouse rests on a row of the side panel (which starts at x 1112).
        let hovered = [("tip-task", tasks(), (1200.0, 313.0)), ("tip-context", context, (1180.0, 90.0))];
        let screens = screens.into_iter().map(|(name, shell)| (name, shell, (-1.0, -1.0))).chain(hovered);
        for (name, mut shell, mouse) in screens {
            let pixels = frame(&mut gpu, &fonts, scheme, mouse, &mut |p, ui, view| {
                shell.draw(p, ui, view, scheme, &mut Vec::new());
            });
            write_png(&dir.join(format!("{name}-{}.png", scheme.key())), SIZE, &pixels);
        }
        // The smallest window: no room beside the session, so the side panel floats over it.
        let mut narrow = panel();
        let pixels = frame(&mut gpu, &fonts, scheme, (-1.0, -1.0), &mut |p, ui, view| {
            narrow.draw(p, ui, Rect::new(view.x, view.y, 820.0, 560.0), scheme, &mut Vec::new());
        });
        write_png(&dir.join(format!("panel-narrow-{}.png", scheme.key())), SIZE, &pixels);
    }
}

/// A diff as git prints it.
const DIFF: &str = "diff --git a/src/tokenizer.rs b/src/tokenizer.rs\nindex 3f1c2aa..9b04e17 100644\n--- a/src/tokenizer.rs\n+++ b/src/tokenizer.rs\n\
@@ -38,9 +38,12 @@ impl Tokenizer<'_> {\n         while let Some(c) = self.peek() {\n             match c {\n\
-                '\\\\' => {\n-                    self.pos += 1;\n-                }\n\
+                '\\\\' => {\n+                    // Skip the escaped character too.\n+                    self.pos += 2;\n+                    continue;\n+                }\n\
                 '\"' => break,\n                 _ => self.pos += 1,\n             }\n         }\n\
@@ -71,6 +74,13 @@ mod tests {\n         assert_eq!(tokens(\"a b\"), [\"a\", \"b\"]);\n     }\n\
+\n+    #[test]\n+    fn escaped_quotes_stay_inside_the_string() {\n+        assert_eq!(tokens(r#\"\"a\\\\\"b\"\"#).len(), 1);\n+    }\n }\n";

/// A repository mid-change.
fn repository() -> GitStatus {
    let file = |path: &str, change, added, removed| GitFile { path: path.into(), change, added, removed };
    GitStatus {
        branch: "fix/escaped-quotes".into(),
        upstream: "origin/fix/escaped-quotes".into(),
        ahead: 2,
        behind: 0,
        files: vec![
            file("src/tokenizer.rs", Change::Modified, 12, 3),
            file("src/parser/expr.rs", Change::Modified, 4, 4),
            file("tests/escapes.rs", Change::Added, 38, 0),
            file("docs/old-notes.md", Change::Deleted, 0, 21),
            file("Cargo.lock", Change::Modified, 6, 6),
        ],
        branches: vec!["main".into(), "fix/escaped-quotes".into(), "feature/spans".into()],
    }
}

/// A year of made-up usage by two agents: busy days, quiet days and empty ones.
fn ledger(agents: [&str; 2]) -> Ledger {
    let today = day_of(unix_now());
    let mut records = Vec::new();
    for back in 0..380u64 {
        // A fixed scramble of the day, so every run draws the same year.
        let seed = (back * 2_654_435_761) % 97;
        if seed % 5 == 0 || (back > 200 && seed % 2 == 0) {
            continue;
        }
        for turn in 0..=(seed % 4) {
            let models = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-2", "openai/gpt-5"];
            let input = 20_000 + seed * 3_100 * (turn + 1);
            let tools = [("Read", false), ("Bash", seed % 7 == 0), ("Edit", false), ("Grep", false)];
            records.push(Record {
                provider: if (seed + turn) % 4 == 3 { "or" } else { "claude-agent" }.into(),
                provider_name: String::new(),
                day: today - back,
                agent: agents[usize::from((seed + turn) % 3 == 0)].into(),
                session: format!("s{}", back / 3 + turn),
                model: models[((seed + turn) % 4) as usize].into(),
                input,
                output: 900 + seed * 40,
                cached: input * 8 / 10,
                written: input / 20,
                cost: input as f64 * 2.5e-6,
                prompts: u64::from(turn == 0),
                tools: tools
                    .iter()
                    .take((seed % 5) as usize)
                    .map(|(name, failed)| ((*name).to_owned(), *failed))
                    .collect(),
            });
        }
    }
    Ledger { records }
}

/// Draws a screen with `draw`, the mouse resting at `mouse`, until its
/// animations settled (and a tooltip there showed), then renders the last frame.
fn frame(
    gpu: &mut Offscreen,
    fonts: &Fonts,
    scheme: Scheme,
    mouse: (f32, f32),
    draw: &mut dyn FnMut(&mut Painter, &mut Ui, Rect),
) -> Vec<u8> {
    let (mut atlas, mut images) = (GlyphAtlas::default(), ImageAtlas::default());
    let mut ui = Ui::default();
    (ui.focused, ui.mouse) = (true, mouse);
    let view = Rect::new(0.0, 0.0, SIZE.0 as f32, SIZE.1 as f32);
    let palette = scheme.palette();
    let mut instances = Vec::new();
    for second in 0..5u8 {
        // A long frame: every ease lands at once. Whole seconds pass, so the caret blinks alike in each.
        ui.begin(1.0, 10.0 + f32::from(second));
        let mut p = Painter::new(fonts, (&mut atlas, &mut images), &mut instances, 1.0, view, palette);
        draw(&mut p, &mut ui, view);
        crate::titlebar::draw(&mut p, &mut ui, view, false);
        ui.end();
        // Pictures are decoded on the spot, where the app has a worker do it.
        for key in images.take_wanted() {
            let pixels = crate::image::thumbnail(&key);
            images.insert(key, pixels);
        }
    }
    gpu.render(SIZE, &instances, (&mut atlas.uploads, &mut images.uploads), 1.0, palette.panel)
}

/// What Claude Code says of an account signed in as `email`.
fn claude(email: &str, plan: &str) -> ClaudeInfo {
    let model = |value: &str, resolved: &str, description: &str| ClaudeModel {
        value: value.into(),
        resolved: resolved.into(),
        description: description.into(),
        efforts: ["low", "medium", "high", "xhigh", "max"].map(str::to_owned).into(),
        auto_mode: true,
        ..ClaudeModel::default()
    };
    let mut info = ClaudeInfo {
        models: vec![
            model("default", "claude-opus-5-5", "Opus 5.5 · Best for everyday, complex tasks"),
            model("opus", "claude-opus-5-5", "Opus 5.5 · Best for everyday, complex tasks"),
            model("fable", "claude-fable-5-2", "Fable 5.2 · Most capable for your hardest and longest-running tasks"),
            model("sonnet", "claude-sonnet-5-5", "Sonnet 5.5 · Efficient for routine tasks"),
            model("haiku", "claude-haiku-5-5", "Haiku 5.5 · Fastest for quick answers"),
        ],
        version: "2.1.322".into(),
        ..ClaudeInfo::default()
    };
    info.account.email = email.into();
    info.account.subscription = plan.into();
    info
}

/// Three agents, the first with a session two turns in: reasoning, tool
/// calls (one open, one failed) and an answer with code.
fn shell() -> Shell {
    let scout = Agent {
        tagline: "Finds and fixes bugs in this repository.".into(),
        instructions: "You are a careful engineer. Reproduce a bug before you fix it, and keep changes small.".into(),
        workdir: "C:\\code\\parser".into(),
        effort: "high".into(),
        autonomy: Autonomy::Edits,
        ..Agent::new("Scout")
    };
    let mut press =
        Agent { tagline: "Drafts the weekly newsletter.".into(), connection: "or".into(), ..Agent::new("Newsletter") };
    press.created += 1;
    let mut inbox = Agent { tagline: "Triages what comes in.".into(), ..Agent::new("Inbound") };
    inbox.created += 2;

    let mut session = Session::new(&scout.id);
    session.workdir = "C:\\code\\parser".into();
    session.prompt("The tokenizer test fails on escaped quotes. Can you find out why and fix it?", "");
    let call = |id: &str, name: &str, input: &str| {
        AgentEvent::ToolCall(ToolUse { id: id.into(), name: name.into(), input: input.into(), ..ToolUse::default() })
    };
    let result = |id: &str, output: &str, is_error: bool| AgentEvent::ToolResult {
        id: id.into(),
        output: output.into(),
        is_error,
    };
    let answer = "Found it. The escape branch skipped the backslash but not the character after it, so `\\\"` still \
                  ended the string.\n\n```rust\n'\\\\' => {\n    // Skip the escaped character too.\n    self.pos += 2;\n    \
                  continue;\n}\n```\n\nThe test passes now, and the other **14 tokenizer tests** still do.";
    let usage = Usage { input_tokens: 48_210, output_tokens: 1_204, cached_tokens: 44_100, cache_write_tokens: 0 };
    let edit = r#"{"file_path":"src/tokenizer.rs","old_string":"self.pos += 1;","new_string":"self.pos += 2;"}"#;
    let events = [
        AgentEvent::ReplyStarted,
        AgentEvent::Thinking("The failure mentions an escaped quote. The escape branch probably ends early.".into()),
        AgentEvent::Text("I'll run the failing test first, then read the tokenizer.".into()),
        call("t1", "Bash", r#"{"command":"cargo test tokenizer::escaped_quotes"}"#),
        result("t1", "test tokenizer::escaped_quotes ... FAILED\n  left: 2\n right: 1\n[exit code 101]", true),
        call("t2", "Read", r#"{"file_path":"src/tokenizer.rs"}"#),
        result("t2", "   41  '\\\\' => { self.pos += 1; }\n   42  '\"' => break,", false),
        call("t3", "Edit", edit),
        result("t3", "Edited src/tokenizer.rs (1 replacement).", false),
        AgentEvent::Text(answer.into()),
        AgentEvent::ReplyEnded {
            raw: Vec::new(),
            wire: atroxabot_core::Wire::default(),
            model: "claude-opus-5-5".into(),
            usage,
            cost: 0.18,
            problem: String::new(),
        },
        AgentEvent::TurnEnded { problem: None },
    ];
    for event in &events {
        session.apply(event);
    }
    let mut earlier = Session::new(&scout.id);
    earlier.prompt("Review the error messages in the CLI", "");
    earlier.updated -= 7_200;
    let mut other = Session::new(&press.id);
    other.prompt("Draft this week's issue from the changelog", "");
    other.apply(&AgentEvent::Text("Here is a first draft of issue 42, in three sections.".into()));
    other.updated -= 3_600;

    let work = Connection {
        id: "work".into(),
        name: "Work".into(),
        config_dir: "/accounts/work".into(),
        ..Connection::claude_agent()
    };
    let router = Connection {
        id: "or".into(),
        kind: ConnectionKind::OpenAi,
        name: "OpenRouter".into(),
        base_url: "https://openrouter.ai/api/v1".into(),
        api_key: Some("sk-or-secret".into()),
        ..Connection::default()
    };
    let codex = Connection { id: "codex".into(), ..Connection::codex("Codex") };
    let config = Config {
        connections: vec![work, router, codex],
        voice: VoiceSettings { provider: "fish".into(), fish_key: Some("secret".into()), ..VoiceSettings::default() },
        mcp: vec![McpServer {
            name: "files".into(),
            command: "npx -y @modelcontextprotocol/server-filesystem C:\\notes".into(),
            enabled: true,
            ..McpServer::default()
        }],
        recent_folders: vec!["C:\\code\\parser".into(), "C:\\code\\website".into(), "C:\\Users\\me\\notes".into()],
        ..Config::default()
    };
    let summaries = vec![session.summary(), other.summary(), earlier.summary()];
    let mut shell = Shell::new(config, vec![scout, press, inbox], summaries);
    shell.claude_info(Connection::CLAUDE_AGENT, Ok(claude("me@example.com", "Claude Max")));
    shell.claude_info("work", Ok(claude("", "")));
    let model = |id: &str, name: &str, description: &str, is_default| CodexModel {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        efforts: ["low", "medium", "high", "xhigh"].map(str::to_owned).into(),
        default_effort: "medium".into(),
        is_default,
        hidden: false,
    };
    let codex = CodexInfo {
        version: "0.167.1".into(),
        account: Some(CodexAccount { kind: "chatgpt".into(), email: "me@example.com".into(), plan: "plus".into() }),
        models: vec![
            model("gpt-6-astra", "GPT-6-Astra", "Frontier intelligence for the most demanding work.", false),
            model("gpt-6.1-sol", "GPT-6.1-Sol", "Latest workhorse model for coding and everyday work.", true),
        ],
        ..CodexInfo::default()
    };
    shell.codex_info("codex", Ok(codex), &mut Vec::new());
    let listed = vec![
        Model { id: "anthropic/claude-opus-5.5".into(), name: "Claude Opus 5.5".into(), ..Model::default() },
        Model { id: "openai/gpt-5".into(), name: "GPT-5".into(), ..Model::default() },
    ];
    shell.models_loaded("or", Ok(listed));
    let id = session.id.clone();
    let mut thread = Thread::new(session);
    thread.open_block(1, 3);
    shell.threads.insert(id.clone(), thread);
    shell.current = Some(id);
    shell
}

/// Writes RGBA `pixels` as a PNG.
fn write_png(path: &Path, (width, height): (u32, u32), pixels: &[u8]) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("snapshot file"));
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG header");
    writer.write_image_data(pixels).expect("PNG data");
}

/// Draws the app's icon (its mark on a dark tile) at every size the
/// platforms want, into the folder `ATROXABOT_ICON` names (the test runs in
/// `crates/desktop`, so a relative path starts there):
/// `ATROXABOT_ICON=assets/icon cargo test -p atroxabot icon -- --ignored`
/// It also draws the logo for use outside the app into `branding/` at the
/// workspace's root: square to its corners, as whoever shows it rounds it.
#[test]
#[ignore = "draws on the GPU; run by hand to make the icon again"]
fn icon() {
    let Some(dir) = std::env::var_os("ATROXABOT_ICON").map(PathBuf::from) else { return };
    let Some(mut gpu) = Offscreen::new() else { return };
    let fonts = Fonts::load();
    let branding = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../branding");
    std::fs::create_dir_all(&branding).expect("branding folder");
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 256, 1024].map(|size| (size, false));
    for (size, square) in sizes.into_iter().chain([(500, true), (2000, true)]) {
        let (mut atlas, mut images) = (GlyphAtlas::default(), ImageAtlas::default());
        let side = size as f32;
        let view = Rect::new(0.0, 0.0, side, side);
        let mut instances = Vec::new();
        let palette = Scheme::Black.palette();
        let mut p = Painter::new(&fonts, (&mut atlas, &mut images), &mut instances, 1.0, view, palette);
        // macOS wants air around the tile; the others fill the square.
        let inset = if size == 1024 { side * 0.1 } else { 0.0 };
        let tile = Rect::new(inset, inset, side - 2.0 * inset, side - 2.0 * inset);
        // The square one keeps its mark clear of a round crop.
        let (radius, mark) = if square { (0.0, tile.w * 0.85) } else { (tile.w * 0.225, tile.w) };
        p.gradient(tile, crate::paint::hex(0x242428), crate::paint::hex(0x0C0C0E), radius);
        p.logo(crate::logo::Logo::App, (side * 0.5, side * 0.5), mark, crate::paint::hex(0xF2EEE6));
        let pixels = gpu.render((size, size), &instances, (&mut atlas.uploads, &mut images.uploads), 1.0, [0.0; 4]);
        let path = match size {
            _ if square => branding.join(format!("atroxabot-{size}.png")),
            1024 => dir.join("macos.png"),
            _ => dir.join(format!("{size}.png")),
        };
        write_png(&path, (size, size), &pixels);
    }
}
