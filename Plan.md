# Plan — P2P LAN Chat CLI (Rust + libp2p + mDNS)

## Goal

Build a single Rust binary for terminal chat that works **only on the local network**, with **no server**.
Peers find each other with **mDNS**, talk directly over libp2p, and keep a local SQLite outbox so
messages to offline peers wait until those peers come back.

The project is built incrementally in **3 phases**. Each phase leaves the app working; phase 2
replaces the plain CLI with a TUI, phase 3 adds persistence without changing the UI contract.

---

## Decisions confirmed with the user

| Topic | Decision |
| --- | --- |
| Rooms | One public `#general` room, direct messages, and **private invite-only groups** |
| Private groups | Creator creates a group nobody sees; adding a user sends a private invite; **any member can invite more users** (chain invites); no history backfill for new members; no leave/remove in v1 |
| Group message delivery | **One direct message per member** with a `group_id` field — no broadcast, no topic name exposed to non-members |
| Presence | Broadcast join/leave to all peers on the network (gossipsub `chat/presence`), so every peer's left panel updates |
| Colors | Each user gets a **stable color derived from their PeerId**, used in every channel; your own messages use one fixed color |
| `/users` UX | `/users` shows the list, ↑/↓ selects, Enter auto-fills `/users <name> `; user types the (possibly multiline) message and presses Enter |
| Identity | Private key stored in a file **in the application folder**, and the `PeerId` + username stored in the local database (DB is authoritative for display; file is authoritative for the key) |
| Offline messages | Never dropped. They stay in the outbox as **Pending**, and are flushed when the recipient broadcasts that it is online. UI shows ` (Pending)` until delivered |

### Important consequence of chain invites (design rule)

When B invites C to a private group, two things must happen:

1. C receives a private `GroupInvite` event and adds the group to their left panel.
2. **Every existing member (including A, who created the group) receives a `MemberAdded` event**,
   so their member lists converge. Otherwise A would keep fanning out messages only to B and C
   would never receive them.

Both events are sent through the reliable outbox (phase 3) / direct request-response (phases 1–2).

---

## Constraints and assumptions

1. **LAN only.** mDNS uses multicast UDP 5353 and requires the same L2 broadcast domain. It will not
   work over the internet, most VPNs, Docker bridge networks, or WiFi with client isolation.
   Show a clear "no peers found — are you on the same LAN?" hint in the UI.
2. **Toolchain.** Rust 1.97 is installed. Use libp2p `0.57.x` (MSRV 1.88) and `libp2p-mdns` `0.49.x`
   with the **tokio** provider; async-std support was removed from libp2p in 0.56.
3. **Identity.** A peer is uniquely a `PeerId`; the username is a label. Duplicate usernames are
   possible, so display `alice·12ab` (username + 4-char PeerId suffix) and resolve `/users` by prefix.
4. **Delivery semantics.** "Sent" means the recipient's libp2p swarm acked. It does not mean "read".
   No server-side store-and-forward exists; the local outbox is the only buffer.
5. **Privacy level of groups.** Group names and membership are known only to members and travel as
   Noise-encrypted direct messages. There is **no separate group encryption key** in v1: a malicious
   peer sniffing the LAN can still observe traffic. Document this honestly in the README.
6. One binary, phases evolve the same codebase. No feature-flag variants unless requested.
7. "Keep everything" retention: messages, group metadata and outbox rows are never pruned in v1.

---

## Technology choices

| Concern | Choice | Reason |
| --- | --- | --- |
| Async runtime | `tokio` | Required by current libp2p mDNS; integrates with crossterm events |
| Discovery | `libp2p-mdns` via `libp2p` feature `mdns` | LAN-only, zero configuration |
| Transport | TCP + DNS + Noise + Yamux | Encrypted, standard libp2p stack |
| DMs, invites, acks | `request-response` with `cbor` codec | Built-in ack maps directly onto the outbox |
| Presence | `gossipsub` topic `chat/presence` | Broadcast to all connected peers |
| Peer metadata | `identify` | Protocol negotiation / diagnostics |
| Wire types | `serde` structs | Cbor-encoded, versioned with `/p2p-chat/1.0.0` |
| TUI (phase 2) | `ratatui` + `crossterm`, `tui-textarea` | Multiline input and raw-mode keys |
| Database (phase 3) | `rusqlite` (bundled SQLite) | Single local file, no server |
| IDs / time | `uuid` v4, `chrono` (or `std::time`) | Idempotent retries, sortable history |
| Logging | `tracing` + `tracing-subscriber` to a **file**, never stdout during TUI mode | stdout is reserved for the UI |

