# lan-chat (Phase 1)

A serverless, peer-to-peer LAN chat written in Rust. Peers discover each other with **mDNS**,
exchange usernames and messages over **libp2p** (Noise-encrypted direct request/response), and use
gossipsub only to broadcast presence. No central server is involved.

## Requirements

- Rust 1.88+ (developed with 1.97).
- All peers must be on the **same local network**. mDNS uses multicast UDP port 5353; VPNs, Docker
  bridge networks, and WiFi client isolation usually block it.

## Quick start

```sh
make build   # cargo build
make test    # cargo test --all-targets
make run     # cargo run
```

You will be asked for a username, then you get a plain chat prompt:

```
Username: alice
Identity: 12D3Koo... (saved in data/identity.key)
Type a message and press Enter. /users lists peers, /quit exits.
Looking for peers on the local network with mDNS...

➜ bob·9f03 joined the network
<bob·9f03> hello from the other terminal
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
`chat.log`. Both instances should discover each other within a few seconds and print a join
notification. On two different laptops, the default `make run` is enough.

Commands: `/users` lists discovered peers, `/quit` (or Ctrl+C) exits. Everything else is broadcast
to all connected peers.

## Files created at runtime

- `data/identity.key` — ed25519 private key (mode `0600` on Unix). The PeerId stays the same across
  restarts. Delete this file to get a new identity.
- `data/chat.log` — tracing output. Logs go to a file so the terminal stays clean.

## Layout

- `src/main.rs` — identity loading, logging and wiring between network and UI.
- `src/network/` — swarm behaviour, mDNS/discovery events, wire protocol.
- `src/ui/cli.rs` — username prompt and plain terminal chat loop.

Every module carries its own unit tests (`make test`). Real multi-peer behaviour is checked manually
with two terminals, as described above.

## Roadmap

- **Phase 2:** ratatui TUI, `/users` autofill, private invite-only groups.
- **Phase 3:** rusqlite local database, outbox worker, pending messages flushed when peers come online.
