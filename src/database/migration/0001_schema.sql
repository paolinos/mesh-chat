CREATE TABLE IF NOT EXISTS peers (
  peer_id TEXT PRIMARY KEY,
  username TEXT NOT NULL,
  is_local INTEGER NOT NULL DEFAULT 0,
  online INTEGER NOT NULL DEFAULT 0,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  id TEXT PRIMARY KEY,
  channel TEXT NOT NULL,
  peer_id TEXT NOT NULL,
  username_snapshot TEXT NOT NULL,
  direction TEXT NOT NULL CHECK (direction IN ('in','out')),
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS groups (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_by TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS group_members (
  group_id TEXT NOT NULL REFERENCES groups(id),
  peer_id TEXT NOT NULL REFERENCES peers(peer_id),
  PRIMARY KEY (group_id, peer_id)
);

CREATE TABLE IF NOT EXISTS outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id TEXT REFERENCES messages(id),
  recipient TEXT NOT NULL REFERENCES peers(peer_id),
  channel TEXT,
  kind TEXT NOT NULL CHECK (kind IN ('chat','group_invite','member_added')),
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending'
    CHECK (state IN ('pending','in_flight','done','dead')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  last_error TEXT,
  UNIQUE (message_id, recipient)
);

CREATE INDEX IF NOT EXISTS idx_outbox_due ON outbox(state, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_outbox_message ON outbox(message_id);
CREATE INDEX IF NOT EXISTS idx_messages_channel ON messages(channel, created_at);