`Cargo.toml` (approximate; exact versions pinned with `cargo add` during implementation):

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
libp2p = { version = "0.57", features = [
  "mdns", "tokio", "tcp", "dns", "noise", "yamux",
  "identify", "request-response", "gossipsub", "cbor"
] }
serde = { version = "1", features = ["derive"] }
uuid = { version = "1", features = ["v4"] }
chrono = "0.4"
tracing = "0.1"
tracing-subscriber = "0.3"
ratatui = "0.29"                             # phase 2
crossterm = "0.28"                           # phase 2
tui-textarea = "0.7"                         # phase 2
rusqlite = { version = "0.32", features = ["bundled"] } # phase 3

[dev-dependencies]
tempfile = "3"          # isolated DB / identity files in tests
```

---

## Architecture (stable across all phases)

```
                    ┌──────────────────────────────────────┐
                    │ main.rs  (wiring only)               │
                    │ - loads identity, asks username      │
                    │ - spawns network + worker tasks      │
                    │ - runs UI loop, routes events        │
                    └───────┬──────────────────┬───────────┘
                            │ mpsc             │ mpsc
              NetworkCommand│                  │NetworkEvent
              (Send, Shutdown)                 │(Peer*, Message, Ack)
                    ┌───────▼──────────┐       │
                    │ network/         │       │
                    │ Swarm owned here │───────┘
                    │ mdns, gossipsub, │
                    │ request-response │
                    └───────▲──────────┘
                            │ SendDirect { outbox_id, recipient, payload }
                    ┌───────┴──────────┐
                    │ worker/          │◄── WorkerCommand
                    │ outbox poller    │    (SaveOutgoing, DeliveryReport,
                    │ owns rusqlite    │     PeerOnline, LoadHistory, ...)
                    │ Connection       │
                    └───────┬──────────┘
                            │ ./chat.db
                    ┌───────▼──────────┐
                    │ database/        │
                    │ schema, queries  │
                    └──────────────────┘
```

**Key rules**

- The `Swarm` is created and polled in exactly one task. Nothing else polls it.
- The worker owns the only `rusqlite::Connection`; UI and network talk to it through channels with
  `oneshot` replies. No shared `Connection`, no SQL from the UI.
- The worker never touches the swarm; it emits `NetworkCommand`s and consumes `DeliveryReport`s.
- The UI never touches SQL or the swarm directly.
- The outbox stores **one row per (message, recipient)** so group fan-out can be acked individually.

### Module layout

```
src/
  main.rs                  # init + wiring, no business logic
  network/
    mod.rs                 # spawn_network(), NetworkCommand / NetworkEvent, peer map
    behaviour.rs           # ChatBehaviour: mdns + identify + request_response + gossipsub
    protocol.rs            # ChatRequest, ChatResponse, ChatMessage, GroupInvite, MemberAdded, Presence
    events.rs              # SwarmEvent / behaviour event -> NetworkEvent mapping
  ui/
    mod.rs                 # run_ui() entry point
    cli.rs                 # phase 1: plain terminal prompt + println notifications
    app.rs                 # phase 2: App state (peers, groups, conversations, input, selection)
    input.rs               # phase 2: crossterm keys, /users, /group commands, autofill
    render.rs              # phase 2: ratatui layout, per-user colors, Pending marker
  database/
    mod.rs                 # Db wrapper, open + migrate
    schema.sql             # tables + indexes
    models.rs              # Message, OutboxRow, PeerRow, GroupRow
    queries.rs             # prepared statements
  worker/
    mod.rs                 # spawn_worker(), outbox polling, retry policy
data/
  identity.key             # libp2p ed25519 private key, created at first run (chmod 0600)
  chat.db                  # phase 3
```

Phase 1 creates `network/` and `ui/cli.rs` plus the `data/` folder and identity handling; the rest is
added in their respective phase.

### Tests — every module owns its tests

Each module has its own tests: a `#[cfg(test)] mod tests` block at the bottom of its `.rs` file (or a
sibling `tests/` file for larger cases). Tests are written **in the same phase as the module**, not
deferred to a final pass. `make test` / `cargo test --all-targets` must be green after every phase.
Keep logic in small pure functions so it can be tested without real mDNS, sockets, or a terminal;
multi-process LAN checks stay documented as manual reproduction steps in the README.

