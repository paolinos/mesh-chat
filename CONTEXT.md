# mesh-chat — Full Project Context

> A serverless, peer-to-peer chat CLI for the terminal. Built with Rust, libp2p, SQLite and ratatui.

---

## 1. Overview

mesh-chat lets peers chat on the same local network without any central server.

- **Discovery:** mDNS (UDP 5353) discovers peers on the LAN.
- **Transport:** libp2p with Noise encryption over TCP.
- **Presence:** gossipsub broadcasts join/leave announcements.
- **Messaging:** direct request/response for chat, group invites, member updates and ACKs.
- **Persistence:** local SQLite database with an outbox pattern for offline resilience.
- **UI:** full-screen ratatui TUI plus a `--simple` line-by-line CLI fallback.
- **File sharing:** encrypted direct file transfer in DMs, stored under `data/downloads/`.

---

## 2. High-level architecture

Three independent owners wired by `main.rs`:

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│     UI      │────▶│    Worker    │────▶│   Network   │
│  (ratatui)  │◀────│  (SQLite +   │◀────│ (libp2p     │
│  (--simple) │     │   outbox)    │     │   swarm)    │
└─────────────┘     └──────────────┘     └─────────────┘
         │                   │                   │
         ▼                   ▼                   ▼
   chat.db            message_attachments    encrypted direct
   downloads/          outbox rows            requests to peers
