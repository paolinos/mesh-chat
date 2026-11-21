# mesh-chat

A serverless, peer-to-peer chat for the terminal. Peers find each other on the local network with **mDNS**, exchange usernames and messages over direct **Noise-encrypted libp2p** connections, and use gossipsub only for presence. There is no central server and no account to create.

History, group memberships and undelivered messages are kept in a local SQLite database, so messages to offline peers stay pending, survive restarts, and are retried with exponential backoff when those peers come back online.

> **Current scope:** peers must be on the same local network. mDNS uses multicast UDP 5353 and requires a shared L2 broadcast domain; VPNs, Docker bridge networks and Wi-Fi client isolation usually block it. Connecting to peers outside the local network is not implemented yet.

## Running it

Requirements:

- Rust 1.88+ (developed with 1.97).
- All peers on the same local network, with UDP 5353 multicast available.

```sh
make build   # cargo build
make test    # cargo test --all-targets
make run     # cargo run (full-screen TUI)
```

You will be asked for a username, then the full-screen UI opens. A real terminal is recommended because the UI uses the alternate screen and raw mode.

For a plain line-by-line terminal instead:

```sh
make run ARGS="--simple"
```

### Two instances on one laptop

Both processes need **different data directories**. Otherwise they load the same `data/identity.key`, share a PeerId, and ignore each other as "self".

```sh
# Terminal 1
make run ARGS="--data-dir data/alice"

# Terminal 2
make run ARGS="--data-dir data/bob"
```

Each directory gets its own `identity.key`, `chat.db` and `chat.log`. Peers should discover each other within a few seconds; type `/users` to see them. On two different laptops the default `make run` is enough.

### Using the TUI

- `Enter` sends the current input; `Shift+Enter` inserts a newline.
- `↑`/`↓` select a channel or group; `Enter` or `Tab` switches to it; `Esc` returns focus to the input. Typing any character also returns focus to the input.
- `/quit`, `/exit` or `Ctrl+C` exits and restores the terminal.

| Command | Effect |
| --- | --- |
| `/users` | Print known peers in the chat panel |
| `/users <name> <body…>` | Send a direct message; body may be multiline |
| `/group create <name>` | Create a private group; only you see it at first |
| `/group add <group> <name>` | Invite a user; any member can invite more users |
| `/group members <group>` | List current members of a group |
| `/help` | Show short command help |

Plain text goes to the active conversation: `#general` (everyone online), a direct-message thread, or a private group. Every chat line has a local `HH:MM` timestamp. A send to an offline recipient shows `(Pending)` until it is acknowledged; retries run at 1 s, 2 s, 4 s … capped at 5 minutes, and nothing is dropped.

Usernames are labels, so each peer is shown as `name·<last4 of PeerId>`. Connected peers and their messages are green, offline or unknown senders are gray, and your own messages are green.

### Files created at runtime

With the default `--data-dir data`:

- `data/identity.key` — ed25519 private key (mode `0600` on Unix). The PeerId stays the same across restarts; delete the file to get a new identity.
- `data/chat.db` — SQLite database (WAL mode) holding peers, messages, group memberships and outbox rows.
- `data/chat.log` — tracing output. Logs go to a file so the terminal stays clean. Set `RUST_LOG=debug` for verbose diagnostics.

## Architecture

```mermaid
flowchart LR
  Main["main.rs (wiring)"] --> UI["TUI / simple CLI"]
  Main --> Net["network/ — libp2p swarm"]
  Main --> Worker["worker — SQLite owner"]

  UI -->|"send, group commands"| Worker
  Worker -->|"history, Pending state"| UI
  Worker -->|"due outbox rows"| Net
  Net -->|"ACKs, presence, messages, group events"| Worker
  Net -->|"events"| UI

  Net -->|"mDNS discovery + Noise direct request/response + gossipsub presence"| Peers["Other LAN peers"]
  Peers -->|"same protocols"| Net

  Worker --> DB[("chat.db")]
```

`main.rs` wires three independent owners: the UI draws state, the worker is the only owner of the SQLite connection, and `network/` owns the swarm. The network task handles mDNS discovery, direct encrypted request/response for messages, invites and acknowledgements, and gossipsub only for presence. Every network event is mirrored to the worker and the UI, so presence can flush the outbox while the same event updates the chat panel.

The outbox stores one row per (message, recipient), so group fan-out is acknowledged individually. Rows are only dequeued for peers currently online; delivery reports mark them done or apply backoff. This is what lets offline messages survive restarts.

### Folder structure

- `src/main.rs` — CLI arguments, identity loading, logging and wiring between the UI, network and database worker.
- `src/network/`
  - `mod.rs` — swarm task, network commands/events, peer map and direct sends with ACK correlation.
  - `behaviour.rs` — mDNS, identify, request-response and gossipsub composition.
  - `events.rs` — swarm and behaviour events mapped to app-level network events.
  - `protocol.rs` — wire types for chat, presence, group invites and ACKs.
- `src/database/`
  - `mod.rs` — SQLite schema setup and the only connection to `chat.db`, owned by the worker.
  - `migration/0001_schema.sql` — table and index definitions.
- `src/worker.rs` — `Store` request/reply API, outbox poll loop, retry backoff and event mirroring.
- `src/ui/`
  - `mod.rs` — UI event loop, action dispatch and routing of network/worker events into app state.
  - `cli.rs` — plain line-by-line prompt (`--simple`), kept as a fallback.
  - `app.rs` — state: peers, groups, conversations, pending ack map, stable colors, history restoration and RAII terminal guard.
  - `input.rs` — key handling, command parser and channel navigation.
  - `render.rs` — ratatui layout and tests using `TestBackend`.
- `Cargo.toml` — crate metadata and dependencies; `Makefile` — `build`, `run`, `test` and convenience targets.

## What is implemented

- mDNS peer discovery with stable PeerIds persisted in an identity file.
- Noise-encrypted direct messages, one-to-one and `#general` broadcasts, with delivery acknowledgements.
- Full-screen terminal UI with multiline input, channels, direct messages, stable per-peer colors and `(Pending)` state; a plain `--simple` CLI is also available.
- Private invite-only groups with chain invites: any member can invite more members, and member lists converge across all existing members. Non-members never see group names or messages.
- Local SQLite persistence: history, peers, group memberships and an outbox with exponential-backoff retries and offline flushing on reconnect.
- Unit tests across all modules (`make test`).

## What is missing

- **Connections beyond the local network.** No WebRTC-style NAT traversal, manual `/connect <multiaddr>`, DHT discovery or relay support yet.
- **Group encryption keys.** Messages are Noise-encrypted between peers, but there is no separate group key; metadata such as membership and traffic patterns is visible to a peer sniffing the LAN. Groups are not a secure channel against local sniffing.
- **Group management.** No leave/remove commands, and new members get no history backfill.
- **Message search and pagination.** The TUI shows a bounded recent window (1000 lines per conversation); older history lives in SQLite but cannot be browsed yet.
- **Automated multi-process integration tests.** Multi-peer behavior is currently verified manually with the two-instance setup above.