| Module | Test scope |
| --- | --- |
| `main.rs` | Username validation, identity load-or-create against a `tempfile` directory, event/command routing helpers. Keep wiring thin so these helpers are testable |
| `network/` | Cbor round-trip of `ChatRequest`/`ChatResponse`, mDNS discovered-address dedupe, `PeerId -> username` resolution including duplicate-name suffixes, synthetic-event → `NetworkEvent` mapping, presence transitions |
| `ui/cli.rs` | Input parsing (`/quit`, empty lines), username display formatting, notification rendering from fixed fixtures |
| `ui/app.rs` / `input.rs` / `render.rs` (phase 2) | `/users` and `/group` command parsing, `/users` auto-fill, invite/`MemberAdded` state transitions, group isolation, stable color from a fixed PeerId, ` (Pending)` derivation, ratatui `TestBackend` snapshots for left/right alignment |
| `database/` | Migration is idempotent on an empty `tempfile` DB, insert/query round-trips for messages/outbox/groups/members, UUID dedupe, `online = 1` filtering query |
| `worker/` | Outbox claim → ack → `done`, failure/backoff progression, per-recipient fan-out rows for groups, `PeerOnline` immediate flush, offline rows never dequeued, shutdown |
| Integration (optional, later) | Spawn two app processes on the LAN and assert join/DM/group behaviour; keep these separate from `cargo test` so unit tests stay deterministic |

---

## Wire protocol (v1)

```rust
enum ChatRequest {
    Hello { username: String },
    Chat(ChatMessage),
    GroupInvite { group_id: String, group_name: String, invited_by: String },
    MemberAdded { group_id: String, peer_id: String, username: String, invited_by: String },
}

enum ChatResponse {
    HelloAck { username: String },
    Ack { outbox_id: Option<u64>, ok: bool, error: Option<String> },
}

struct ChatMessage {
    id: String,            // UUID, dedupe key
    from_peer: String,
    from_username: String,
    channel: String,       // "#general" | "dm:<peer_id>" | "<group_uuid>"
    body: String,
    created_at: i64,
}

struct Presence { peer_id: String, username: String, online: bool }
```

- Protocol string `/p2p-chat/1.0.0`; max request size ~1 MiB.
- `Ack` carries an `outbox_id` so delivery updates are exact even with group fan-out. It is `None`
  in phase 1 (no database yet) and `Some(id)` from phase 3.
- Incoming `ChatMessage.id` is deduplicated (`INSERT OR IGNORE` in phase 3, `HashSet` in phase 2).

---

## Phase 1 — Project init, identity, mDNS discovery, plain CLI

### Tasks

1. `cargo init` in `/app` (binary crate, e.g. `lan-chat`), add dependencies listed above.
2. **Identity persistence** (`main.rs` + small helper):
   - If `data/identity.key` exists, load it with `Keypair::from_protobuf_encoding`.
   - Otherwise generate `Keypair::generate_ed25519()`, write `to_protobuf_encoding()` to
     `data/identity.key`, set `0600` permissions on Unix.
   - Build the swarm with `SwarmBuilder::with_existing_identity(keypair)`.
   - Username is still asked interactively at every start (`Username: `); it is stored in the DB in
     phase 3. If the file and DB disagree later, the file wins for the key and the DB row is updated.
   - Accept `--data-dir DIR` (default `data`) so two instances on one laptop can use separate
     identity files; otherwise both would share a PeerId and discovery would skip them as "self".
3. `main.rs`: create mpsc channels, spawn the network task, call `ui::cli::run(...)` with a blocking
   stdin thread, and route `NetworkEvent`s to `ui::cli::handle_event` (`println!` / `eprintln!`).
4. `network/behaviour.rs`: compose `NetworkBehaviour` with
   `mdns::tokio::Behaviour::new(Config::default(), local_peer_id)`, `identify`,
   `request_response::cbor::Behaviour<ChatRequest, ChatResponse>` and `gossipsub`.
5. `network/events.rs`:
   - `mdns::Event::Discovered { peers }` → dial each `Multiaddr` (dedupe via `HashSet`), emit
     `PeerDiscovered`. `Expired` → emit `PeerExpired`.
   - `ConnectionEstablished` → send `Hello`; on `HelloAck` store `PeerId -> username` and emit
     `PeerConnected`. `ConnectionClosed` → emit `PeerDisconnected` and publish `Presence { online: false }`.
   - On connect publish `Presence { online: true }` on `chat/presence`; on receiving presence,
     update the peer map and emit `Presence`.
   - Inbound `ChatRequest::Chat` → emit `MessageReceived`, reply `Ack { ok: true }`.