```

### Key rules

- The **Worker** owns the only `rusqlite::Connection`.
- The **Network** task owns and polls the only libp2p `Swarm`.
- The **UI** never touches SQL or the swarm directly; it talks through the Worker's channel API.
- Network events are mirrored to both the Worker and the UI so presence flushes the outbox while the same event updates the screen.

---

## 3. Module-by-module breakdown

### `src/main.rs`

Entry point and wiring only.

- Parses CLI args (`--data-dir`, `--simple`).
- Loads or creates `data/identity.key` (ed25519 keypair, mode `0600`).
- Initializes logging to `data/chat.log`.
- Spawns network task, worker task, and runs the chosen UI.
- Shuts everything down cleanly on exit.

### `src/network/`

#### `protocol.rs`

Wire types (serde, JSON over CBOR request-response):

- `ChatRequest` — `Hello`, `Chat(ChatMessage)`, `GroupInvite`, `MemberAdded`.
- `ChatResponse` — `HelloAck`, `Ack { outbox_id, ok, error }`.
- `ChatMessage` — `id`, `from_peer`, `from_username`, `channel`, `body`, `created_at`, `outbox_id`, `attachments`.
- `Attachment` — `filename`, `bytes`, `mime_type`.
- `Presence` — gossipsub announcement of online/offline state.

#### `behaviour.rs`

Composes the libp2p behaviour:

- mDNS discovery
- identify
- request-response (CBOR codec)
- gossipsub for presence

#### `events.rs`

Swarm-event helpers:

- `discovered_peers` deduplicates mDNS addresses and skips self.
- `should_initiate_dial` picks one initiator to avoid simultaneous dials.
- `display_name` formats `username·<last4 of PeerId>`.

#### `mod.rs`

Spawns the swarm task and runs the event loop.

- `NetworkCommand` — `SendChat`, `SendDirect`, `Shutdown`.
- `NetworkEvent` — peer discovery, presence, messages, ACKs, group events, errors.
- Correlates outbound requests with ACKs/outbox IDs.
- Sends/receives direct requests over Noise.

### `src/database/`

#### `mod.rs`

Single SQLite connection wrapper.

- Opens `data/chat.db`, enables WAL and foreign keys.
- Runs migrations from `src/database/migration/`.

#### Migrations

- `0001_schema.sql` — peers, messages, groups, group_members, outbox.
- `0002_unread.sql` — unread_conversations.
- `0003_attachments.sql` — message_attachments.

#### Core structs

- `StoredPeer`, `StoredMessage`, `StoredAttachment`, `StoredGroup`, `UnreadConversation`.
- `History` — aggregate loaded on startup.
- `OutboxRow` — one row per (message, recipient).

#### Key methods

- `upsert_local_peer`, `set_peer_online`
- `insert_incoming_message` — stores incoming message + attachments.
- `save_outgoing` — inserts message, attachment metadata, and outbox rows.
- `claim_due` — atomically claims pending rows for online peers.
- `ack` / `fail` — update outbox state and apply exponential backoff.
- `history` — load peers, messages (with attachments), groups, pending IDs and unread markers.

### `src/worker.rs`

`Store` is the async API; the worker loop runs on a blocking thread.

- `WorkerCommand` — `LoadHistory`, `SaveOutgoing`, `CreateGroup`, `QueueGroupInvite`, `IncomingMessage`, `MarkConversationRead`, `PeerOnline/Offline`, group events, `DeliveryReport`, `Shutdown`.
- `WorkerEvent` — `PendingChanged`, `DeliveryFailed`, `AttachmentsStored`.

#### Outbox pattern

1. UI asks worker to save outgoing message.
2. Worker inserts `messages` + one `outbox` row per recipient.
3. Worker polls every 500 ms; claims due rows for online peers.
4. Worker sends `NetworkCommand::SendDirect`.
5. Network reports ACK/failure via `DeliveryReport`.
6. Worker marks row `done` or reschedules with backoff (`1s, 2s, 4s … 5min`).

#### File handling

- Outgoing: reads source files, enforces 15 MB total limit, stores sender's absolute source path.
- Incoming: writes attachments to `data/downloads/<YYYYMMDD>_<username>_<filename>`, deduplicates names, emits `AttachmentsStored` event so the UI can replace placeholder paths.

### `src/files.rs`

File-system helpers:

- `downloads_dir` / `ensure_downloads_dir`
- `store_incoming_file` — flat naming, sanitization, deduplication, `0600` permissions on Unix.
- `open_with_os` — cross-platform `open`/`xdg-open`/`start`.
- `MAX_ATTACHMENT_BYTES` — 15 MB.

### `src/ui/`

#### `app.rs`

Application state for the TUI.

- `App` — peers, conversations, groups, selection state, textarea, pending acks, pending attachments, attachment hit areas.
- `ChatLine` — `Message { id, author, peer, body, created_at, mine, attachments }` or `System(String)`.
- `AttachmentView` — filename + display path + open path.
- `AttachmentHitArea` — screen rectangle + path for mouse clicks.
- `make_message` builds a `ChatMessage`.
- `record_incoming` / `record_outgoing` update in-memory conversations.
- `apply_history` restores state from SQLite on startup.
- `TerminalGuard` enables raw mode, alternate screen and mouse capture; restores on drop.

#### `input.rs`

Key handling and command parser.

- Commands: `/users`, `/file`, `/attach`, `/group create/add/members`, `/help`, `/quit`/`/exit`.
- `Action` enum maps commands to UI actions handled by `ui/mod.rs`.
- `Shift+Enter` inserts newlines; `Enter` sends or activates selection.
- Arrow keys navigate channels/groups; `Tab` completes; `Esc` dismisses.

#### `complete.rs`

Autocomplete engine.

- Completes commands, `/group` subcommands, usernames, group names, and **file paths**.
- `path_values` expands `~`, lists directory entries, distinguishes files from directories.
- Directory completions end with `/` and do not add a trailing space.

#### `render.rs`

ratatui layout.

- Left panel: CHANNELS and GROUPS.
- Right panel: header tabs, chat history, multiline input.
- Renders messages with per-sender colors, `(Pending)` suffix, system notices.
- Renders attachments as `📎 filename → path` with clickable hit areas.
- Shows completion hints and pending-attachment hints above the input.

#### `mod.rs`

TUI event loop.

- Receives `NetworkEvent`s and `WorkerEvent`s.
- Routes key/mouse events.
- `handle_action` executes `Action`s: send chat/DM, attach file, create group, etc.
- `send_message` transfers queued attachments to the worker and renders the outgoing line.
- Mouse down on an attachment spawns `files::open_with_os`.

#### `cli.rs`

Plain `--simple` CLI.

- Prompt loop reading stdin lines.
- Prints peer join/leave, messages, errors.
- Prints attachments as `[📎 filename]`.
- Does not support sending files or loading history.

---

## 4. Data flows

### Sending a chat message

1. UI `handle_key(Enter)` → `Action::SendChat(body)`.
2. `send_message` creates `ChatMessage`, records outgoing line, calls `store.save_outgoing(message, recipients, [])`.
3. Worker inserts `messages` + outbox rows.
4. Worker polls, claims due rows for online peers.
5. Worker sends `NetworkCommand::SendDirect` with `ChatRequest::Chat`.
6. Network sends encrypted request and receives `Ack`.
7. Network emits `DeliveryReport` → Worker marks row `done`.
8. Worker emits `PendingChanged` → UI removes `(Pending)`.

### Sending a file

1. User types `/file <path>` → `Action::AttachFile` queues path in `app.pending_attachments`.
2. User types caption and presses `Enter`.
3. `send_message` takes queued paths, builds `AttachmentView`s, calls `store.save_outgoing(message, recipients, attachment_paths)`.
4. Worker reads files, creates `Attachment` bytes, enforces 15 MB limit, stores sender metadata.
5. The outbox payload carries the encrypted `ChatMessage` with attachments.
6. Receiver's network layer gets the message, emits `MessageReceived`.
7. UI renders placeholder; Worker writes files to `data/downloads/`.
8. Worker emits `AttachmentsStored` → UI updates paths.

### Receiving a message

1. Network receives `ChatRequest::Chat`.
2. Emits `NetworkEvent::MessageReceived` to UI and Worker.
3. UI renders message immediately.
4. Worker stores message in `chat.db` (and attachments on disk).
5. If conversation not open, unread marker is written.

---

## 5. Database schema summary

```sql
peers(peer_id, username, is_local, online, first_seen, last_seen)
messages(id, channel, peer_id, username_snapshot, direction, body, created_at)
groups(id, name, created_by)
group_members(group_id, peer_id)
outbox(id, message_id, recipient, channel, kind, payload, state, attempts, next_attempt_at, last_error)
unread_conversations(channel, first_unread_at, last_unread_at)
message_attachments(id, message_id, channel, filename, local_path, size_bytes, is_sender, created_at)
```

- `outbox.state` ∈ `{pending, in_flight, done, dead}`.
- `outbox` has one row per recipient so group fan-out is acked individually.
- `message_attachments.is_sender` distinguishes sender absolute path from receiver download path.

---

## 6. Wire protocol

- Protocol string: `/p2p-chat/1.0.0`
- Presence topic: `chat/presence`
- Codec: request-response over CBOR, payloads are JSON-serializable serde structs.
- `ChatMessage.attachments` and `outbox_id` use `#[serde(default)]` for backward compatibility.

