# AtroxaBot

A native, GPU-rendered desktop app for agents you chat with and call. You define agents; each has many
sessions, each session works in a folder of its own (or in a git worktree of its own); and an agent
thinks through one of your Claude accounts, your Codex account, Anthropic's API, or any OpenAI-compatible
gateway. It runs on Windows, macOS and Linux, and it
is pure Rust: no web view and no UI framework.

```sh
cargo run --release -p atroxabot      # binary name: atroxabot
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## What it does

- **Agents.** An agent is a name, a face (drawn in its colour, or a picture: click the face in its
  editor), instructions, a provider, how much it may do alone, whether it keeps notes, and a voice. The
  sidebar lists them; the selected one's sessions hang under it. Ctrl/Cmd+Shift+N makes one; the pen on
  an agent's row edits it.
- **Files and pictures.** The paperclip under the message box picks files to send along; so does dropping
  them on the window, or pasting (Ctrl/Cmd+V) a copied picture or copied files. They ride under the
  message as pills (a click takes one off) and show in the conversation with it; a click on a picture
  there opens it whole over the screen (as large as fits, never larger than it is), and a click beside
  it, the cross or Esc puts it away. A picture (PNG, JPEG,
  GIF or WebP, up to 3.5 MB) goes to the model as content, so it sees it; any other file is named to the
  agent by path, which reads it with its own tools. Typing `@` in the message box lists the files of the
  session's folder (what git tracks or would; without a repository, what is there short of hidden and
  dependency folders): type on to narrow it, arrows move, Tab or Enter writes the picked file's path
  into the message, Esc puts the list away.
- **Questions.** When an agent on a Claude account asks you something (Claude's `AskUserQuestion`: say
  "interview me"), a card over the message box shows one question at a time with its options. Click or
  press its number to pick, type to answer in your own words, Enter goes on and sends when every question
  is answered, Tab moves between questions, Esc dismisses.
- **Subagents.** An agent that delegates shows each subagent as a card in the conversation (what it is
  for, what it is doing now, and what it took), and the side panel lists the turn's subagents under the
  tasks, each with how it is doing. A click on either opens the subagent's own conversation in the
  session's place (what it was asked, what it said and every tool it called), under a bar that leads back.
- **Tasks.** When an agent keeps a task list (Claude Code's `TaskCreate` and `TaskUpdate`, or `TodoWrite`),
  the side panel shows it: what is done, what it is doing now and what is still to do; while it works,
  the conversation names the task in hand. Resting the mouse on a task that is cut short (or that the
  agent described) shows it whole, with its description. Claude Code gives a session an app starts those tools only
  when they are named as allowed (`--allowedTools`), so AtroxaBot names them.
- **Commands and skills.** Type `/` in the message box for the list: `/clear` empties the session (its
  messages are deleted and the agent starts over with an empty context, while the session keeps its place,
  its name in the sidebar until the next message names it, and its folder), `/compact` has a Claude
  account summarise its context, and every command and skill Claude Code offers
  (yours included) is there by name. Arrows move, Tab or Enter takes one.
- **Sessions.** Every agent has as many sessions as you like (Ctrl/Cmd+N starts one). A session shows the
  agent's answer as Markdown (code highlighted, with a copy button), its reasoning folded away, and every
  tool call as a row that opens to its input and output. Text can be selected with the mouse, across
  messages too, and copied.
  Sessions are saved as they go and reopen where they were. Click a session's title in the top bar to
  rename it: Enter or a click elsewhere keeps the new name, Esc the old one. In the sidebar a session
  stays where it is, however busy it gets: a new one starts on top, and you drag one up or down to put
  it where you want it. That order is remembered.
- **Messages while it works.** The message box stays open while the agent works: Enter puts what you
  wrote in line, in a tray above the message box, and the agent reads it at its next step, as soon as the
  tools it is running are done, then carries on with what it was doing (the message appears in the
  conversation where it was read). If the turn ends first, it is the next message; several waiting are
  sent as one. Until the agent has it, a waiting message can be edited (the pen puts it back in the
  message box) or removed. *Send now* (or Ctrl/Cmd+Enter) stops the step in progress so the agent reads
  it at once, with everything it did so far; Stop (or Esc) stops the turn and puts what waits back in the
  message box. A command (`/compact`) waits for the turn to end. On a Claude account this is Claude Code's
  own queue: the app hands a message over while a tool runs, and Claude Code says when it takes it in.
  Claude Code also carries on by itself when something sent to the background (a subagent, a command)
  ends after the turn did: a line in the conversation says what ended, and the agent shows it is working
  from the moment Claude Code says it runs again ("Thinking…" while it reasons, since summarised reasoning
  only arrives once it is done).
- **A folder per session.** The folder chip at the right of the top bar says where the session works;
  click it for the folders used lately, or to choose another. A new session starts in the folder the
  agent's last one worked in.
- **Worktrees.** The folder menu has a switch, *Work in a worktree*: the session then works on a branch
  of its own (`atroxabot/<what it is about>-<id>`), checked out in a folder of its own under
  `~/.atroxabot/worktrees/`, so several sessions change one repository without touching each other or your
  checkout. It is chosen before the session's first message (an agent's next session starts the way its
  last one did), made when that message goes, and the agent is told where it is and to commit there. The
  top bar shows the folder it stands for with a branch icon; the side panel says what it came from and how
  many commits are still to merge. Worktrees are cleaned up by themselves (Settings, Worktrees):
  - one whose work is **merged** into the branch it came from (fast-forwarded, merged or squashed, into
    the local branch or what it tracks) goes at once, with its branch;
  - one with nothing uncommitted goes once its session has been **unused** for a while (three days unless
    you choose a day, a week, a month or never); its branch stays for as long as it holds commits that are
    not merged, and goes when they are;
  - a session used again after that gets its worktree back, in the same folder: from its branch, or from
    the branch it came from as that is now;
  - one with **uncommitted changes** is never cleaned up by itself;
  - **deleting a session** takes its worktree with it: the armed delete button says how many uncommitted
    changes go with it; changes it did not know of are kept, as a folder the settings list;
  - a folder left **without a session** (after a crash, say) is removed when it is clean and listed when
    it is not, with its size and a button to remove it; branches of the app's that nothing uses are deleted
    once merged, and git's records of folders that are gone are pruned.
  All of that runs when the app starts, when you come back to its window (at most once a minute), and
  from *Clean up now*; nothing touches a worktree while a turn or an agent's process works in it. The
  settings page lists every worktree on disk with what it holds and how much room it takes.
- **Providers**, chosen per agent and swapped from the chip in a session's top bar:
  - **Claude accounts**: the Claude Code you installed, driven with the Claude Agent SDK's protocol, with
    its own tools, permissions, subagents, skills and MCP servers. Several accounts (work, personal) are
    kept apart; see *Claude accounts and Anthropic's terms*.
  Click a provider's name in the settings to rename it, the first Claude account included.
  - **Codex accounts**: the Codex CLI you installed, driven through its app server, with its own tools,
    sandbox, approvals and sub-agents, on your ChatGPT plan or API key. Several accounts are kept apart;
    see *Codex accounts*.
  - **Claude API**: Anthropic's Messages API with your API key.
  - **OpenAI-compatible**: OpenRouter, Inworld, OpenAI, Ollama, or any other Chat Completions gateway.
  With an API provider the app runs the agent loop itself, with its own tools: `read`, `write`, `edit`,
  `list`, `search`, `shell` and `fetch` (and, on the Claude API, Anthropic's web search and fetch).
  Swapping provider mid-session works: a Claude or Codex account that never saw the conversation is
  handed what was said so far. Swapping to another account of the same kind (one Claude account for
  another) keeps the model and effort, and takes away the notice of the account that was left.
- **When an account runs out** (Settings, Providers): when a Claude or Codex account's usage limit stops a
  turn, the session can *Wait for limit refill* (the default: it goes on by itself a minute after the limit
  lifts, so you can walk away; when the provider did not say when, it tries again every 15 minutes), *Continue using different account* (the agent moves at once to another
  signed-in account of the same kind that has usage left, the least used first, keeping its model and
  effort, and that account is handed the conversation; with none left it waits for the first to lift), or
  *Stop*. While it waits, a row above the message box says when it goes on, with *Continue now* and a
  button to cancel; what you sent while the agent worked waits there too and goes with it. A line in the
  conversation says why it went on and on which account, and the agent is told it was cut short and to
  carry on; nothing is written in your name. Whatever you do first wins: sending a message, switching the
  agent's provider, clearing or deleting the session cancels the wait. A session the limit stops five turns
  in a row stops trying. The app wakes for it while it is in the tray; a wait does not outlive the app.
- **Models from the provider.** The model menu at the right under the message box lists what the provider itself
  offers: a Claude account's list comes from Claude Code (so it is always current; of a family listed in
  several versions only the newest is shown), a Codex account's from Codex (what its own picker lists, each
  with the efforts and the usual effort Codex gives it), an API provider's from
  its `/models`. The provider's default is the one in use until you pick another; any model id can be
  typed. The same menu sets the effort, which is always one of the model's own levels: Opus, Haiku and
  Fable start at medium, Sonnet at high, and picking another model starts at that model's level. A model
  that takes no effort shows none. Picking an effort closes the menu. The model and effort are the
  session's: each session keeps its own, so switching sessions switches them too, and a new session starts
  with the last ones picked. A session moved to another provider takes that provider's choice; moved to
  another account of the same kind, it keeps its own. A message
  typed and not sent stays with its session (until the app quits), files attached included.
- **Autonomy**, set in the agent's editor: *Ask first*, *Edit freely*, *Auto* (what a new agent starts
  with: Claude Code's auto mode, with a Claude account), *Full autonomy* or *Read only*. With a Codex account these are Codex's own: *Ask first* asks before
  any change or command, *Edit freely* and *Auto* work freely in the folder inside Codex's sandbox and ask
  for what reaches beyond it, *Full autonomy* runs without sandbox or questions, *Read only* refuses what
  would change something. A call that needs approval waits on a card above
  the message box: Enter allows it, Esc denies it, *Always allow* stops asking for the session. The app's
  own data folder, which holds API keys, is off limits to every agent's file tools.
- **Integrations** (Settings, Integrations): MCP servers, local commands or remote addresses whose tools
  every agent gets. Claude accounts also keep what each account has: the connectors added on claude.ai (Gmail, Drive,
  Slack, …) and the servers set up in Claude Code itself. The same page lists those per account, as
  Claude Code reports them (`mcp_status`, asked without sending a message, so nothing is used): which are
  connected and with how many tools, which failed, and which are still to connect on claude.ai (a button
  opens claude.ai's connector page). They are only shown: they belong to the account, and only its own
  sessions get them.
- **Skills** (Settings, Skills): folders of instructions (a `SKILL.md` and the files it needs) that every
  agent has, on every Claude account and every API provider. An agent sees each skill's name and
  description and reads the rest only when a task calls for it. Skills come from repositories: Anthropic's
  `anthropics/skills` is on the list from the start, and *Add repository* takes `owner/repo` (with `#ref`
  or `@ref`), a GitHub page (`…/tree/<branch>/<folder>` too), any Git address, or a folder on this computer,
  the way Claude Code's `plugin marketplace add` takes them. A repository opens to what it offers: a
  Claude Code plugin marketplace (`.claude-plugin/marketplace.json`) is read as Claude Code reads it, its
  skills grouped by plugin, and a plugin it lists from another repository opens in turn; any other
  repository is searched for `SKILL.md` files. GitHub repositories come as GitHub's own archive, so they
  need no Git (a private one is fetched with your Git and its sign-in); other repositories need Git.
  *Install* copies a skill into the library; when its repository has changed it since, the skill offers
  *Update*. An agent on a Claude account gets the library as a plugin for the session (`--plugin-dir`), so
  its skills are also listed as `library:<name>` there; an agent on an API gets a `skill` tool that lists
  them and loads one, and may read a skill's files. A change reaches an agent's next session (an API
  session keeps the skills it started with while the app runs, so its tool list stays the same). Skills you
  installed in Claude Code itself, and a project's own `.claude/skills/`, still work on Claude accounts.