6. `ui/cli.rs`: prompt loop; `/quit` exits; lines are sent to all connected peers (document this
   phase-1 syntax in README); print `➜ alice·12ab joined`, `➜ bob left`, `<alice> hello`.
7. **Tests for the phase-1 modules** (`main.rs` helpers, `network/`, `ui/cli.rs`) per the Tests table
   above; no real network or TTY required.
8. **Makefile** in `/app`, with at least `run`, `test`, and `build` targets:

   ```make
   .PHONY: build run test fmt clippy

   build:
   	cargo build

   run:
   	cargo run

   test:
   	cargo test --all-targets

   fmt:
   	cargo fmt --all

   clippy:
   	cargo clippy --all-targets -- -D warnings
   ```

   `build`, `run`, `test` are the required minimum; `fmt` and `clippy` are convenience extras that
   will be used by the final pass. Keep the target names stable in later phases (extend the commands
   only if needed).
9. `README.md`: build/run via `make`, "same LAN, UDP 5353 must be allowed", two-terminal demo,
   identity file note.

### Definition of done (phase 1)

- Two instances on the same LAN see join notifications without configuration.
- Restarting an instance keeps the same PeerId (identity file works).
- Typing a line in one terminal appears in the other with the sender's username.
- Ctrl+C exits cleanly; the identity file is reloaded on the next run.
- `make build`, `make test` and `make run` all work from `/app`.
- Unit tests exist for each phase-1 module and `make test` passes.

---

## Phase 2 — TUI, private groups, `/users` and `/group` commands

### Tasks

1. Add `ratatui`, `crossterm`, `tui-textarea`; switch active UI to `ui/app.rs` / `ui/input.rs` /
   `ui/render.rs`. Keep `ui/cli.rs` as a `--simple` fallback.
2. `main.rs`: after the username prompt, enable raw mode and `AlternateScreen`; use an RAII guard to
   restore the terminal on **every** exit path (errors included). Never log to stdout in TUI mode.
3. **State** (`ui/app.rs`):
   - `peers: HashMap<PeerId, Peer { username, online, color }>`.
     `color` is computed once from a stable hash of the PeerId (hue 0–360 → RGB), so it is identical
     in every channel. Your own messages use a fixed color (green).
   - `conversations: IndexMap<String, Vec<ChatLine>>` keyed by `#general`, `dm:<peer_id>`, or
     `<group_uuid>`.
   - `groups: Vec<Group { id, name, members: HashSet<PeerId> }>` — **only groups you are a member of**
     are stored/rendered. Non-members never receive any group event.
   - `active_conversation`, `user_list_visible`, `textarea`, `user_selection`, `group_selection`.
4. **Layout** (`ui/render.rs`):
   ```
   ┌──────────────┬───────────────────────────────────────────┐
   │ USERS        │  #general | alice·12ab | 🔒 team          │
   │ ● alice      ├───────────────────────────────────────────┤
   │ ○ bob        │  alice: hey                               │
   │              │              me: hi                       │
   │ CHANNELS     │  alice: multi-line                        │
   │ #general     │         message                           │
   │              ├───────────────────────────────────────────┤
   │ GROUPS       │  Type a message (Shift+Enter = newline)  │
   │ 🔒 team      │                                           │
   └──────────────┴───────────────────────────────────────────┘
   ```
   - Left column ~20–25%; right column = chat history (flex) + input (grows with line count).
   - Incoming message author name and text use that peer's stable color; your own messages are
     right-aligned in green. System notices are dim italic.
   - Delivery state suffix: ` (Pending)` while any recipient has not acked; no suffix once all
     acks arrive. Phase 2 tracks this in memory; phase 3 reads it from the outbox.
5. **Keys** (`ui/input.rs`):
   - `Enter` send, `Shift+Enter` newline, `↑`/`↓` select in the active list (users or groups),
     `Tab` autofill, `Ctrl+C` / `/quit` exit.
   - `/users` → print list, activate user selection; Enter inserts `/users <name> ` into the textarea.
   - `/users <name> <body…>` → resolve name → PeerId, send DM (multiline body allowed).
   - `/group create <name>` → create a private group (`uuid` id, creator as first member), add to
     left panel; nobody else is contacted.
   - `/group add <group> <username>` → resolve user, send `GroupInvite` **to that user** and
     `MemberAdded` **to every current member**; no history is sent.
   - `/group members <group>` → list members in the chat panel (useful for chain invites).
   - Selecting a group (or `#general`, or a user) sets the active conversation; plain Enter sends the
     typed body there. Sending to a group means one DM per member via the network task.
   - Unknown `/…` → short help text.
