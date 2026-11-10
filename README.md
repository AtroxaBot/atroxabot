# AtroxaBot

A native, GPU-rendered desktop app for agents you chat with and call. You define agents; each has many
sessions, each session works in a folder of its own; and an agent thinks through one of your Claude
accounts, Anthropic's API, or any OpenAI-compatible gateway. It runs on Windows, macOS and Linux, and it
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
  message as pills (a click takes one off) and show in the conversation with it. A picture (PNG, JPEG,
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
  for, what it is doing now, and what it took) and as a tab over it: the tab is the subagent's own
  conversation, with what it was asked, what it said and every tool it called.
- **Tasks.** When an agent keeps a task list (Claude Code's `TaskCreate` and `TaskUpdate`, or `TodoWrite`),
  the side panel shows it: what is done, what it is doing now and what is still to do; while it works,
  the conversation names the task in hand. Claude Code gives a session an app starts those tools only
  when they are named as allowed (`--allowedTools`), so AtroxaBot names them.
- **Commands and skills.** Type `/` in the message box for the list: `/clear` starts a new session,
  `/compact` has a Claude account summarise its context, and every command and skill Claude Code offers
  (yours included) is there by name. Arrows move, Tab or Enter takes one.
- **Sessions.** Every agent has as many sessions as you like (Ctrl/Cmd+N starts one). A session shows the
  agent's answer as Markdown (code highlighted, with a copy button), its reasoning folded away, and every
  tool call as a row that opens to its input and output. Text can be selected with the mouse, across
  messages too, and copied.
  Sessions are saved as they go and reopen where they were. Click a session's title in the top bar to
  rename it: Enter or a click elsewhere keeps the new name, Esc the old one.
- **A folder per session.** The folder chip at the right of the top bar says where the session works;
  click it for the folders used lately, or to choose another. A new session starts in the folder the
  agent's last one worked in.
- **Providers**, chosen per agent and swapped from the chip in a session's top bar:
  - **Claude accounts**: the Claude Code you installed, driven with the Claude Agent SDK's protocol, with
    its own tools, permissions, subagents, skills and MCP servers. Several accounts (work, personal) are
    kept apart; see *Claude accounts and Anthropic's terms*.
  Click a provider's name in the settings to rename it, the first Claude account included.
  - **Claude API**: Anthropic's Messages API with your API key.
  - **OpenAI-compatible**: OpenRouter, Inworld, OpenAI, Ollama, or any other Chat Completions gateway.
  With an API provider the app runs the agent loop itself, with its own tools: `read`, `write`, `edit`,
  `list`, `search`, `shell` and `fetch` (and, on the Claude API, Anthropic's web search and fetch).
  Swapping provider mid-session works: a Claude account that never saw the conversation is handed what
  was said so far.
- **Models from the provider.** The model menu at the right under the message box lists what the provider itself
  offers: a Claude account's list comes from Claude Code (so it is always current; of a family listed in
  several versions only the newest is shown), an API provider's from
  its `/models`. The provider's default is the one in use until you pick another; any model id can be
  typed. The same menu sets the effort, which is always one of the model's own levels: Opus, Haiku and
  Fable start at medium, Sonnet at high, and picking another model starts at that model's level. A model
  that takes no effort shows none.
- **Autonomy**, set in the agent's editor: *Ask first*, *Edit freely*, *Auto* (what a new agent starts
  with: Claude Code's auto mode, with a Claude account), *Full autonomy* or *Read only*. A call that needs approval waits on a card above
  the message box: Enter allows it, Esc denies it, *Always allow* stops asking for the session. The app's
  own data folder, which holds API keys, is off limits to every agent's file tools.
- **MCP servers** (Settings, MCP servers): local commands or remote addresses whose tools every agent
  gets. Claude accounts also keep the servers you set up in Claude Code itself.
- **Spotlight.** Ctrl/Cmd+K (or the magnifier over the agents) opens one field over everything:
  it finds sessions by title at once and by what was said in them a moment later, agents by name, and
  things to do (a new session, a new agent, the statistics, the settings, the side panel). Arrows move,
  Enter opens, Esc closes.
- **The side panel** (the last icon of the top bar) shows what the session used and how full its context
  is, and the git repository of its folder: the branch (a click switches it), how far it is from what it
  tracks, the changed files with their added and removed lines (a click opens the diff), a message box
  that commits everything changed, and Push. It uses the `git` you have installed and reads the
  repository again when a turn ends and when you come back to the window. For a Claude account it also
  shows where the plan's limits stand (the 5-hour and weekly windows, the week's limit for Fable once a
  turn ran on it, and when each starts over), under the name and address of the account they belong to.
  They are what Claude Code reports for that account with each turn, the same numbers its own `/usage`
  prints; reaching a limit is said in a notice.
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
- **Looks.** The app draws its own title bar, the sidebar sits on the window and the session floats beside
  it on a rounded canvas, with the side panel on one of its own; the agent editor, the settings, the
  statistics, the spotlight and a file's diff open over them as modals. Three themes: Black
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
  `CLAUDE_CONFIG_DIR`). If you would rather have every account explicit, take "Claude" off the list in
  Settings; it can be put back there. AtroxaBot only tells Claude Code which folder to use; what is in it
  is Claude Code's.
- The account's email and plan shown in Settings, the model list, the commands and skills, and the plan's
  usage limits are what Claude Code itself reports.
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
newline-delimited JSON on its standard input and output, permission prompts answered from the app.