- **Spotlight.** Ctrl/Cmd+K (or the magnifier over the agents) opens one field over everything:
  it finds sessions by title at once and by what was said in them a moment later, agents by name, and
  things to do (a new session, a new agent, the statistics, the settings, the side panel). Arrows move,
  Enter opens, Esc closes.
- **The side panel** (the last icon of the top bar) shows what the session used and how full its context
  is; resting the mouse on the context shows what it holds: the system prompt, the tools (the app's or
  Claude Code's own, and those of MCP servers), the skills, memory files, the messages and the free
  space. A Claude account says so itself after each turn (`get_context_usage`, as `/context` shows it,
  which uses nothing); for an API provider the app sizes each part of its request and gives the
  conversation the rest of the tokens the provider counted. The session keeps the latest breakdown. It
  also shows the git repository of its folder: the branch (a click switches it), how far it is from what it
  tracks, and the changed files with their added and removed lines (a click opens the diff). Committing
  and pushing are left to the agent: the panel only shows. It uses the `git` you have installed and reads
  the repository again when a turn ends and when you come back to the window. For a Claude account it also
  shows where the plan's limits stand (the 5-hour and weekly windows, the week's limit for Fable once a
  turn ran on it, and when each starts over); the account is the one the top bar names, so picking
  another there shows its numbers at once, and its address is not put on screen here. They are what
  Claude Code reports for that account with each turn, the same numbers its own `/usage` prints (an
  account that has not replied yet says so); a window that is used up says *Limit reached* and when it
  starts over, and reaching a limit is said in a notice that names the account, counts down to when it
  lifts and goes away by itself then. A Codex account's limits are shown the same way, as Codex reports
  them (when the app starts and with each turn). A message after the
  account was switched goes to a Claude Code started for the new account, even if the old one was still
  answering when it was switched.
