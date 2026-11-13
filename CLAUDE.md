# CLAUDE.md

AtroxaBot is a native desktop app for agents the user chats with and calls. It is a Cargo workspace:
`crates/claude-agent` (the Rust port of the Claude Agent SDK's transport), `crates/codex` (the transport
for the installed Codex CLI's app server), `crates/mcp` (a small MCP client), `crates/core` (agents, sessions, API client, agent loop and tools, usage statistics, git, the sessions' worktrees, updating the app; no GUI), `crates/voice` (Fish
Audio and ElevenLabs, the call loop, audio devices) and `crates/desktop` (the app; package and binary
`atroxabot`). `README.md` describes every feature, the crate layout and the storage format: read it before
changing behaviour, and keep it current when you do.

```sh
cargo run --release -p atroxabot
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Hard requirements

- **Anthropic's terms, for Claude accounts.** Run the user's installed Claude Code binary as published:
  never bundle, modify or wrap it, and never pass `--bare`. Never read, store, set, strip or forward its
  credentials (no credentials files, no `ANTHROPIC_API_KEY`, no OAuth tokens), and never add a Claude
  sign-in to the app: signing in is `claude auth login`, started in a terminal the user sees, and installing
  is Anthropic's own installer (`CLAUDE_INSTALL`), started the same way: the app never downloads or places
  the binary itself. The app
  sets `CLAUDE_CONFIG_DIR` to select an extra account's configuration folder, which is how Anthropic
  documents using several accounts (the app never looks inside that folder), and, as the official Agent
  SDK does, `CLAUDE_CODE_SDK_READS_SESSION_STATE`. Whatever else the official SDK does is fine to do when
  it serves the app better than our own way; but do not set `CLAUDE_CODE_ENTRYPOINT` or
  `CLAUDE_AGENT_SDK_VERSION`, which would say the app is Anthropic's SDK. The UI says "Claude", never "Claude Code" as a
  product name of ours (a plain statement that it runs the user's Claude Code is fine). The README's
  section on this is the reference; re-read Anthropic's pages linked there before changing how the binary
  is started.
- **Codex accounts, the same way.** Run the user's installed Codex binary as published (`codex
  app-server`), never a `.cmd` shim. Never read, store or forward its credentials, never send
  `account/login/start`, and refuse its requests for tokens: signing in is `codex login`, started in a
  terminal the user sees, and installing is OpenAI's own installer (`CODEX_INSTALL`), the same way. The
  app sets `CODEX_HOME` to select an extra account's folder and never looks
  inside it. What the app gives Codex (MCP servers, the skill library) is for the run only: nothing is
  written to an account's configuration.
- **Rust-native, custom UI.** `winit` + `wgpu`, and we draw the app's interface ourselves. No UI
  frameworks (egui, iced, Tauri, …).
- **Beautiful, elegant and fast.** Redraw only when something changes, do minimal work per frame, and keep
  the single-pipeline, single-draw-call renderer (`gpu.rs` + `shader.wgsl`). New visual effects go into
  that pipeline as new quad kinds, never as extra passes.
- **Cross-platform:** Windows, macOS and Linux.
- **Dependencies are costly.** Add one only if it is truly required and can't reasonably be written by
  hand, with a `# why:` comment. No `thiserror` (write error types by hand: `Display` +
  `std::error::Error`), no `anyhow`, no async runtimes (threads and blocking I/O), no convenience crates.
- **Code quality:** every public item has doc comments, `clippy::pedantic` stays clean, no `unwrap`s on
  fallible runtime paths, non-trivial logic gets a small unit test. Parsers survive hostile input.
- **Token discipline:** within a session the system prompt and tool list do not change and history is only
  appended to (this keeps the prompt cache and Claude's thinking blocks valid). That goes for an account's
  process too: it is started again under the prompt the session began with (`Session::account_prompt`),
  takes another model or effort as it runs (`ClaudeAgent::retune`), and is kept a while after the user
  looks elsewhere (`shell/warm.rs`) rather than closed and resumed. Never edit or reorder
  earlier turns of a request; add a note to the newest user message instead (see `CALL_STARTED`).
- **Secrets:** never log an API key. Config files are written atomically, `0600` on Unix. A key is only
  sent to the address it was saved for. Agents' file tools never reach `~/.atroxabot` (`tools::risk`),
  except to read the installed skills (`library/skills/`) and to work in the sessions' worktrees
  (`worktrees/`).
- **Worktrees clean up after themselves** (`core/src/worktree.rs`, whose module comment is the rule book).
  Never leave one behind: whatever makes a worktree records it on its session, whatever removes a session
  removes its worktree, and nothing is removed while a `worktree::Lease` is held or while it has
  uncommitted changes the user was not told of. A new way to make or drop a worktree gets a test against
  real git there.
- **Updates only when asked** (`core/src/update.rs`, whose module comment is the rule book). The app never
  looks for a release by itself: not at start, not on a timer, not in the background. Looking, downloading
  and restarting are each the user's click in Settings, General. A release is taken only when its manifest
  is signed with the key in `packaging/update-key.pub`, is newer than what runs, and the download has the
  manifest's size and SHA-256: never weaken or skip one of these, and never put the key's private half in
  the repository. The version lives only in `Cargo.toml`'s `[workspace.package]` (`packaging/release.sh`
  changes it and tags it); the release pipeline, the installer and the app all read it from there.
