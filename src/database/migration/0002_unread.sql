-- Tracks conversations with messages the user has not opened yet.
-- Rows are removed as soon as the conversation is read, so the table stays small.
CREATE TABLE IF NOT EXISTS unread_conversations (
  channel TEXT PRIMARY KEY,
  first_unread_at INTEGER NOT NULL,
  last_unread_at INTEGER NOT NULL
);
