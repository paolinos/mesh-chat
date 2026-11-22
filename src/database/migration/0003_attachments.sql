-- File attachments linked to chat messages.
-- For outgoing messages local_path is the absolute source path on the sender's machine.
-- For incoming messages local_path is the path under data/downloads/ where the file was saved.
CREATE TABLE IF NOT EXISTS message_attachments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  channel TEXT NOT NULL,
  filename TEXT NOT NULL,
  local_path TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  is_sender INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_attachments_message ON message_attachments(message_id);