- **Statistics** (the chart at the foot of the sidebar, beside the settings; counted from a usage log of
  their own, so deleting a session does not lose what it used): tokens, cost, sessions, tool
  calls, cache hits, active days, streaks and the busiest day over the last 7, 30 or 90 days, a year or
  all time; a year of days as a grid that is brighter the more was used (by tokens, cost or tool calls); the period day by day; where
  the tokens went (output, new input, written to and read from the cache); what each provider, model,
  agent and tool used; and where each Claude account's plan limits stood when Claude Code last reported
  them (kept between runs; nothing is asked of Anthropic to show them).
- **Provider marks.** A provider wears its platform's logo (Claude, Anthropic, OpenAI, OpenRouter, Meta,
  Gemini, Mistral, Ollama), drawn from its outline at any size, in the platform's colour where it has
  one; others get an icon. The marks are their owners' trademarks, shown only to say which platform a
  provider is. To use a picture of your own instead, put an image (`png`, `jpg` or `webp`) in
  `~/.atroxabot/icons/` named after the provider: `claude`, `anthropic`, or the name in an API's address
  (`openrouter`, `openai`, …).
- **Voice calls.** The phone in the top bar calls the agent: it listens until you pause, transcribes,
  answers, and speaks sentence by sentence; talk over it and it stops. Speech-to-text and text-to-speech
  come from **Fish Audio** or **ElevenLabs** (add one under Settings, Voice; a voice per agent). A call is
  part of the session, so it can be read back. M mutes, Esc hangs up. Use headphones: there is no echo
  cancellation.