## Layout

| Crate | Purpose |
|---|---|
| `crates/claude-agent` (`atroxabot-claude-agent`) | Rust port of the Claude Agent SDK's transport and control protocol. Only dependency: `serde_json`. |
| `crates/mcp` (`atroxabot-mcp`) | A small MCP client (stdio and Streamable HTTP): lists a server's tools and calls them. |
| `crates/core` (`atroxabot-core`) | Agents, sessions and their storage; the API client (Messages and Chat Completions); the agent loop and its tools; the Claude Agent engine; usage statistics (`stats.rs`) and git through the `git` CLI (`git.rs`). No GUI. |
| `crates/voice` (`atroxabot-voice`) | Fish Audio and ElevenLabs speech-to-text and text-to-speech, voice-activity detection, the call loop, and audio devices (`cpal`). |
| `crates/desktop` | The app (`atroxabot`): winit window, wgpu renderer, text engine, immediate-mode UI. |

Both engines report a turn as the same `AgentEvent`s (`core/src/engine.rs`), and `Session::apply` turns
those into the saved transcript, so the app shows a Claude session and an API session the same way.

`crates/desktop/src`: `gpu.rs` + `shader.wgsl` (one instanced pipeline, one draw call per frame),
`text.rs`/`font.rs`/`raster.rs` (text), `markdown.rs` → `doc.rs` (laid-out Markdown), `paint.rs`/`ui.rs`
(drawing and widgets), `orb.rs` (the agents' faces), `logo.rs` (the app's own mark, three bodies in a line, and the platforms' marks, from the outlines in
`assets/logos/`), `shell/` (the screen: `sidebar.rs`, `thread.rs`, `composer.rs`, `question.rs`,
`menu.rs`, `panel.rs`, `spotlight.rs`, `stats.rs`, `agent_form.rs`, `settings.rs`, `call.rs`,
`widgets.rs`), `app.rs` (events, threads, engines).

The app redraws only when something changes. Model requests, tools, the Claude Agent's process, calls and
file reads run on other threads and post results to the event loop; every file write goes through one
writer thread.

To look at the design without opening the app, draw every screen offscreen in every theme:
`ATROXABOT_SNAPSHOTS=<folder> cargo test -p atroxabot snapshots -- --ignored`.

## Storage

Everything lives in `~/.atroxabot/`:

- `config.json`: providers (with their API keys), voice providers (with theirs), MCP servers, recent
  folders, the theme, window and zoom, and whether the side panel is open.
- `usage.jsonl`: what every reply used (when, which agent, session, provider and model, its tokens, cost
  and tool calls), a line each. The statistics are counted from it; it is started once from the saved
  sessions and only added to after that. `limits.json`: each Claude account's plan limits as last
  reported.
- `icons/`: your own pictures of providers (see *Provider marks*). The app only reads it.
- `attachments/`: pictures you pasted into a message, one PNG each.
- `agents/<id>.json`: one file per agent. `memory/<id>/notes.md`: its notes.
- `sessions/<id>.json`: one file per session, with its folder; `.index.json` next to them holds titles.
- `claude/<id>/`: the Claude Code configuration folder of each extra Claude account. Claude Code owns what
  is in it. Removing an account in Settings leaves the folder, and so its sign-in, where it is.
- `desktop.log`, `desktop-crash.log`, `desktop.lock`.

Files the app writes are written atomically, `0600` on Unix. An API key is only sent to the address it was
saved for.

## Known limits (deliberate, for now)

- Codex is not a provider yet.
- No agent-to-agent handoff and no scheduled jobs.
- API agents run their tool calls one after another and have no subagents (Claude accounts have both,
  from Claude Code). MCP servers behind OAuth are not supported for them: static headers only.
- A selection does not reach into reasoning or a tool's details.
- Git: a commit takes everything changed (there is no staging of single files), branches are switched but
  not created, and there is no pull. A diff's long lines scroll sideways (a sideways wheel, or Shift and
  the wheel) by columns of one width, so a long line of wide characters drifts.
- Statistics count by UTC day, and a reply saved before replies were dated counts on the day its session
  was last changed. What a session used before the usage log existed is only counted if the session was
  still there when the log was started; for those replies the provider is only known for Claude accounts.
- Files that are not pictures go to the agent by path: an API provider's agent asks before reading one
  outside its folder. A picture is read from where it is each time it is sent, so one that was moved or
  deleted later is only named. Pasting copied files works on Windows; elsewhere only a copied picture.
- Questions, subagents, skills, the task list, `/compact` and the plan's limits are Claude Code's, so they
  exist on Claude accounts only. A turn's file changes cannot be undone (rewound) yet. A subagent's
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
- Release builds are unsigned and there are no installers.

## License

Licensed under either of Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)) or MIT license
([`LICENSE-MIT`](LICENSE-MIT)) at your option. The renderer, text engine and widgets come from
[OpenRP](https://github.com/Luvarly/openrp), under the same licenses.

The bundled fonts keep their own licenses: Inter, Literata and JetBrains Mono (SIL Open Font License),
Twemoji Mozilla (Apache 2.0 code, CC-BY 4.0 graphics) and Lucide (ISC); see `crates/desktop/assets/`.
The outlines of the platforms' marks in `crates/desktop/assets/logos/` are from Simple Icons (CC0); the
marks themselves are trademarks of their owners.
