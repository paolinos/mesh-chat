# mesh-chat — Agent Guide

> Concise instructions for coding agents working on mesh-chat.

## What this project is

A serverless, peer-to-peer terminal chat for the local network. Rust, libp2p, SQLite, ratatui.

## Quick commands

```sh
make build    # cargo build
make test     # cargo test --all-targets
make run      # cargo run (TUI)
make run ARGS="--simple"          # plain CLI
make run ARGS="--data-dir data/alice"   # two instances on one laptop
```

## Architecture (never violate these)

- **Worker owns the only SQLite connection** (`src/database/mod.rs`).
- **Network task owns the only libp2p Swarm** (`src/network/mod.rs`).
- **UI never touches SQL or the swarm directly.** It sends commands to the Worker and receives events from Network/Worker.
- Network events are mirrored to both Worker and UI so presence flushes the outbox while the UI updates.

## Key modules

| Module | Responsibility |
|---|---|
| `src/main.rs` | CLI args, identity loading, logging, task wiring |
| `src/network/protocol.rs` | Wire types: `ChatRequest`, `ChatResponse`, `ChatMessage`, `Attachment`, `Presence` |
| `src/network/mod.rs` | Swarm task, network events/commands, ACK correlation |
| `src/database/mod.rs` | SQLite API, migrations, history, outbox |
| `src/database/migration/` | `0001_schema.sql`, `0002_unread.sql`, `0003_attachments.sql` |
| `src/worker.rs` | `Store` API, outbox poll loop, retry/backoff, file read/write |
| `src/files.rs` | Downloads folder, filename sanitization, OS file open |
| `src/ui/app.rs` | TUI state, conversations, groups, history restore, terminal guard |
| `src/ui/input.rs` | Key handling, command parser (`/users`, `/file`, `/attach`, `/group`) |
| `src/ui/complete.rs` | Autocomplete for commands, names, groups, file paths |
| `src/ui/render.rs` | ratatui layout, messages, attachments, mouse hit areas |
| `src/ui/mod.rs` | Event loop, action dispatch, mouse handling |
| `src/ui/cli.rs` | `--simple` fallback CLI |

## When adding a feature

1. **Wire types first** if the protocol changes (`src/network/protocol.rs`).
2. **Database next** if storage changes:
   - Add a migration file if schema changes.
   - Update `Database::open` to run it.
   - Update `StoredMessage`/`History` if output changes.
3. **Worker next** if the feature touches persistence or the outbox.
4. **UI last** — commands, rendering, mouse/key handling.
5. **Tests in the same module** under `#[cfg(test)] mod tests`.
6. Run `make test` before finishing.

## Common gotchas

- `ChatMessage` is used everywhere. Adding a field requires updating all construction sites.
- `save_outgoing` and `insert_incoming_message` now take an attachment metadata slice.
- `Store::spawn` requires `data_dir: PathBuf` in addition to `db_path`.
- File sharing is **DM-only**. Reject `/file` in `#general` and groups.
- File paths are canonicalized before storing; relative paths and `~` are expanded.
- The sender sees the **absolute source path**; the receiver sees the **download path**.
- Max attachment size: `files::MAX_ATTACHMENT_BYTES` (15 MB).

## Code style

- Rust edition 2021, toolchain pinned in `rust-toolchain.toml`.
- Keep logic pure and testable without real network/terminal/disk.
- Use `tracing` for logs; stdout is reserved for the UI.
- Do not run `git commit`, `git push`, `git reset`, or `git rebase` unless explicitly asked.

## Context files

- `CONTEXT.md` — full technical reference.
- `README.md` — user-facing docs.
- `Audio.Feat.md` — file-sharing feature plan and progress.