6. **Group event handling** in `main.rs` / `ui/app.rs`:
   - `GroupInvite` → validate sender is a member of that group (phase 3 checks DB), add group with
     members `{self, invited_by}`, show `➜ alice added you to private group 'team'`.
   - `MemberAdded` → if we are a member, add the new PeerId to that group; show a system notice.
   - Group `ChatMessage` → route by `channel = group_id`; ignore messages for unknown groups.
7. **Per-user colors** persist across channels because they derive from the persisted PeerId.

### Definition of done (phase 2)

- A creates private group `team`; B and C do **not** see it.
- A adds B → B gets the invite event, both list `team`; no one else learns about it.
- B adds C → C gets an invite, and **A also learns C joined** via `MemberAdded`; no history backfill.
- A message in `team` reaches A, B and C only; each sender's color is the same in `#general`, DMs,
  and `team`. Your own messages are always green.
- `/users` auto-fill + multiline DM works; `(Pending)` appears when a recipient is offline.
- Raw mode restored on exit.
- Unit tests cover every phase-2 module (`ui/app.rs`, `ui/input.rs`, `ui/render.rs`, group event
  handling and the new `network/` cases); `make test` passes.

---

## Phase 3 — Local SQLite database and outbox worker

### Tasks

1. Add `rusqlite` bundled; `database/mod.rs` opens `data/chat.db`, sets
   `PRAGMA journal_mode = WAL; foreign_keys = ON;`, and runs `schema.sql` at startup.
2. **Schema** (initial draft):

   ```sql
   CREATE TABLE peers (
     peer_id TEXT PRIMARY KEY,
     username TEXT NOT NULL,
     is_local INTEGER NOT NULL DEFAULT 0,
     online INTEGER NOT NULL DEFAULT 0,
     first_seen INTEGER NOT NULL,
     last_seen INTEGER NOT NULL
   );

   CREATE TABLE messages (
     id TEXT PRIMARY KEY,                 -- UUID, dedupe key
     channel TEXT NOT NULL,               -- '#general' | 'dm:<peer_id>' | '<group_uuid>'
     peer_id TEXT NOT NULL,               -- author for incoming, primary recipient for outgoing
     username_snapshot TEXT NOT NULL,
     direction TEXT NOT NULL CHECK (direction IN ('in','out')),
     body TEXT NOT NULL,
     created_at INTEGER NOT NULL
   );

   CREATE TABLE groups (
     id TEXT PRIMARY KEY,                 -- group UUID
     name TEXT NOT NULL,
     created_by TEXT NOT NULL
   );

   CREATE TABLE group_members (
     group_id TEXT NOT NULL REFERENCES groups(id),
     peer_id TEXT NOT NULL REFERENCES peers(peer_id),
     PRIMARY KEY (group_id, peer_id)
   );

   CREATE TABLE outbox (
     id INTEGER PRIMARY KEY AUTOINCREMENT,
     message_id TEXT REFERENCES messages(id), -- NULL for invite/member_added events
     recipient TEXT NOT NULL REFERENCES peers(peer_id),
     channel TEXT,
     kind TEXT NOT NULL CHECK (kind IN ('chat','group_invite','member_added')),
     payload TEXT NOT NULL,               -- serialized ChatRequest
     state TEXT NOT NULL DEFAULT 'pending'
       CHECK (state IN ('pending','in_flight','done','dead')),
     attempts INTEGER NOT NULL DEFAULT 0,
     next_attempt_at INTEGER NOT NULL,
     last_error TEXT,
     UNIQUE (message_id, recipient)
   );

   CREATE INDEX idx_outbox_due ON outbox(state, next_attempt_at);
   CREATE INDEX idx_outbox_message ON outbox(message_id);
   CREATE INDEX idx_messages_channel ON messages(channel, created_at);
   ```

   At startup, upsert the local row: `peer_id` from `data/identity.key`, `username` from the prompt,
   `is_local = 1`. The identity file is authoritative for the key; DB is authoritative for display.