- **Safety:** links from replies only open `http(s)` and `mailto`. A tool call that changes something,
  runs a command or goes online is asked about unless the agent's autonomy allows it.
- **Testing:** the owner tests the GUI. Don't drive the app with computer use; `cargo build`, `clippy`
  and `test` are enough. To look at a visual change, render the screens offscreen:
  `ATROXABOT_SNAPSHOTS=<folder> cargo test -p atroxabot snapshots -- --ignored` saves each scene in
  `shell/snapshot.rs` as a PNG per theme (add a scene there for a new surface). Tests that use real
  services are `#[ignore]`d and spend the owner's usage: run one only when a change needs it.

## Design

Modern, quiet and uncluttered. The only colour on screen is an agent's own (`theme::agent_color`), on its
face (`orb.rs`) and, faintly, washed down the top of its canvas; everything else is the theme's greys.
The exceptions are what a diff adds and removes (`Palette::positive`, `Palette::danger`) and a provider's
mark: its platform's logo (`logo.rs`, drawn from an outline through the glyph atlas with `Painter::logo`,
in `theme::provider_color` where the platform has a colour), or a picture the user put in
`~/.atroxabot/icons/`. The owner decided the logos are fine to show as nominative use: they only say
which platform a provider is. Never use one as the app's own branding.

- The shell: the app draws its own title bar (`titlebar.rs`). The sidebar sits on the window
  (`Palette::panel`); the session floats beside it on a rounded canvas (`Palette::bg`), and the side
  panel (`shell/panel.rs`: usage, tasks, subagents, plan limits and git, which it shows but does not
  commit or push: that is the agent's) on another. The agent editor, the settings, the statistics and
  a diff are modals (`widgets::modal`), the spotlight and an opened picture (`shell/picture.rs`) ones of
  their own shape; menus, the approval card and
  the composer float on `Palette::surface` with a soft shadow. The top bar holds only what belongs to
  the session; the sidebar holds the app's as icons: search and a new agent over the agents, settings
  and statistics at its foot. Under the message box: the paperclip at the left, the model at the right.
- Whether a session works in a worktree of its own is a switch in the folder menu of the top bar, the
  session's until its worktree is made; what is on disk is listed in the settings (`shell/worktrees.rs`).
- A tool call that changes a file shows as the lines of a diff (`shell/change.rs`), and a turn that
  changed files lists them under its last reply. Text is found in the conversation with a bar that floats
  at its top (`shell/find.rs`); what is marked or found is washed under the text by the document itself
  (`Doc::mark`, `Doc::mark_selection`), so it shows in a code block too.
- An account that ran out of usage is said in the conversation and in the tray over the message box,
  never as a notice over the session. Notices (`Shell::notify`) are for problems nothing else shows.
- An agent has no model or effort in its editor: both are chosen under the message box. There is no
  "auto" effort; `Shell::effort_in_use` gives every model a level of its own.
- The provider, model and effort are the session's (`Session::provider`, read through
  `Shell::agent_in_use` and `Shell::agent_of`, never straight from `Agent::connection`): a session keeps
  what it was left with, the top bar's chip swaps the open session's only, and the agent's own (its
  editor's provider) is where its next sessions start.
- Nothing of a session is changed or saved while its messages are still being read (`Shell::loading`):
  it would be saved over them. What is sent meanwhile waits for them.
- What the agent needs from the user rides on a card over the message box: an approval (`draw_ask`) or a
  question (`shell/question.rs`). What the user sends while the agent works waits in a tray just above the
  message box until the agent reads it at its next step (`shell/queue.rs`); it is never a separate list or
  window. A session an account's usage limit stopped says in that tray when it goes on by itself
  (`shell/limits.rs`); it only does while the user leaves it be, and never writes in the user's name. A subagent is a card in the conversation and a row in the side
  panel's list under the tasks; either opens it in the conversation's place (`shell/thread.rs`), never as
  tabs or a separate window.
- An agent's system prompt is its own (`Agent::system_prompt`, plus `CLAUDE_TOOLS` on a Claude account),
  not Claude Code's preset: keep it short, and add to it only what an agent here needs.
- Say it with a control, not a sentence: a dropdown for one of many (`widgets::dropdown` + `Menu`), option
  cards for one of a few that need explaining, a switch for on or off, an icon where one is understood.
  Adding something is a button, then a choice of what, then only the fields that choice needs.
- Whatever scrolls uses `widgets::Scroll`: the wheel moves a target that never leaves the content, and the
  view eases after it. Call its `update` before drawing what scrolls and its `bar` after: the bar thickens
  under the mouse and drags, and while it is held (`Ui::held`) nothing else is hovered or clicked.
- Colours and sizes live in `theme.rs`, from the active `Palette` (`p.theme`), never hard-coded, so every
  theme (Black default, White, Amber) keeps working.
- Icons come from the Lucide font (`ui::icon`): add a glyph by its codepoint from `lucide-static`'s
  `font/lucide.css`; the icon test checks every one resolves.
- An agent's face animates only while it is busy or on a call, so an idle app draws nothing.