---

## 7. Security notes

- **Transport:** libp2p Noise encrypts messages and files in transit. A passive LAN sniffer sees encrypted bytes, not content.
- **mDNS/presence:** peer usernames and online status are broadcast in plaintext via gossipsub.
- **Metadata:** an observer can see which peers talk, how often, and message sizes.
- **Authentication:** based on libp2p PeerId. A new peer can join the LAN; there is no central whitelist.
- **At-rest:** `chat.db`, `identity.key` and `downloads/` are stored unencrypted.
- **Group privacy:** groups are invite-only and messages are sent as individual DMs, but there is no group encryption key.

---

## 8. Build, test and run

```sh
make build    # cargo build
make test     # cargo test --all-targets
make run      # cargo run (TUI)

# Two instances on one laptop
make run ARGS="--data-dir data/alice"
make run ARGS="--data-dir data/bob"

# Plain CLI
make run ARGS="--simple"
```

---

## 9. Conventions

- Each module owns its `#[cfg(test)] mod tests` block.
- Prefer pure functions; avoid real network, real terminal and real disk in unit tests.
- The Worker is the only SQLite owner.
- The Network task is the only Swarm poller.
- Keep UI logic free of SQL and swarm code.
- Use `tracing` for diagnostics; logs go to `data/chat.log`, not stdout in TUI mode.

---

## 10. Current feature status

Implemented:
- mDNS peer discovery
- Noise-encrypted direct messages and `#general` broadcasts
- Private invite-only groups with chain invites
- Offline outbox with exponential backoff
- Full TUI with channels, groups, colors, pending markers, unread bullets
- File sharing in DMs with `/file`/`/attach`, path autocompletion, click-to-open
- `--simple` CLI fallback

Not implemented:
- Connections beyond the LAN
- End-to-end message encryption separate from transport encryption
- Group encryption keys
- Leave/remove group members
- Message search/pagination
- At-rest database encryption
