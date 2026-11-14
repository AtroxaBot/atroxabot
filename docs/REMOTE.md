# Remote control: design

Status: **proposal, nothing built.** For the owner to read, change and approve before any code.

The user starts a session on the desktop and follows and steers it from a phone: watch it work, reply,
approve, stop, start new sessions, change models and settings, and get a notification when an agent is
done or needs them.

Decided (2026-11-14):

- The phone reaches the desktop through a **relay on atroxabot.com**, end-to-end encrypted.
- The phone app is **our own Rust/wgpu app** (Android and iOS), sharing the desktop's interface code.
- The phone can do **everything** the desktop can, except what must happen at the desktop (below).
- **Notifications** when an agent needs the user and when a turn is done.

## 1. Principles

1. **The desktop is the only place anything runs.** Claude Code, Codex, API calls, tools, git, worktrees
   and files stay on the desktop. The phone is a second screen and keyboard. It never talks to Anthropic,
   OpenAI or any provider, and never holds an account's credentials or an API key.
2. **The desktop is the only writer.** Sessions, agents and settings are changed and saved by the
   desktop alone; the phone asks, the desktop does it and tells every screen what changed. No two copies
   of a session can be saved over each other.
3. **The relay sees nothing.** It passes sealed frames between a desktop and its phones. It cannot read
   or change them, and a stolen relay database gives no access to anyone's desktop.
4. **Pairing is the security boundary.** A paired phone can make agents run commands on the desktop.
   Pairing is deliberate, visible and undoable, and the phone can never raise an agent's autonomy past
   what the desktop allows.
5. **The project's rules still hold:** no async runtime, dependencies only when truly needed, the
   single-pipeline renderer, token discipline, `hide_emails`.

## 2. The pieces

```
 phone (Rust/wgpu)                  relay (Cloudflare)                 desktop (AtroxaBot)
 ┌──────────────────┐   WSS    ┌──────────────────────────┐   WSS    ┌──────────────────────┐
 │ shared UI + view │◄────────►│ Worker + Durable Object  │◄────────►│ Shell (the one writer)│
 │ mirror of state  │  sealed  │ one room per desktop:    │  sealed  │ remote host: pairing, │
 │ push token       │  frames  │ routes frames, sends     │  frames  │ sync, commands        │
 └──────────────────┘          │ pushes, holds no secrets │          └──────────────────────┘
        ▲                      └─────────────┬────────────┘
        └──────── APNs / FCM ◄───────────────┘  (no content, see 7)
```

| Piece | Where | New |
|---|---|---|
| `crates/remote` | The protocol: frames, commands, sync messages, the signed handshake and sealing (on `ring`). Shared by desktop and phone, no GUI. | crate |
| `crates/ui` | The interface code moved out of `crates/desktop`: `paint`, `text`, `atlas`, `gpu` + `shader.wgsl`, `ui`, `theme`, `orb`, `markdown`, `doc`, `highlight`, `shell/`. | crate (moved code) |
| `crates/desktop` | The window, the event loop, `App` (runs actions), and the remote host. | changes |
| `crates/mobile` | The phone app: Android activity and iOS entry, a phone layout of the shared UI, the remote client, push registration. | crate |
| `relay/` | The Cloudflare Worker and Durable Object, TypeScript, in this public repository so the encryption claims can be checked. Deployed to `relay.atroxabot.com`. | folder |

## 3. Commands and sync: the contract

Today the `Shell` owns the state and the logic, draws it, and hands `Action`s to `App` to carry out.
Remote control needs the user's intents to be explicit, so the phone can send the same ones.

**`Command`** (in `crates/remote`): every change the user can make, as data. For example:

- sessions: `Send { session, text, attachments }`, `Stop`, `Interrupt`, `FollowUp`, `Decide { ask,
  verdict, reason }`, `Answer { question, choices }`, `NewSession { agent }`, `Rename`, `Delete`,
  `SetModel`, `SetEffort`, `SwapProvider`, `SetWorktree`, `SetFolder`, `Compact`
- agents: `SaveAgent`, `DeleteAgent`
- settings: `SetConfig(field, value)` per setting, never the whole config
- reads: `Open { session }`, `LoadEarlier { session, before }`, `ListEntries { folder }`,
  `ListFiles { folder }`, `Search { text }`, `LoadStats`, `Git { folder }`, `Blob { hash }`