- **Memory.** An agent can keep notes between sessions, in a file of its own that rides along with every
  session and that it updates itself.
- **Token discipline.** A session's system prompt and tool list do not change and history is only appended
  to, so every request reuses the provider's prompt cache. Tool outputs are bounded (long files read in
  pages, long outputs cut in the middle, both saying so). Claude's thinking blocks are replayed unchanged,
  so it keeps its reasoning between tool calls. Long conversations are summarised: by Anthropic's
  server-side compaction on its own API, by a request of the app's elsewhere. The side panel shows how
  full the context is and what the session cost.
- **Closing the window** keeps the app running by default, so agents and calls go on (Settings, General,
  *When you close the window*). On Windows the window goes to the tray: a click on the icon by the clock
  opens it again, and its menu opens it or quits; starting AtroxaBot again also brings the window back. On
  macOS the app hides as with Cmd+H, and its Dock icon brings it back. Linux has no tray here, so there the
  window is minimised. The other choices are to minimise or to quit. Ctrl+Q quits (Cmd+Q on macOS).
- **Looks.** The app draws its own title bar, the sidebar sits on the window and the session floats beside
  it on a rounded canvas, with the side panel on one of its own; the agent editor, the settings, the
  statistics, the spotlight, a file's diff and a picture open over them as modals. Three themes: Black
  (the default), White and Amber. Zoom with Ctrl/Cmd + and -. Scrolling eases and never overshoots.

