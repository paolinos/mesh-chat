# lan-chat (Phase 2)

A serverless, peer-to-peer LAN chat written in Rust. Peers discover each other with **mDNS**,
exchange usernames and messages over **libp2p** (Noise-encrypted direct request/response), and use
gossipsub only to broadcast presence. No central server is involved.

Phase 2 adds a full-screen ratatui UI, multiline input, `/users` autofill, private invite-only
groups, and in-memory `(Pending)` delivery state. Phase 3 will add a local SQLite outbox so pending
messages survive restarts and are retried when peers come back online.

## Requirements

- Rust 1.88+ (developed with 1.97).
- All peers must be on the **same local network**. mDNS uses multicast UDP port 5353; VPNs, Docker
  bridge networks, and WiFi client isolation usually block it.

## Quick start

```sh
make build   # cargo build
make test    # cargo test --all-targets
make run     # cargo run (full-screen TUI)
```

You will be asked for a username, then the full-screen UI opens. A minimal terminal can be enough,
but a real terminal is recommended because the UI uses alternate screen and raw mode.

```sh
make run ARGS="--simple"   # plain line-by-line CLI (phase 1 fallback)
```

### Testing two instances on one laptop

Both terminals must use **different data directories**. If you just run `make run` twice in the same
folder, both processes load the same `data/identity.key` and therefore have the same PeerId; the app
ignores its own PeerId, so you will see no peers. Give each instance its own directory:

```sh
# Terminal 1
make run ARGS="--data-dir data/alice"

# Terminal 2
make run ARGS="--data-dir data/bob"
```

(Equivalent: `cargo run -- --data-dir data/alice`.) Each directory gets its own `identity.key` and
`chat.log`. Both instances should discover each other within a few seconds; type `/users` to see the
known peers. On two different laptops, the default `make run` is enough.

## TUI layout and keys

```
┌──────────────┬───────────────────────────────────────────┐
│ CHANNELS     │  #general | DM alice·12ab | 🔒 team       │
│ #general     ├───────────────────────────────────────────┤
│ DM alice·12ab│  alice·12ab: hey                          │
│              │                       me: hi              │
│              │  alice·12ab: multi-line                   │
│              │                   message                 │
│ GROUPS       ├───────────────────────────────────────────┤
│ 🔒 team      │  Message (Shift+Enter = newline)          │
└──────────────┴───────────────────────────────────────────┘
```

- `Enter` sends the current input. `Shift+Enter` inserts a newline.
- `↑`/`↓` select a channel or group in the left panel.
- `Enter` or `Tab` switches to the highlighted channel/group; `Esc` returns focus to the input.
- `/quit`, `/exit`, or `Ctrl+C` exits and always restores the terminal.
- There is no USERS panel: `/users` prints the known peers into the chat panel. DMs appear directly
  in CHANNELS as `DM name·suffix` as soon as a peer connects (presence/connection event), before any
  message is exchanged.

Because usernames are only labels, every peer is shown as `name·<last4 of PeerId>` (PeerIds all start
with `12D3`, so the suffix is the distinguishing part). Names are resolved by prefix in commands.
Connected peers are shown in **green** in their DM channel label, and their broadcast/DM messages
are green too; offline or unknown senders are gray. Any incoming message (including a `#general`
broadcast) registers the sender as online, so a just-connected peer appears green immediately.
Messages from an unknown sender create the canonical `dm:<peer-id>` thread and learn that user's
name from the message, rather than opening an extra "unknown" channel.

## Commands

| Command | Effect |
| --- | --- |
| `/users` | Print known peers in the chat panel |
| `/users <name> <body…>` | Send a direct message; body may be multiline |
| `/group create <name>` | Create a private group; only you see it at first |
| `/group add <group> <name>` | Invite a user; any member can invite more users |
| `/group members <group>` | List current members of a group |
| `/quit` | Exit the TUI |
| `/help` or unknown `/…` | Show short command help |

Plain text sends to the active conversation: `#general` (everyone online), a `dm:<peer_id>` thread,
or a private group (one encrypted direct message per member). Messages to offline recipients stay
marked ` (Pending)` in memory in phase 2; phase 3 will persist and retry them.

## Private groups

- Groups are invite-only and **invisible until invited**. Creating a group contacts nobody.
- When B invites C, C receives a `GroupInvite`; all existing members receive `MemberAdded`, so member
  lists converge and chain invites work (B can invite C, then C can invite D).
- New members get **no history backfill**. There is no leave/remove command in v1.
- Group messages are one direct libp2p request per member carrying the group UUID; there is no
  gossipsub group topic, so non-members never see the group name.
- Privacy caveat: names, membership and messages travel as Noise-encrypted direct traffic between
  peers, but there is **no separate group encryption key** in v1. A malicious peer sniffing the LAN
  can still observe traffic metadata. Do not treat groups as a secure channel against local sniffing.

## Files created at runtime

With the default `--data-dir data`:

- `data/identity.key` — ed25519 private key (mode `0600` on Unix). The PeerId stays the same across
  restarts. Delete this file to get a new identity.
- `data/chat.log` — tracing output. Logs go to a file so the terminal stays clean, including in TUI
  mode. Set `RUST_LOG=debug` for verbose diagnostics.

## Layout

- `src/main.rs` — identity loading, logging and wiring between network and UI; `--simple` selects the
  fallback CLI.
- `src/network/` — swarm behaviour, mDNS/discovery events, wire protocol, group events and ACK
  correlation by libp2p request id.
- `src/ui/cli.rs` — phase 1 plain terminal prompt (`--simple`).
- `src/ui/app.rs` — phase 2 state: peers, groups, conversations, pending ack map, stable colors, RAII
  terminal guard.
- `src/ui/input.rs` — phase 2 key handling, `/users` autofill, command parser.
- `src/ui/render.rs` — phase 2 ratatui layout and tests using `TestBackend`.

Every module carries its own unit tests (`make test`). Real multi-peer behaviour is checked manually
with two terminals, as described above.

## Roadmap

- **Phase 3:** rusqlite local database, outbox worker, pending messages flushed when peers come online.
