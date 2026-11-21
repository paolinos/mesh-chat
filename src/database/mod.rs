use std::{
    collections::HashSet,
    error::Error,
    fmt,
    fs,
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use libp2p::PeerId;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::network::protocol::{ChatMessage, ChatRequest};

const MAX_BACKOFF: i64 = 300_000;

#[derive(Debug)]
pub enum DatabaseError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(io::Error),
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite error: {error}"),
            Self::Json(error) => write!(f, "payload serialization error: {error}"),
            Self::Io(error) => write!(f, "database I/O error: {error}"),
        }
    }
}

impl Error for DatabaseError {}

impl From<rusqlite::Error> for DatabaseError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for DatabaseError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<io::Error> for DatabaseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, DatabaseError>;

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPeer {
    pub peer_id: String,
    pub username: String,
    pub is_local: bool,
    pub online: bool,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: String,
    pub channel: String,
    pub peer_id: String,
    pub username_snapshot: String,
    pub direction: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGroup {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub peers: Vec<StoredPeer>,
    pub messages: Vec<StoredMessage>,
    pub groups: Vec<StoredGroup>,
    pub pending_message_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: u64,
    pub message_id: Option<String>,
    pub recipient: PeerId,
    pub channel: Option<String>,
    pub kind: String,
    pub payload: String,
    pub attempts: i64,
}

/// The single SQLite connection. Only the worker thread calls these methods.
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(include_str!("migration/0001_schema.sql"))?;
        Ok(Self { conn })
    }

    /// Presence and in-flight delivery are process-local. Clear them on startup so a restart
    /// never treats old peers as online or leaves a row stuck in `in_flight`.
    pub fn reset_runtime_state(&mut self) -> Result<()> {
        self.conn.execute("UPDATE peers SET online = 0", [])?;
        self.conn.execute(
            "UPDATE outbox SET state = 'pending', next_attempt_at = ?1
             WHERE state = 'in_flight'",
            params![now_millis()],
        )?;
        Ok(())
    }

    pub fn upsert_local_peer(&mut self, peer_id: &PeerId, username: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_peer(&tx, peer_id, username, true, true, now_millis())?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_peer_online(
        &mut self,
        peer_id: PeerId,
        username: Option<&str>,
        online: bool,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        upsert_peer(
            &tx,
            &peer_id,
            username.unwrap_or("unknown"),
            online,
            false,
            now_millis(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Store an incoming message once. The UUID is the dedupe key.
    pub fn insert_incoming_message(&mut self, message: &ChatMessage) -> Result<bool> {
        let tx = self.conn.transaction()?;
        if let Ok(peer) = message.from_peer.parse::<PeerId>() {
            upsert_peer(
                &tx,
                &peer,
                &message.from_username,
                true,
                false,
                now_millis(),
            )?;
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO messages
             (id, channel, peer_id, username_snapshot, direction, body, created_at)
             VALUES (?1, ?2, ?3, ?4, 'in', ?5, ?6)",
            params![
                message.id,
                message.channel,
                message.from_peer,
                message.from_username,
                message.body,
                message.created_at
            ],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Insert one outgoing message row and one outbox row per recipient in a single transaction.
    pub fn save_outgoing(
        &mut self,
        message: &ChatMessage,
        recipients: &[PeerId],
    ) -> Result<Vec<OutboxRow>> {
        let now = now_millis();
        let tx = self.conn.transaction()?;
        if let Ok(author) = message.from_peer.parse::<PeerId>() {
            upsert_peer(&tx, &author, &message.from_username, true, true, now)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO messages
             (id, channel, peer_id, username_snapshot, direction, body, created_at)
             VALUES (?1, ?2, ?3, ?4, 'out', ?5, ?6)",
            params![
                message.id,
                message.channel,
                primary_recipient(recipients),
                message.from_username,
                message.body,
                message.created_at
            ],
        )?;

        let mut queued = Vec::new();
        for recipient in recipients {
            if recipient.to_string() == message.from_peer {
                continue;
            }
            ensure_peer(&tx, recipient, "unknown", false, now)?;
            let id = prepare_outbox_row(
                &tx,
                Some(&message.id),
                recipient,
                Some(&message.channel),
                "chat",
                now,
            )?;
            let request = ChatRequest::Chat(ChatMessage {
                outbox_id: Some(id),
                ..message.clone()
            });
            set_outbox_payload(&tx, id, &request)?;
            queued.push(outbox_row(&tx, id)?);
        }
        tx.commit()?;
        Ok(queued)
    }

    pub fn create_group(&mut self, id: &str, name: &str, created_by: &PeerId) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO groups (id, name, created_by) VALUES (?1, ?2, ?3)",
            params![id, name, created_by.to_string()],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO group_members (group_id, peer_id) VALUES (?1, ?2)",
            params![id, created_by.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Sender side of `/group add`: queue the invite plus member-added notices for existing members.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_group_invite(
        &mut self,
        group_id: &str,
        group_name: &str,
        invitee: PeerId,
        invitee_label: &str,
        existing: &[PeerId],
        invited_by: &PeerId,
    ) -> Result<Vec<OutboxRow>> {
        let now = now_millis();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO groups (id, name, created_by) VALUES (?1, ?2, ?3)",
            params![group_id, group_name, invited_by.to_string()],
        )?;
        upsert_peer(&tx, invited_by, "unknown", true, true, now)?;
        ensure_peer(&tx, &invitee, invitee_label, false, now)?;
        tx.execute(
            "INSERT OR IGNORE INTO group_members (group_id, peer_id) VALUES (?1, ?2)",
            params![group_id, invitee.to_string()],
        )?;

        let mut queued = Vec::new();
        let invite_id = prepare_outbox_row(
            &tx,
            None,
            &invitee,
            Some(group_id),
            "group_invite",
            now,
        )?;
        set_outbox_payload(
            &tx,
            invite_id,
            &ChatRequest::GroupInvite {
                group_id: group_id.to_string(),
                group_name: group_name.to_string(),
                invited_by: invited_by.to_string(),
                outbox_id: Some(invite_id),
            },
        )?;
        queued.push(outbox_row(&tx, invite_id)?);

        for peer in existing {
            if *peer == invitee || peer == invited_by {
                continue;
            }
            ensure_peer(&tx, peer, "unknown", false, now)?;
            let id =
                prepare_outbox_row(&tx, None, peer, Some(group_id), "member_added", now)?;
            set_outbox_payload(
                &tx,
                id,
                &ChatRequest::MemberAdded {
                    group_id: group_id.to_string(),
                    peer_id: invitee.to_string(),
                    username: invitee_label.to_string(),
                    invited_by: invited_by.to_string(),
                    outbox_id: Some(id),
                },
            )?;
            queued.push(outbox_row(&tx, id)?);
        }
        tx.commit()?;
        Ok(queued)
    }

    pub fn apply_group_invite(
        &mut self,
        local_peer: &PeerId,
        local_username: &str,
        from: &PeerId,
        group_id: &str,
        group_name: &str,
    ) -> Result<()> {
        let now = now_millis();
        let tx = self.conn.transaction()?;
        upsert_peer(&tx, local_peer, local_username, true, true, now)?;
        upsert_peer(&tx, from, "unknown", true, false, now)?;
        tx.execute(
            "INSERT OR IGNORE INTO groups (id, name, created_by) VALUES (?1, ?2, ?3)",
            params![group_id, group_name, from.to_string()],
        )?;
        for member in [local_peer, from] {
            tx.execute(
                "INSERT OR IGNORE INTO group_members (group_id, peer_id) VALUES (?1, ?2)",
                params![group_id, member.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn apply_member_added(
        &mut self,
        group_id: &str,
        added: &PeerId,
        username: &str,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        ensure_peer(&tx, added, username, false, now_millis())?;
        tx.execute(
            "INSERT OR IGNORE INTO group_members (group_id, peer_id) VALUES (?1, ?2)",
            params![group_id, added.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Claim due rows for online peers only, moving them to `in_flight` atomically.
    pub fn claim_due(&mut self, now: i64, limit: usize) -> Result<Vec<OutboxRow>> {
        let tx = self.conn.transaction()?;
        let mut statement = tx.prepare(
            "SELECT o.id, o.message_id, o.recipient, o.channel, o.kind, o.payload, o.attempts
             FROM outbox o
             JOIN peers p ON p.peer_id = o.recipient
             WHERE o.state = 'pending' AND o.next_attempt_at <= ?1 AND p.online = 1
             ORDER BY o.next_attempt_at, o.id
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![now, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let mut claimed = Vec::new();
        for (id, message_id, recipient, channel, kind, payload, attempts) in rows {
            let Some(recipient) = recipient.parse::<PeerId>().ok() else {
                continue;
            };
            tx.execute(
                "UPDATE outbox SET state = 'in_flight', attempts = attempts + 1 WHERE id = ?1",
                params![id as i64],
            )?;
            claimed.push(OutboxRow {
                id,
                message_id,
                recipient,
                channel,
                kind,
                payload,
                attempts: attempts + 1,
            });
        }
        tx.commit()?;
        Ok(claimed)
    }

    pub fn message_pending(&self, message_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM outbox
             WHERE message_id = ?1 AND state <> 'done')",
            params![message_id],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    pub fn ack(&mut self, outbox_id: u64) -> Result<Option<String>> {
        let message_id = self
            .conn
            .query_row(
                "SELECT message_id FROM outbox WHERE id = ?1",
                params![outbox_id as i64],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        self.conn.execute(
            "UPDATE outbox SET state = 'done', last_error = NULL WHERE id = ?1",
            params![outbox_id as i64],
        )?;
        Ok(message_id)
    }

    pub fn fail(&mut self, outbox_id: u64, error: &str) -> Result<Option<String>> {
        let (message_id, attempts) = self
            .conn
            .query_row(
                "SELECT message_id, attempts FROM outbox WHERE id = ?1",
                params![outbox_id as i64],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or((None, 1));
        self.conn.execute(
            "UPDATE outbox SET state = 'pending', next_attempt_at = ?2, last_error = ?3
             WHERE id = ?1",
            params![
                outbox_id as i64,
                now_millis() + backoff_millis(attempts),
                error
            ],
        )?;
        Ok(message_id)
    }

    /// An in-flight row must not stay stuck while its peer is offline.
    pub fn reset_in_flight_for_peer(&mut self, peer_id: &PeerId) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET state = 'pending', next_attempt_at = ?2
             WHERE recipient = ?1 AND state = 'in_flight'",
            params![peer_id.to_string(), now_millis()],
        )?;
        Ok(())
    }

    pub fn history(&self) -> Result<History> {
        let mut history = History::default();
        let mut peers = self.conn.prepare(
            "SELECT peer_id, username, is_local, online, first_seen, last_seen FROM peers",
        )?;
        let peer_rows = peers.query_map([], |row| {
            Ok(StoredPeer {
                peer_id: row.get(0)?,
                username: row.get(1)?,
                is_local: row.get::<_, i64>(2)? != 0,
                online: row.get::<_, i64>(3)? != 0,
                first_seen: row.get(4)?,
                last_seen: row.get(5)?,
            })
        })?;
        for peer in peer_rows {
            history.peers.push(peer?);
        }
        drop(peers);

        let mut messages = self.conn.prepare(
            "SELECT id, channel, peer_id, username_snapshot, direction, body, created_at
             FROM messages ORDER BY created_at, id",
        )?;
        let message_rows = messages.query_map([], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                channel: row.get(1)?,
                peer_id: row.get(2)?,
                username_snapshot: row.get(3)?,
                direction: row.get(4)?,
                body: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        for message in message_rows {
            history.messages.push(message?);
        }
        drop(messages);

        let mut groups = self.conn.prepare("SELECT id, name FROM groups ORDER BY name, id")?;
        let group_rows = groups
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(groups);
        for (id, name) in group_rows {
            let mut members = self.conn.prepare(
                "SELECT peer_id FROM group_members WHERE group_id = ?1 ORDER BY peer_id",
            )?;
            let member_rows = members
                .query_map(params![id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            history.groups.push(StoredGroup {
                id,
                name,
                members: member_rows,
            });
        }

        let mut pending = self.conn.prepare(
            "SELECT DISTINCT message_id FROM outbox
             WHERE message_id IS NOT NULL AND state <> 'done'",
        )?;
        let pending_rows = pending.query_map([], |row| row.get::<_, String>(0))?;
        for id in pending_rows {
            history.pending_message_ids.insert(id?);
        }
        Ok(history)
    }
}

fn primary_recipient(recipients: &[PeerId]) -> String {
    recipients
        .first()
        .map(|peer| peer.to_string())
        .unwrap_or_default()
}

fn backoff_millis(attempts: i64) -> i64 {
    let exponent = (attempts - 1).clamp(0, 9) as u32;
    1_000_i64
        .checked_shl(exponent)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

fn upsert_peer(
    tx: &Transaction<'_>,
    peer_id: &PeerId,
    username: &str,
    online: bool,
    is_local: bool,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO peers (peer_id, username, is_local, online, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(peer_id) DO UPDATE SET
           username = CASE
             WHEN excluded.username = 'unknown' AND peers.username <> 'unknown'
             THEN peers.username ELSE excluded.username END,
           is_local = MAX(peers.is_local, excluded.is_local),
           online = excluded.online,
           last_seen = excluded.last_seen",
        params![
            peer_id.to_string(),
            username,
            i64::from(is_local),
            i64::from(online),
            now
        ],
    )?;
    Ok(())
}

/// Ensure a peer row exists without changing a live presence flag.
fn ensure_peer(
    tx: &Transaction<'_>,
    peer_id: &PeerId,
    username: &str,
    is_local: bool,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO peers (peer_id, username, is_local, online, first_seen, last_seen)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)
         ON CONFLICT(peer_id) DO UPDATE SET
           username = CASE
             WHEN excluded.username = 'unknown' AND peers.username <> 'unknown'
             THEN peers.username ELSE excluded.username END,
           is_local = MAX(peers.is_local, excluded.is_local),
           last_seen = excluded.last_seen",
        params![peer_id.to_string(), username, i64::from(is_local), now],
    )?;
    Ok(())
}

/// Insert a pending row (or reset a duplicate) and return its id. The payload is filled in next.
fn prepare_outbox_row(
    tx: &Transaction<'_>,
    message_id: Option<&str>,
    recipient: &PeerId,
    channel: Option<&str>,
    kind: &str,
    now: i64,
) -> Result<u64> {
    let existing = if let Some(message_id) = message_id {
        tx.query_row(
            "SELECT id FROM outbox WHERE message_id = ?1 AND recipient = ?2",
            params![message_id, recipient.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    } else {
        None
    };
    let id = if let Some(id) = existing {
        tx.execute(
            "UPDATE outbox SET state = 'pending', channel = ?2, kind = ?3,
             next_attempt_at = ?4, payload = '{}' WHERE id = ?1",
            params![id, channel, kind, now],
        )?;
        id as u64
    } else {
        tx.execute(
            "INSERT INTO outbox (message_id, recipient, channel, kind, payload, state,
             attempts, next_attempt_at)
             VALUES (?1, ?2, ?3, ?4, '{}', 'pending', 0, ?5)",
            params![message_id, recipient.to_string(), channel, kind, now],
        )?;
        tx.last_insert_rowid() as u64
    };
    Ok(id)
}

fn set_outbox_payload(
    tx: &Transaction<'_>,
    id: u64,
    request: &ChatRequest,
) -> Result<()> {
    tx.execute(
        "UPDATE outbox SET payload = ?2 WHERE id = ?1",
        params![id as i64, serde_json::to_string(request)?],
    )?;
    Ok(())
}

fn outbox_row(tx: &Transaction<'_>, id: u64) -> Result<OutboxRow> {
    Ok(tx.query_row(
        "SELECT id, message_id, recipient, channel, kind, payload, attempts FROM outbox WHERE id = ?1",
        params![id as i64],
        |row| {
            Ok(OutboxRow {
                id: row.get::<_, i64>(0)? as u64,
                message_id: row.get::<_, Option<String>>(1)?,
                recipient: row.get::<_, String>(2)?.parse::<PeerId>().unwrap_or_else(|_| {
                    // This cannot happen for rows created by this module; keep the loop alive.
                    PeerId::random()
                }),
                channel: row.get::<_, Option<String>>(3)?,
                kind: row.get::<_, String>(4)?,
                payload: row.get::<_, String>(5)?,
                attempts: row.get::<_, i64>(6)?,
            })
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn peer(value: &str) -> PeerId {
        value.parse().unwrap()
    }

    fn alice() -> PeerId {
        peer("12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB")
    }

    fn bob() -> PeerId {
        peer("12D3KooWNgyWGoSc276cjs6MLNmtsGmNnspB8TdCQ22G69vXaDr4")
    }

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let database = Database::open(&dir.path().join("chat.db")).unwrap();
        (dir, database)
    }

    fn message(id: &str, channel: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            from_peer: alice().to_string(),
            from_username: "alice".to_string(),
            channel: channel.to_string(),
            body: "hello".to_string(),
            created_at: 10,
            outbox_id: None,
        }
    }

    #[test]
    fn migration_creates_schema_and_local_peer() {
        let (_dir, mut database) = db();
        database.upsert_local_peer(&alice(), "alice").unwrap();
        let table: String = database
            .conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'outbox'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table, "outbox");
        let history = database.history().unwrap();
        assert_eq!(history.peers.len(), 1);
        assert!(history.peers[0].is_local);
        assert_eq!(history.peers[0].username, "alice");
    }

    #[test]
    fn startup_clears_stale_presence_and_in_flight_rows() {
        let (_dir, mut database) = db();
        database.upsert_local_peer(&alice(), "alice").unwrap();
        database.set_peer_online(bob(), Some("bob"), true).unwrap();
        database
            .save_outgoing(&message("m1", "#general"), &[bob()])
            .unwrap();
        assert_eq!(database.claim_due(now_millis() + 1, 10).unwrap().len(), 1);

        database.reset_runtime_state().unwrap();
        let history = database.history().unwrap();
        let bob_row = history
            .peers
            .iter()
            .find(|peer| peer.peer_id == bob().to_string())
            .unwrap();
        assert!(!bob_row.online);
        // The peer is offline now, so the reset row cannot be claimed yet.
        assert!(database.claim_due(now_millis() + 1, 10).unwrap().is_empty());

        database.set_peer_online(bob(), Some("bob"), true).unwrap();
        assert_eq!(database.claim_due(now_millis() + 1, 10).unwrap().len(), 1);
    }

    #[test]
    fn incoming_messages_are_deduped_and_persisted() {
        let (_dir, mut database) = db();
        let incoming = ChatMessage {
            from_peer: bob().to_string(),
            from_username: "bob".to_string(),
            ..message("m1", "#general")
        };
        assert!(database.insert_incoming_message(&incoming).unwrap());
        assert!(!database.insert_incoming_message(&incoming).unwrap());
        let history = database.history().unwrap();
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].direction, "in");
        assert!(history.peers.iter().any(|peer| peer.peer_id == bob().to_string() && peer.online));
    }

    #[test]
    fn outbox_waits_for_online_peer_and_backs_off_on_failure() {
        let (_dir, mut database) = db();
        database.upsert_local_peer(&alice(), "alice").unwrap();
        let queued = database
            .save_outgoing(&message("m1", &format!("dm:{}", bob())), &[bob()])
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert!(database.claim_due(now_millis() + 1, 10).unwrap().is_empty());

        database.set_peer_online(bob(), Some("bob"), true).unwrap();
        let claimed = database.claim_due(now_millis() + 1, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].message_id.as_deref(), Some("m1"));
        assert_eq!(claimed[0].attempts, 1);
        assert!(database.claim_due(now_millis() + 1, 10).unwrap().is_empty());

        let request: ChatRequest = serde_json::from_str(&claimed[0].payload).unwrap();
        assert!(matches!(
            request,
            ChatRequest::Chat(ChatMessage {
                outbox_id: Some(id),
                ..
            }) if id == claimed[0].id
        ));

        database.fail(claimed[0].id, "timeout").unwrap();
        let history = database.history().unwrap();
        assert!(history.pending_message_ids.contains("m1"));
        assert!(database.claim_due(now_millis() + 1, 10).unwrap().is_empty());
        let claimed = database.claim_due(now_millis() + 1_001, 10).unwrap();
        assert_eq!(claimed[0].attempts, 2);
        assert_eq!(database.ack(claimed[0].id).unwrap().as_deref(), Some("m1"));
        assert!(database.history().unwrap().pending_message_ids.is_empty());
    }

    #[test]
    fn offline_in_flight_rows_are_reset() {
        let (_dir, mut database) = db();
        database.upsert_local_peer(&alice(), "alice").unwrap();
        database.set_peer_online(bob(), Some("bob"), true).unwrap();
        database
            .save_outgoing(&message("m1", "#general"), &[bob()])
            .unwrap();
        assert_eq!(database.claim_due(now_millis() + 1, 10).unwrap().len(), 1);

        database.set_peer_online(bob(), Some("bob"), false).unwrap();
        database.reset_in_flight_for_peer(&bob()).unwrap();
        assert!(database.claim_due(now_millis() + 1, 10).unwrap().is_empty());

        database.set_peer_online(bob(), Some("bob"), true).unwrap();
        assert_eq!(database.claim_due(now_millis() + 1, 10).unwrap().len(), 1);
    }

    #[test]
    fn group_invite_fans_out_and_survives_restart() {
        let (_dir, mut database) = db();
        let carol = PeerId::random();
        database.upsert_local_peer(&alice(), "alice").unwrap();
        database.create_group("g1", "team", &alice()).unwrap();
        let rows = database
            .queue_group_invite("g1", "team", bob(), "bob·abcd", &[carol], &alice())
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "group_invite");
        assert_eq!(rows[1].kind, "member_added");
        for row in &rows {
            let request: ChatRequest = serde_json::from_str(&row.payload).unwrap();
            let id = match request {
                ChatRequest::GroupInvite { outbox_id, .. }
                | ChatRequest::MemberAdded { outbox_id, .. } => outbox_id,
                _ => None,
            };
            assert_eq!(id, Some(row.id));
        }

        database
            .apply_group_invite(&bob(), "bob", &alice(), "g2", "book")
            .unwrap();
        database
            .apply_member_added("g2", &carol, "carol")
            .unwrap();
        let history = database.history().unwrap();
        let g1 = history.groups.iter().find(|group| group.id == "g1").unwrap();
        assert!(g1.members.contains(&alice().to_string()));
        assert!(g1.members.contains(&bob().to_string()));
        let g2 = history.groups.iter().find(|group| group.id == "g2").unwrap();
        assert!(g2.members.contains(&bob().to_string()));
        assert!(g2.members.contains(&carol.to_string()));

        drop(database);
        let reopened = Database::open(&_dir.path().join("chat.db")).unwrap();
        assert_eq!(reopened.history().unwrap().groups.len(), 2);
    }

    #[test]
    fn backoff_is_exponential_then_capped() {
        assert_eq!(backoff_millis(1), 1_000);
        assert_eq!(backoff_millis(2), 2_000);
        assert_eq!(backoff_millis(3), 4_000);
        assert_eq!(backoff_millis(20), MAX_BACKOFF);
    }
}