## Claude accounts and Anthropic's terms

Claude accounts exist to be used within Anthropic's terms for Claude Code and the Agent SDK, and the code
is written to keep them there:

- AtroxaBot **starts the Claude Code binary you installed, as published**. It does not bundle, modify,
  patch or wrap it.
- AtroxaBot **never handles your sign-in**. It offers no Claude sign-in of its own and never reads, stores
  or forwards Claude credentials or tokens. *Sign in* opens Claude Code's own `claude auth login` in a
  terminal, which signs you in through Anthropic's flow. Usage is billed to the account you sign in to,
  under your own agreement with Anthropic.
- **Several accounts** use the mechanism Anthropic documents for exactly that: each extra account is a
  Claude Code configuration folder of its own (`CLAUDE_CONFIG_DIR`, under `~/.atroxabot/claude/<id>/`),
  with its own sign-in, settings and history. The first account, "Claude", is the Claude Code setup you
  already have: whoever `claude` is signed in as in your terminal, with your skills, plugins and settings.
  An added account starts without those (install them in it by running `claude` with its
  `CLAUDE_CONFIG_DIR`); the skills in Settings, Skills reach every account without that. If you would rather have every account explicit, take "Claude" off the list in
  Settings; it can be put back there. AtroxaBot only tells Claude Code which folder to use; what is in it
  is Claude Code's.
- The account's email and plan shown in Settings, the model list, the commands and skills, and the plan's
  usage limits are what Claude Code itself reports.
- **The skill library** reaches Claude Code through its documented `--plugin-dir` flag, which loads a
  plugin for that session only: nothing is installed into, or read from, an account's folder.
- Besides `CLAUDE_CONFIG_DIR`, AtroxaBot sets what the official Agent SDK sets so that Claude Code reports
  when it runs a turn (`CLAUDE_CODE_SDK_READS_SESSION_STATE`). It does not set the variables that would say
  it is Anthropic's SDK (`CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_AGENT_SDK_VERSION`).
- An agent on a Claude account runs with **the agent's own system prompt**, not Claude Code's: who it is,
  your instructions, and a short section on how to work (act rather than describe, read before changing,
  check its work, be careful with what cannot be undone, ask with questions, delegate to subagents, use
  skills). Claude Code's prompt is written for its terminal and its own name; what an agent still needs
  from it is restated, the rest left out. Claude Code still adds your `CLAUDE.md` files, the list of
  skills and subagents, and the environment (date, system, folder) to the conversation by itself.
- AtroxaBot does not pay for, resell or intermediate Claude usage, and is not Claude Code: it is not built
  or endorsed by Anthropic.