3. **Write path (outbox pattern)**:
   1. UI submits → `main.rs` sends `WorkerCommand::SaveOutgoing { id, channel, body, recipients, kind }`.
   2. Worker transaction: insert one `messages` row + **one `outbox` row per recipient**, commit,
      reply via `oneshot` so the UI renders immediately as `(Pending)`.
      For `/group add`: insert/ensure `groups`, then one `group_invite` outbox row for the invitee and
      one `member_added` outbox row per existing member.
   3. Worker poll loop (~500 ms, `recv_timeout`): select `state='pending' AND next_attempt_at <= now`
      **joined with `peers.online = 1`** (offline rows are never dequeued), flip to `in_flight`, send
      `NetworkCommand::SendDirect { outbox_id, recipient, payload }`.
   4. Network replies `NetworkEvent::DeliveryReport { outbox_id, ok, error }`; `main.rs` forwards it
      to the worker.
   5. Worker sets `state='done'` on ack; on failure increments `attempts` and applies exponential
      backoff (1 s → 2 s → 4 s → … capped at 5 min). If a peer goes offline with in-flight rows,
      they are reset to `pending`.
   6. On `PeerConnected` / `Presence { online: true }`, `main.rs` updates `peers.online` via
      `WorkerCommand::PeerOnline { peer_id, username }` and triggers an immediate outbox poll.
      This is what flushes messages that waited while the peer was offline.
4. **Read path**: on startup the UI calls `LoadHistory` (conversations, peers, groups, group members)
   and fills its state; incoming `ChatMessage` rows are stored with `direction='in'` (no outbox row)
   via `INSERT OR IGNORE` on the UUID, then rendered. Invite/member events update `groups` /
   `group_members` and are also stored (as outbox rows on the sender side, as applied events on the
   receiver side — add an `events` table if audit of received group events is desired; optional).
5. Worker is a blocking thread (`spawn_blocking` + `tokio::sync::mpsc::Receiver::blocking_recv` with
   `recv_timeout`) so SQLite never blocks the async runtime. UI never opens a second connection.
6. UI derives `(Pending)` from `SELECT count(*) FROM outbox WHERE message_id = ? AND state <> 'done'`
   via the worker (cached in `App` and updated on `DeliveryReport`).
7. `README.md`: `data/chat.db`, retry/backoff policy, "offline = pending, connect = flush".

### Definition of done (phase 3)

- Send to an offline peer: message stays `Pending`, no ack, nothing is dropped.
- Peer comes online → presence event → all waiting outbox rows for that peer are flushed and the
  `(Pending)` marker disappears once acked.
- Restart: identity (same PeerId), username, groups, memberships, conversation history and pending
  outbox rows all survive.
- Private group membership converges after chain invites, including across restarts.
- Duplicate delivery of the same `message.id` never creates a duplicate chat line.
- Unit tests cover every phase-3 module (`database/`, `worker/`, plus updated `ui/` and `main.rs`
  routing): migration, outbox claim/ack/failure/backoff, offline filtering, group fan-out rows,
  dedupe, identity load/create; `make test` passes.

---

## Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| mDNS blocked (firewall/VPN/client isolation) | UDP 5353 check in README; "no peers found" hint; optional manual `/connect <multiaddr>` escape hatch |
| mDNS discovers but does not dial | Explicitly dial `Event::Discovered` addresses, dedupe, handle dial errors without panic |
| Duplicate usernames | Always show `name·abcd` suffix; resolve by prefix and list ambiguous matches |
| Group member lists diverge | `MemberAdded` broadcast to all existing members on every invite; store membership in DB; recipients validate inviter is a member |
| Group privacy is only transport-level | Document that there is no group encryption key in v1; consider a group key in a later version |
| Terminal left in raw mode | RAII guard calls `disable_raw_mode()` and `LeaveAlternateScreen` on all exit paths |
| SQLite blocking async runtime | Single worker thread owns the only connection; UI uses `oneshot` |
| Message lost if peer offline | Outbox rows are never dequeued while `online = 0`; retried on reconnect |
| Unbounded memory | Phase 2 keeps a bounded in-memory window (e.g. 1000 lines/conversation); full history lives in DB |
| Cbor mismatch between versions | Versioned protocol string; explicit structs; bump protocol version on breaking changes |

---

## Execution order

1. Phase 1: scaffold + identity file + discovery + plain CLI. Verify with two terminals and a restart.
2. Phase 2: TUI + presence + `/users` + private groups. Verify the A/B/C invite chain.
3. Phase 3: SQLite + outbox worker. Verify offline → online flush, restart persistence, dedupe.
4. Final pass: README, `cargo fmt`, `cargo clippy -- -D warnings`, tests.
