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
`chat.log`. Both instances should discover each other within a few seconds and show each other in the
USERS panel. On two different laptops, the default `make run` is enough.

## TUI layout and keys

```
┌──────────────┬───────────────────────────────────────────┐
│ USERS        │  #general | alice·12ab | 🔒 team          │
│ ● alice·12ab ├───────────────────────────────────────────┤
│ ○ bob·9f03   │  bob·9f03: hey                            │
│              │                       me: hi              │
│ CHANNELS     │  bob·9f03: multi-line                     │
│ #general     │            message                        │
│              ├───────────────────────────────────────────┤
│ GROUPS       │  Message (Shift+Enter = newline)          │
│ 🔒 team      │                                           │
└──────────────┴───────────────────────────────────────────┘
```

- `Enter` sends the current input. `Shift+Enter` inserts a newline.
- `↑`/`↓` select a user or group in the active left panel.
- `Tab` autofills `/users <selected-name> ` into the input.
- `/quit`, `/exit`, or `Ctrl+C` exits and always restores the terminal.
- Clicking/selecting is keyboard-only in v1: use `/users` to reach DMs and group commands to create
  or enter groups. Selecting `#general` in CHANNELS switches back to the public room.

Because usernames are only labels, every peer is shown as `name·<last4 of PeerId>` (PeerIds all start
with `12D3`, so the suffix is the distinguishing part). Names are resolved by prefix in commands.

## Commands

| Command | Effect |
| --- | --- |
| `/users` | Show the user list in USERS; `↑`/`↓` then `Enter` inserts `/users <name> ` |
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