If you distribute AtroxaBot to others, read Anthropic's [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance),
its [authentication page](https://code.claude.com/docs/en/authentication) and the
[Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) yourself: offering Claude Code
inside a product has conditions (the Commercial Terms, among them), and they can change.

The `crates/claude-agent` crate is the port of the Agent SDK's transport: one process per open session,
newline-delimited JSON on its standard input and output, permission prompts answered from the app. Every
message is sent as typed by a person (`origin`), and `--replay-user-messages` has Claude Code say when a
message sent during a turn is taken in (under the `uuid` it was sent with). How such a message is read (after
the running tool calls, or as a turn of its own right after; `queued_turn_count` in the result says so),
how an interrupt keeps or drops it (`cancel_queued`) and how `/clear` resets the context were checked
against Claude Code 2.1.322.

## Codex accounts

A Codex account is the Codex CLI you installed, used the way OpenAI documents for clients of its own:

- AtroxaBot **starts the Codex binary you installed, as published** (`codex app-server`, "the interface
  Codex uses to power rich clients", over its standard input and output), found on your `PATH` or in the
  usual install folders. An npm install's `codex.cmd` shim is never run: the native binary beside it is.
- AtroxaBot **never handles your sign-in**. *Sign in* opens Codex's own `codex login` in a terminal; the
  app never asks the app server to sign in, never reads or stores Codex's credentials, and refuses when
  Codex asks a client for tokens. Usage is billed to the account you sign in to.
- **Several accounts**: the first is whoever `codex` is signed in as in your terminal, with its settings
  (`~/.codex`). Each account you add is a Codex home of its own (`CODEX_HOME`, under
  `~/.atroxabot/codex/<id>/`), with its own sign-in, settings and conversations. AtroxaBot only tells
  Codex which folder to use.
- A session is a Codex thread, resumed by its id; the agent's instructions go in as the thread's developer
  instructions, on top of Codex's own; your `AGENTS.md` files are Codex's to read. The app's MCP servers
  and the skill library are given to Codex for the run (`-c mcp_servers.…`, `skills/extraRoots/set`):
  nothing is written to an account's own configuration.
- A message sent while Codex works is put into the running turn (`turn/steer`), where it reads it at its
  next step; one it does not take in is sent again as the next message. *Send now* stops the turn and
  sends it right after.

OpenAI's page on the app server says to contact OpenAI before shipping an integration for enterprise use,
and (as we read it) that signing users in through the app server is not for commercial or hosted services;
AtroxaBot does not sign anyone in. Read [OpenAI's page](https://developers.openai.com/codex/app-server)
yourself before distributing the app.

`crates/codex` is the transport. What it sends and reads was checked against Codex CLI 0.167.1 **signed
out** (the handshake, models, threads, resuming, a turn that fails for want of a sign-in, steering,
interrupting, compaction); replies, tool calls, approvals, questions, plans, usage, limits and sub-agents
are written from Codex's own protocol types and were not run against a signed-in account.

## Layout

| Crate | Purpose |
|---|---|
| `crates/claude-agent` (`atroxabot-claude-agent`) | Rust port of the Claude Agent SDK's transport and control protocol. Only dependency: `serde_json`. |
| `crates/codex` (`atroxabot-codex`) | Drives the installed Codex CLI through `codex app-server`: the handshake, threads and turns as a state machine without I/O (`machine.rs`), its messages (`parse.rs`), the process (`session.rs`), finding the binary (`cli.rs`). Only dependency: `serde_json`. |
| `crates/mcp` (`atroxabot-mcp`) | A small MCP client (stdio and Streamable HTTP): lists a server's tools and calls them. |
| `crates/core` (`atroxabot-core`) | Agents, sessions and their storage; the API client (Messages and Chat Completions); the agent loop and its tools; the Claude Agent and Codex engines (`claude.rs`, `codex.rs`); usage statistics (`stats.rs`); git through the `git` CLI (`git.rs`); the sessions' worktrees and their cleaning up (`worktree.rs`); the skill library and its repositories (`skills/`: `fetch.rs` gets a repository, `archive.rs` unpacks GitHub's `.tar.gz`, `scan.rs` reads what it offers). No GUI. |
| `crates/voice` (`atroxabot-voice`) | Fish Audio and ElevenLabs speech-to-text and text-to-speech, voice-activity detection, the call loop, and audio devices (`cpal`). |
| `crates/desktop` | The app (`atroxabot`): winit window, wgpu renderer, text engine, immediate-mode UI. |

Every engine reports a turn as the same `AgentEvent`s (`core/src/engine.rs`), and `Session::apply` turns
those into the saved transcript, so the app shows a Claude session, a Codex session and an API session the same way.

`crates/desktop/src`: `gpu.rs` + `shader.wgsl` (one instanced pipeline, one draw call per frame),
`text.rs`/`font.rs`/`raster.rs` (text), `markdown.rs` → `doc.rs` (laid-out Markdown), `paint.rs`/`ui.rs`
(drawing and widgets), `orb.rs` (the agents' faces), `logo.rs` (the app's own mark, three bodies in a line, and the platforms' marks, from the outlines in
`assets/logos/`), `tray.rs` (what closing the window does, and the Windows tray icon, through Win32 directly),
`shell/` (the screen: `sidebar.rs`, `thread.rs`, `composer.rs`, `queue.rs`, `limits.rs` (what a session
does when its account runs out), `question.rs`,
`menu.rs`, `panel.rs`, `picture.rs`, `spotlight.rs`, `stats.rs`, `agent_form.rs`, `settings.rs`, `skills.rs`,
`worktrees.rs`,
`call.rs`, `widgets.rs`), `app.rs` (events, threads, engines).

The app redraws only when something changes. Model requests, tools, the Claude Agent's process, calls and
file reads run on other threads and post results to the event loop; every file write goes through one
writer thread.

To look at the design without opening the app, draw every screen offscreen in every theme:
`ATROXABOT_SNAPSHOTS=<folder> cargo test -p atroxabot snapshots -- --ignored`.

## Storage

Everything lives in `~/.atroxabot/`:

- `config.json`: providers (with their API keys), voice providers (with theirs), MCP servers, recent
  folders, the theme, window and zoom, what closing the window does, whether the side panel is open, the order you dragged the
  sessions into, when unused worktrees are cleaned up, the repositories worktrees were made in, and what a
  session does when its account runs out.
- `usage.jsonl`: what every reply used (when, which agent, session, provider and model, its tokens, cost
  and tool calls), a line each. The statistics are counted from it; it is started once from the saved
  sessions and only added to after that. `limits.json`: each Claude account's plan limits as last
  reported.
- `icons/`: your own pictures of providers (see *Provider marks*). The app only reads it.
- `attachments/`: pictures you pasted into a message, one PNG each.
- `agents/<id>.json`: one file per agent. `memory/<id>/notes.md`: its notes.
- `library/`: the installed skills (`skills/<name>/`), laid out as a Claude Code plugin
  (`.claude-plugin/plugin.json`), and `library.json`: the repositories you look in and where each skill came
  from. A skill folder you put in `skills/` yourself works too. `cache/repositories/`: the last download
  of each repository looked at, read again on the next look.
- `worktrees/<repository>-<session id>/`: the git worktree of each session that works in one. The app
  makes and removes them; agents' file tools may work in them as in any folder.
- `codex/<id>/`: the Codex home of each extra Codex account. Codex owns what is in it. Removing an account
  in Settings leaves the folder, and so its sign-in, where it is.
- `sessions/<id>.json`: one file per session, with its folder (and its worktree, if it has one) and the model and effort chosen in it; `.index.json` next to them holds titles.
- `claude/<id>/`: the Claude Code configuration folder of each extra Claude account. Claude Code owns what
  is in it. Removing an account in Settings leaves the folder, and so its sign-in, where it is.
- `desktop.log`, `desktop-crash.log`, `desktop.lock`.

Files the app writes are written atomically, `0600` on Unix. An API key is only sent to the address it was
saved for.

## Known limits (deliberate, for now)

- Codex accounts: not run against a signed-in account yet (see *Codex accounts*). `/compact` is not sent
  to Codex (it summarises by itself), the context's breakdown and a reply's cost are not known for it,
  and its sign-in status is read when the app starts and when you come back to the settings.
- Worktrees: merged work is recognised from what your repository knows, so a pull request merged on the
  server counts once you have fetched or pulled (the app does not fetch); until then the unused-worktree
  rule cleans it up. What git ignores in a worktree (build output, a local `.env`) goes with it. A session
  that has started cannot move into or out of a worktree; start a new one.
- No agent-to-agent handoff and no scheduled jobs.
- API agents run their tool calls one after another and have no subagents (Claude accounts have both,
  from Claude Code). MCP servers behind OAuth are not supported for them: static headers only.
- A selection does not reach into reasoning or a tool's details.
- An agent's reply shows no pictures: Markdown images are not drawn, so only the pictures you send appear
  in the conversation (and open whole on a click). An opened picture is not zoomed past the window; a GIF
  is not decoded, so it shows as a placeholder.
- Git: the side panel shows the repository and switches branches; it does not commit, push, pull or
  create a branch (ask the agent). A diff's long lines scroll sideways (a sideways wheel, or Shift and
  the wheel) by columns of one width, so a long line of wide characters drifts.
- Statistics count by UTC day, and a reply saved before replies were dated counts on the day its session
  was last changed. What a session used before the usage log existed is only counted if the session was
  still there when the log was started; for those replies the provider is only known for Claude accounts.
- Files that are not pictures go to the agent by path: an API provider's agent asks before reading one
  outside its folder. A picture is read from where it is each time it is sent, so one that was moved or
  deleted later is only named. Pasting copied files works on Windows; elsewhere only a copied picture.
- Skills: a marketplace's plugins that come from npm, an archive or a command are not offered (only
  their GitHub and Git sources are), and only a plugin's skills are taken, not its commands, agents, hooks
  or MCP servers. A repository's `.json` address alone cannot be read; add the repository it is in.
  Repositories are not looked at by themselves: those your skills came from are read once when the Skills
  page first opens, to find updates.