The desktop's own interface goes through the same `Shell::apply(Command)`. Local clicks and remote taps
take one path, so the two cannot drift apart, and every command gets a test once. (This is the shape of
adelite's `Command`/`Event` contract in `crates/chat`.)

**Desktop to phone:**

| Message | What |
|---|---|
| `Hello { protocol, app_version, desktop_name }` | First frame. A phone that does not speak `protocol` says "update the app" and goes no further. |
| `State { agents, sessions, connections, config }` | The sidebar's world: agents, `SessionSummary`s, accounts and settings **without secrets**: API keys are only "set" or "not set"; emails follow `hide_emails`. Resent whole when it changes (it is small). |
| `Session { session, entries, more }` | An opened session: its newest entries; `more` says older ones can be asked for. |
| `Event { session, seq, event }` | A live `AgentEvent` of an opened (or busy) session, numbered. |
| `Changed { session, summary }` | A session's summary changed (busy, title, model, effort). |
| `Result { id, outcome }` | What came of a command: done, or why not. |
| `Blob { hash, bytes }` | A picture or attachment the phone asked for. |

Large sessions are sent newest first, in pages, compressed (`flate2` is already in the tree). After a
reconnect the phone asks for `State` and its open session again rather than replaying missed events:
simpler, and correct by construction.

## 4. What stays on the desktop

| Stays | Why |
|---|---|
| Signing in to Claude or Codex, installing them | `CLAUDE.md`: started in a terminal **the user sees** at the desktop. The phone says "sign in on your desktop". |
| Looking for, downloading and installing updates | The update rules: the user's clicks in Settings. A restart from afar would also cut the connection. |
| Voice calls | The call loop uses the desktop's microphone and speakers. Calls on the phone (audio over the relay) are a later project. |
| Native file and folder dialogs | Instead, the phone browses the desktop's folders (`ListEntries`) and can send its own pictures and files along with a message. |
| Pairing a phone | Started from the desktop's settings (5). Removing a phone works from either. |

Typing a new API key on the phone is possible (sealed, the relay cannot read it) but is left out of the
first version: see open question 4.

## 5. Pairing

1. Settings, General, **Phones**: "Pair a phone". Only after that click the desktop shows a QR code.
2. The code holds: the relay's address, the desktop's room id, the desktop's public key, and a one-time
   secret. It is valid for **two minutes** and for **one phone**, then gone.
3. The phone scans it, makes its own keys, and sends its public key and name sealed under the one-time
   secret.
4. The desktop lists the phone under Phones at once ("Jerry's Pixel, paired just now") with a remove
   button. Removing it ends its connection and forgets its key; it cannot come back without a new QR.

The owner streams: a QR code on screen could be scanned by a viewer within its two minutes. Hence open
question 1: a yes/no on the desktop before a new phone is let in.

## 6. Encryption

Built from `ring`'s primitives (already in the tree for updates and TLS); nothing invented.

- **Keys.** The desktop and each phone have one long-lived **Ed25519** key pair: who they are. Stored in
  `~/.atroxabot/remote/` (`0600`); on the phone in its private app storage. (`ring` keeps no long-lived
  X25519 keys, so identity is Ed25519 and the key exchange below uses fresh X25519 keys only.)
- **Pairing** binds the two identities: the phone sends its Ed25519 public key with an HMAC under the
  QR code's one-time secret, and the desktop answers with an HMAC over both keys, so each knows the
  other's identity key and nobody else's could have been slipped in.
- **Each connection** runs a signed key exchange (the SIGMA pattern TLS 1.3 also follows): both sides send
  a fresh X25519 public key, each signs the whole handshake with its Ed25519 key, and HKDF-SHA256 turns
  the shared secret into two keys, one per direction. A key stolen later cannot read past traffic. After
  it, every frame is sealed with ChaCha20-Poly1305 with a counter as nonce; a replayed, reordered or
  altered frame is dropped and the connection restarts.
- **The relay** sees: room ids, device ids (hashes of public keys), frame sizes and times, push tokens.
  Nothing else.
- **To the relay** a device proves itself by signing a fresh challenge with the same Ed25519 key, so strangers
  cannot fill a room. The relay limits frames per second and bytes per day per room.

The parsers of frames and commands are hostile-input parsers (`CLAUDE.md`): bounded sizes, no panics,
fuzz-style tests.

## 7. Notifications

- The phone gives the relay its push token (APNs on iOS, FCM on Android) when it pairs and when the token
  changes.
- When a session **needs the user** (an approval or a question) or **finishes a turn**, the desktop sends
  the relay a `Notify` frame. The relay sends the push.
- The push says **no content**: "An agent needs you" / "An agent is done" (open question 2: the agent's
  name too?). The phone shows the details once opened and connected. Apple and Google learn only that a
  notification happened.
- A push is not sent while the phone has the app open (it already shows it), and at most one per session
  per minute.
- The relay holds the APNs key and the FCM service account: secrets of the project, not of users, kept in
  the Worker's secrets like `UPDATE_SIGNING_KEY` in GitHub's.

## 8. The relay

- A Cloudflare Worker at `relay.atroxabot.com` with one **Durable Object per desktop** (the room). Desktop
  and phones connect over WebSocket; the room passes each sealed frame to the other side.
- **Hibernating WebSockets**, so a room costs (almost) nothing while nobody types. A desktop left
  connected all day must not cost the project per hour.
- Nothing is stored but each room's device ids, public keys and push tokens. No history: a phone that is
  offline misses frames and asks for the state again when it connects.
- The relay is code in this repository, so anyone can read it, and could run their own (open question 6).

## 9. The connection in the apps (no async runtime)

The Rust side needs a WebSocket client: TLS through `rustls` (already in the tree through `ureq`) and the
WebSocket framing written by hand (a client needs the handshake, frames, masking, ping/pong and close: a
few hundred lines, with tests). The alternative is the `tungstenite` crate (synchronous, small); hand
written first, per the dependency rule, unless it proves fragile.

One thread holds the connection, reads frames and posts them to the event loop, like every other worker.
It reconnects with backoff, and only while the desktop has at least one paired phone and remote control
is on.

## 10. The phone app

- **One codebase** with the desktop's interface (`crates/ui`), a **phone layout**: the sidebar becomes the
  first screen (agents and sessions), a session fills the screen with the message box at the bottom, the
  side panel (tasks, subagents, usage, git) is a sheet pulled up from the bottom, menus become sheets.
  Same themes, same orbs, same drawing pipeline.
- **Touch:** drag to scroll (with the same easing as `widgets::Scroll`), long-press where the desktop
  right-clicks, safe areas, larger hit areas.
- **The on-screen keyboard and text input** are the biggest technical risk: how far `winit` takes text
  input, the keyboard's own suggestions and composition on Android and iOS is to be tried before
  anything else is built (step 3 below). If it falls short, a thin native text field (Kotlin/Swift) under
  our drawing is the fallback.
- **Platform glue** that cannot be Rust: push registration and the app's entry on each platform, as small
  Kotlin and Swift files.
- **Stores:** Android first (an APK on the owner's phone needs no store), then Google Play; iOS through
  TestFlight, then the App Store. Needs an Apple Developer account ($99/year) and a Google Play Console
  account ($25 once). iOS builds on CI's macOS runners.

## 11. On the desktop

- Settings, General, **Phones**: a switch for remote control, "Pair a phone", the list of phones with
  when each was last connected and a remove button.
- Off by default; while off, the app makes no connection to the relay at all.
- Open question 5: whether the title bar shows a small mark while a phone is connected.

## 12. Order of work

| Step | What | Rough size |
|---|---|---|
| 1 | This document, approved. | done when approved |
| 2 | `Command`: every user change in the desktop goes through `Shell::apply`. No visible change; tests. | 1 week |
| 3 | Text-input spike: a bare `winit` + `wgpu` window with our text editor on an Android phone and in the iOS simulator. Decides 10's fallback. | 2-3 days |
| 4 | `crates/remote`: protocol, the signed handshake on `ring`, framing, the WebSocket client; tests. | 1 week |
| 5 | `relay/`: Worker, Durable Object, device proof, limits; deployed. | 3-4 days |
| 6 | Desktop: Phones in Settings, pairing, the remote host (State, Session, Event, commands). | 1 week |
| 7 | `crates/ui`: the interface moved out of `crates/desktop`. | 1 week |
| 8 | Android app: phone layout, pairing by QR, watch, reply, approve, stop. On the owner's phone. | 2 weeks |
| 9 | Notifications (FCM first), then the rest of "everything" on the phone. | 1-2 weeks |
| 10 | iOS: build, APNs, TestFlight. Then the stores. | 2 weeks + review time |

About two to three months in all. Every step is usable or testable on its own, and steps 2, 3 and 4 can
run side by side.

## 13. Open questions for the owner

1. **Pairing confirmation.** Ask yes/no on the desktop before a new phone is let in? (Recommended,
   because a QR code on a stream could be scanned by a viewer.)
2. **Push text.** Generic ("An agent needs you"), or with the agent's name ("Scout needs you")? The name
   then passes through Apple or Google.
3. **The relay's code** in this public repository (`relay/`, recommended), or in the private website
   repository?
4. **API keys typed on the phone**: allow (sealed end to end), or only on the desktop?
5. **A mark on the desktop** while a phone is connected: yes or no?
6. **Your own relay**: a setting for another relay's address, for people who run their own? (Later.)
7. **Cost ceiling**: is the Workers paid plan (about $5 a month, Durable Objects included up to a limit)
   fine for the relay?
