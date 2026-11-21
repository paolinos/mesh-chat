# lan-chat (Phase 3)

A serverless, peer-to-peer LAN chat written in Rust. Peers discover each other with **mDNS**,
exchange usernames and messages over **libp2p** (Noise-encrypted direct request/response), and use
gossipsub only to broadcast presence. No central server is involved.

Phase 2 added a full-screen ratatui UI, multiline input, `/users` autofill, private invite-only
groups, and `(Pending)` delivery state. Phase 3 adds a local SQLite outbox: messages to offline
peers stay Pending, survive restarts, and are flushed with exponential backoff when those peers come
back online. The TUI contract is unchanged. The phase 1 line-by-line CLI remains available as
`--simple` (it does not use the outbox).

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

(Equivalent: `cargo run -- --data-dir data/alice`.) Each directory gets its own `identity.key`,
`chat.db` and `chat.log`. Both instances should discover each other within a few seconds; type
`/users` to see the known peers. On two different laptops, the default `make run` is enough.

## TUI layout and keys

```
┌──────────────┬───────────────────────────────────────────┐
│ CHANNELS     │  #general | DM alice·12ab | 🔒 team       │
│ #general     ├───────────────────────────────────────────┤
│ DM alice·12ab│ [10:42] alice·12ab: hey                   │
│              │ [10:43]                       me: hi      │
│              │ [10:44] alice·12ab: multi-line            │
│              │                   message                 │
│ GROUPS       ├───────────────────────────────────────────┤
│ 🔒 team      │  Message (Shift+Enter = newline)          │
└──────────────┴───────────────────────────────────────────┘
```

- `Enter` sends the current input. `Shift+Enter` inserts a newline.
- `↑`/`↓` select a channel or group in the left panel.
- `Enter` or `Tab` switches to the highlighted channel/group; `Esc` returns focus to the input.
  Typing any character also returns focus to the input, so the next `Enter` sends your message.
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
or a private group (one encrypted direct message per member). Every chat line shows its local
`HH:MM` timestamp. A send to an offline recipient stays marked `(Pending)` in the UI and is stored
in `chat.db`. When that peer returns, the outbox worker
retries the send; the marker disappears once every recipient has acknowledged it. Failed attempts
retry at 1 s, 2 s, 4 s … capped at 5 minutes; nothing is dropped.

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
- `data/chat.db` — SQLite database (WAL mode) holding peers, messages, group memberships and outbox
  rows. History and Pending state survive restarts. Presence and in-flight delivery flags are reset
  on startup because they describe the current process, not saved history.
- `data/chat.log` — tracing output. Logs go to a file so the terminal stays clean, including in TUI
  mode. Set `RUST_LOG=debug` for verbose diagnostics.

## Layout

- `src/main.rs` — identity loading, logging and wiring between network, database worker and UI;
  `--simple` selects the fallback CLI.
- `src/network/` — swarm behaviour, mDNS/discovery events, wire protocol, group events and ACK
  correlation by libp2p request id or phase 3 outbox id.
- `src/database/` — SQLite schema and the only connection to `chat.db`, owned by the worker.
- `src/worker.rs` — `Store` request/reply API, outbox poll loop, retry backoff and event mirroring.
- `src/ui/cli.rs` — phase 1 plain terminal prompt (`--simple`), kept as a fallback.
- `src/ui/app.rs` — phase 2/3 state: peers, groups, conversations, pending ack map, stable colors,
  history restoration and RAII terminal guard.
- `src/ui/input.rs` — phase 2 key handling, `/users` autofill, command parser and channel navigation.
- `src/ui/render.rs` — phase 2 ratatui layout and tests using `TestBackend`.

Every module carries its own unit tests (`make test`). Real multi-peer behaviour is checked manually
with two or more terminals, as described above.

## Roadmap

All three planned phases are complete. Possible follow-ups: group encryption keys, leave/remove
commands, manual `/connect <multiaddr>` for restricted networks, and message search/pagination.