- Questions, subagents, the task list, `/compact` and the plan's limits are Claude Code's, so they
  exist on Claude accounts only (skills work on every provider). A message sent while an agent works is read after the tool calls in
  progress: one long call (a test run, a subagent) keeps it waiting until it is done, unless you use
  *Send now*, which stops that call. A Claude Code older than about 2.1.219 cannot drop a message it was
  handed when you press Stop: it answers it after all. A turn's file changes cannot be undone (rewound) yet. A subagent's
  words show when Claude Code sends them: piece by piece if it streams them, but Claude Code 2.1.322 sent
  none of a subagent's text before its result when this was checked.
- A file named with `@` is read into the message by Claude Code on a Claude account; an API provider's
  agent gets its path and reads it with its own tools.
- Calls: no echo cancellation; one text-to-speech request at a time; the speech filter's two spoken
  phrases ("I've put the code on screen", "a link") are English.
- No complex-script shaping (Arabic joining, Indic reordering) and no bidi.
- An API key shows as stars while it is typed and is never shown again once saved; the headers and
  variables of an MCP server are shown as typed.
- An agent's picture is used from where it is on disk; move the file and the drawn face comes back.
- While the window is in the tray (or hidden on macOS) nothing says that an agent finished or waits for an
  approval: there are no system notifications yet. Linux has no tray icon (that needs a D-Bus client).
- Release builds are unsigned and there are no installers.

## License

Licensed under either of Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)) or MIT license
([`LICENSE-MIT`](LICENSE-MIT)) at your option. The renderer, text engine and widgets come from
[OpenRP](https://github.com/Luvarly/openrp), under the same licenses.

The bundled fonts keep their own licenses: Inter, Literata and JetBrains Mono (SIL Open Font License),
Twemoji Mozilla (Apache 2.0 code, CC-BY 4.0 graphics) and Lucide (ISC); see `crates/desktop/assets/`.
The outlines of the platforms' marks in `crates/desktop/assets/logos/` are from Simple Icons (CC0); the
marks themselves are trademarks of their owners.
