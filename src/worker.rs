use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc as std_mpsc, Arc},
    time::{Duration, Instant},
};

use libp2p::PeerId;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::warn;

use crate::{
    database::{Database, History, OutboxRow, StoredAttachment},
    files::{self, store_incoming_file},
    network::{
        protocol::{Attachment, ChatMessage, ChatRequest},
        NetworkCommand, NetworkEvent,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const CLAIM_BATCH: usize = 32;

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

#[derive(Debug)]
pub enum WorkerCommand {
    LoadHistory { reply: Reply<History> },
    SaveOutgoing {
        message: ChatMessage,
        recipients: Vec<PeerId>,
        attachment_paths: Vec<PathBuf>,
        reply: Reply<()>,
    },
    CreateGroup {
        id: String,
        name: String,
        reply: Reply<()>,
    },
    QueueGroupInvite {
        group_id: String,
        group_name: String,
        invitee: PeerId,
        invitee_label: String,
        existing: Vec<PeerId>,
        reply: Reply<()>,
    },
    IncomingMessage {
        message: ChatMessage,
        conversation: String,
        unread: bool,
    },
    MarkConversationRead { channel: String },
    PeerOnline {
        peer_id: PeerId,
        username: String,
    },
    PeerOffline {
        peer_id: PeerId,
        username: Option<String>,
    },
    GroupInviteApplied {
        from: PeerId,
        group_id: String,
        group_name: String,
    },
    MemberAddedApplied {
        group_id: String,
        peer_id: PeerId,
        username: String,
    },
    DeliveryReport {
        outbox_id: u64,
        ok: bool,
        error: Option<String>,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    PendingChanged {
        message_id: String,
        pending: bool,
    },
    DeliveryFailed {
        message_id: Option<String>,
        outbox_id: u64,
        error: String,
    },
    AttachmentsStored {
        message_id: String,
        attachments: Vec<StoredAttachment>,
    },
}

#[derive(Clone)]
pub struct Store {
    commands: Arc<StoreInner>,
}

#[derive(Clone)]
struct StoreInner {
    tx: std_mpsc::Sender<WorkerCommand>,
}

pub struct WorkerHandle {
    pub events: mpsc::UnboundedReceiver<WorkerEvent>,
    pub task: JoinHandle<Result<(), String>>,
}

impl Store {
    pub fn spawn(
        db_path: &Path,
        data_dir: PathBuf,
        me: PeerId,
        username: String,
        network_tx: mpsc::Sender<NetworkCommand>,
    ) -> Result<(Self, WorkerHandle), crate::database::DatabaseError> {
        let mut database = Database::open(db_path)?;
        database.reset_runtime_state()?;
        database.upsert_local_peer(&me, &username)?;

        let (command_tx, command_rx) = std_mpsc::channel::<WorkerCommand>();
        let (event_tx, events) = mpsc::unbounded_channel::<WorkerEvent>();
        let task = tokio::task::spawn_blocking(move || {
            worker_loop(database, data_dir, me, username, command_rx, network_tx, event_tx)
        });
        Ok((
            Self {
                commands: Arc::new(StoreInner { tx: command_tx }),
            },
            WorkerHandle { events, task },
        ))
    }

    async fn call<T>(
        &self,
        build: impl FnOnce(Reply<T>) -> WorkerCommand,
    ) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .tx
            .send(build(tx))
            .map_err(|_| "database worker has stopped".to_string())?;
        rx.await.map_err(|_| "database worker has stopped".to_string())?
    }

    pub async fn load_history(&self) -> Result<History, String> {
        self.call(|reply| WorkerCommand::LoadHistory { reply }).await
    }

    pub async fn save_outgoing(
        &self,
        message: ChatMessage,
        recipients: Vec<PeerId>,
        attachment_paths: Vec<PathBuf>,
    ) -> Result<(), String> {
        self.call(|reply| WorkerCommand::SaveOutgoing {
            message,
            recipients,
            attachment_paths,
            reply,
        })
        .await
    }

    pub async fn create_group(&self, id: String, name: String) -> Result<(), String> {
        self.call(|reply| WorkerCommand::CreateGroup { id, name, reply })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn queue_group_invite(
        &self,
        group_id: String,
        group_name: String,
        invitee: PeerId,
        invitee_label: String,
        existing: Vec<PeerId>,
    ) -> Result<(), String> {
        self.call(|reply| WorkerCommand::QueueGroupInvite {
            group_id,
            group_name,
            invitee,
            invitee_label,
            existing,
            reply,
        })
        .await
    }

    /// Store an incoming message together with its unread marker. The UI supplies the canonical
    /// conversation key and whether it is already open. Never blocks the UI.
    pub fn record_incoming_message(
        &self,
        message: &ChatMessage,
        conversation: &str,
        unread: bool,
    ) {
        let _ = self.commands.tx.send(WorkerCommand::IncomingMessage {
            message: message.clone(),
            conversation: conversation.to_string(),
            unread,
        });
    }

    /// Forget unread state when the user opens a conversation. Never blocks the UI.
    pub fn mark_conversation_read(&self, channel: &str) {
        let _ = self.commands.tx.send(WorkerCommand::MarkConversationRead {
            channel: channel.to_string(),
        });
    }

    /// Mirror network presence/group events into the database. Never blocks the UI.
    pub fn observe_network_event(&self, event: &NetworkEvent) {
        let command = match event {
            NetworkEvent::PeerConnected { peer_id, username }
            | NetworkEvent::Presence {
                peer_id,
                username,
                online: true,
            } => Some(WorkerCommand::PeerOnline {
                peer_id: *peer_id,
                username: username.clone(),
            }),
            NetworkEvent::PeerDisconnected { peer_id, username } => {
                Some(WorkerCommand::PeerOffline {
                    peer_id: *peer_id,
                    username: username.clone(),
                })
            }
            NetworkEvent::Presence {
                peer_id,
                username,
                online: false,
            } => Some(WorkerCommand::PeerOffline {
                peer_id: *peer_id,
                username: Some(username.clone()),
            }),
            NetworkEvent::PeerExpired { peer_id } => Some(WorkerCommand::PeerOffline {
                peer_id: *peer_id,
                username: None,
            }),
            NetworkEvent::GroupInviteReceived {
                from,
                group_id,
                group_name,
                ..
            } => Some(WorkerCommand::GroupInviteApplied {
                from: *from,
                group_id: group_id.clone(),
                group_name: group_name.clone(),
            }),
            NetworkEvent::MemberAddedReceived {
                group_id,
                peer_id,
                username,
                ..
            } => Some(WorkerCommand::MemberAddedApplied {
                group_id: group_id.clone(),
                peer_id: *peer_id,
                username: username.clone(),
            }),
            NetworkEvent::DeliveryReport {
                outbox_id,
                ok,
                error,
            } => Some(WorkerCommand::DeliveryReport {
                outbox_id: *outbox_id,
                ok: *ok,
                error: error.clone(),
            }),
            // Incoming messages are stored by `record_incoming_message`, which also knows the
            // canonical conversation key and whether the user is already reading it.
            NetworkEvent::MessageReceived { .. }
            | NetworkEvent::PeerDiscovered { .. }
            | NetworkEvent::MessageAck { .. }
            | NetworkEvent::Error { .. } => None,
        };
        if let Some(command) = command {
            let _ = self.commands.tx.send(command);
        }
    }

    pub fn shutdown(&self) {
        let _ = self.commands.tx.send(WorkerCommand::Shutdown);
    }
}

fn worker_loop(
    mut database: Database,
    data_dir: PathBuf,
    me: PeerId,
    username: String,
    commands: std_mpsc::Receiver<WorkerCommand>,
    network_tx: mpsc::Sender<NetworkCommand>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), String> {
    let mut next_poll = Instant::now() + POLL_INTERVAL;
    loop {
        let timeout = next_poll.saturating_duration_since(Instant::now());
        match commands.recv_timeout(timeout) {
            Ok(WorkerCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(command) => {
                let should_poll = match handle_command(
                    &mut database,
                    &data_dir,
                    &me,
                    &username,
                    command,
                    &network_tx,
                    &event_tx,
                ) {
                    Ok(poll_now) => poll_now,
                    Err(error) => {
                        warn!(%error, "database worker command failed");
                        false
                    }
                };
                if should_poll || Instant::now() >= next_poll {
                    poll_due(&mut database, &network_tx, &event_tx)?;
                    next_poll = Instant::now() + POLL_INTERVAL;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                poll_due(&mut database, &network_tx, &event_tx)?;
                next_poll = Instant::now() + POLL_INTERVAL;
            }
        }
    }
    Ok(())
}

fn handle_command(
    database: &mut Database,
    data_dir: &Path,
    me: &PeerId,
    username: &str,
    command: WorkerCommand,
    _network_tx: &mpsc::Sender<NetworkCommand>,
    event_tx: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<bool, String> {
    let mut poll_now = false;
    match command {
        WorkerCommand::LoadHistory { reply } => {
            let result = database.history().map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        WorkerCommand::SaveOutgoing {
            message,
            recipients,
            attachment_paths,
            reply,
        } => {
            let result = save_outgoing_with_attachments(
                database,
                data_dir,
                &message,
                &recipients,
                &attachment_paths,
            )
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = reply.send(result);
            poll_now = true;
        }
        WorkerCommand::CreateGroup { id, name, reply } => {
            let result = database
                .create_group(&id, &name, me)
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        WorkerCommand::QueueGroupInvite {
            group_id,
            group_name,
            invitee,
            invitee_label,
            existing,
            reply,
        } => {
            let result = database
                .queue_group_invite(
                    &group_id,
                    &group_name,
                    invitee,
                    &invitee_label,
                    &existing,
                    me,
                )
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
            poll_now = true;
        }
        WorkerCommand::IncomingMessage {
            message,
            conversation,
            unread,
        } => {
            let attachments = match store_incoming_attachments(data_dir, &message) {
                Ok(attachments) => attachments,
                Err(error) => {
                    warn!(%error, "could not store incoming attachments");
                    Vec::new()
                }
            };
            if let Err(error) =
                database.insert_incoming_message(&message, &conversation, unread, &attachments)
            {
                warn!(%error, "could not store incoming message");
            } else if !attachments.is_empty() {
                let _ = event_tx.send(WorkerEvent::AttachmentsStored {
                    message_id: message.id,
                    attachments,
                });
            }
        }
        WorkerCommand::MarkConversationRead { channel } => {
            if let Err(error) = database.clear_unread(&channel) {
                warn!(%error, "could not clear unread marker");
            }
        }
        WorkerCommand::PeerOnline { peer_id, username } => {
            if let Err(error) = database.set_peer_online(peer_id, Some(&username), true) {
                warn!(%error, "could not mark peer online");
            }
            // A reconnect is the signal that queued rows for this peer can go out now.
            poll_now = true;
        }
        WorkerCommand::PeerOffline { peer_id, username } => {
            if let Err(error) = database.set_peer_online(peer_id, username.as_deref(), false) {
                warn!(%error, "could not mark peer offline");
            }
            if let Err(error) = database.reset_in_flight_for_peer(&peer_id) {
                warn!(%error, "could not reset in-flight rows");
            }
        }
        WorkerCommand::GroupInviteApplied {
            from,
            group_id,
            group_name,
        } => {
            if let Err(error) =
                database.apply_group_invite(me, username, &from, &group_id, &group_name)
            {
                warn!(%error, "could not store group invite");
            }
        }
        WorkerCommand::MemberAddedApplied {
            group_id,
            peer_id,
            username,
        } => {
            if let Err(error) = database.apply_member_added(&group_id, &peer_id, &username) {
                warn!(%error, "could not store group member");
            }
        }
        WorkerCommand::DeliveryReport {
            outbox_id,
            ok,
            error,
        } => {
            let result = if ok {
                database.ack(outbox_id)
            } else {
                database.fail(outbox_id, error.as_deref().unwrap_or("delivery failed"))
            };
            match result {
                Ok(message_id) => {
                    if let Some(id) = &message_id {
                        let pending = database
                            .message_pending(id)
                            .map_err(|error| error.to_string())?;
                        let _ = event_tx.send(WorkerEvent::PendingChanged {
                            message_id: id.clone(),
                            pending,
                        });
                    }
                    if !ok {
                        let _ = event_tx.send(WorkerEvent::DeliveryFailed {
                            message_id,
                            outbox_id,
                            error: error.unwrap_or_else(|| "delivery failed".to_string()),
                        });
                    }
                }
                Err(error) => warn!(%error, outbox_id, "could not update outbox"),
            }
            poll_now = true;
        }
        WorkerCommand::Shutdown => {}
    }
    Ok(poll_now)
}

fn save_outgoing_with_attachments(
    database: &mut Database,
    _data_dir: &Path,
    message: &ChatMessage,
    recipients: &[PeerId],
    attachment_paths: &[PathBuf],
) -> Result<Vec<OutboxRow>, String> {
    let (wire_attachments, stored_attachments) = read_source_attachments(attachment_paths)?;
    let mut message = message.clone();
    message.attachments = wire_attachments;
    // The sender sees the absolute source path of the original file, not a copy.
    database
        .save_outgoing(&message, recipients, &stored_attachments)
        .map_err(|error| error.to_string())
}

fn read_source_attachments(
    paths: &[PathBuf],
) -> Result<(Vec<Attachment>, Vec<StoredAttachment>), String> {
    let mut wire = Vec::new();
    let mut stored = Vec::new();
    let mut total: usize = 0;
    for path in paths {
        let bytes = fs::read(path).map_err(|error| {
            format!("could not read attachment {}: {error}", path.display())
        })?;
        let size = bytes.len();
        total = total.checked_add(size).ok_or("attachment size overflow")?;
        if total > files::MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "attachments exceed {} MB limit",
                files::MAX_ATTACHMENT_BYTES / (1024 * 1024)
            ));
        }
        let absolute = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let filename = absolute
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        wire.push(Attachment {
            filename: filename.clone(),
            bytes,
            mime_type: None,
        });
        stored.push(StoredAttachment {
            filename,
            local_path: absolute.to_string_lossy().to_string(),
            size_bytes: size as i64,
            is_sender: true,
        });
    }
    Ok((wire, stored))
}

fn store_incoming_attachments(
    data_dir: &Path,
    message: &ChatMessage,
) -> Result<Vec<StoredAttachment>, String> {
    let mut stored = Vec::new();
    for attachment in &message.attachments {
        let path = store_incoming_file(
            data_dir,
            message.created_at,
            &message.from_username,
            &attachment.filename,
            &attachment.bytes,
        )
        .map_err(|error| {
            format!(
                "could not store incoming attachment {}: {error}",
                attachment.filename
            )
        })?;
        stored.push(StoredAttachment {
            filename: attachment.filename.clone(),
            local_path: path.to_string_lossy().to_string(),
            size_bytes: attachment.bytes.len() as i64,
            is_sender: false,
        });
    }
    Ok(stored)
}

fn poll_due(
    database: &mut Database,
    network_tx: &mpsc::Sender<NetworkCommand>,
    event_tx: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), String> {
    let due = database
        .claim_due(crate::database::now_millis(), CLAIM_BATCH)
        .map_err(|error| error.to_string())?;
    for row in due {
        let request = match serde_json::from_str::<ChatRequest>(&row.payload) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, outbox_id = row.id, "dropping malformed outbox payload");
                let _ = database.fail(row.id, "malformed outbox payload");
                continue;
            }
        };
        if network_tx
            .blocking_send(NetworkCommand::SendDirect {
                request,
                recipients: vec![row.recipient],
            })
            .is_err()
        {
            let error = "network task stopped before delivery".to_string();
            let _ = database.fail(row.id, &error);
            if let Some(message_id) = row.message_id {
                let _ = event_tx.send(WorkerEvent::PendingChanged {
                    message_id,
                    pending: true,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::protocol::ChatResponse;
    use tokio::time::{timeout, Duration};

    fn alice() -> PeerId {
        "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap()
    }

    fn bob() -> PeerId {
        "12D3KooWNgyWGoSc276cjs6MLNmtsGmNnspB8TdCQ22G69vXaDr4"
            .parse()
            .unwrap()
    }

    fn message(id: &str, channel: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            from_peer: alice().to_string(),
            from_username: "alice".to_string(),
            channel: channel.to_string(),
            body: "hello".to_string(),
            created_at: 1,
            outbox_id: None,
            attachments: Vec::new(),
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
        handle: WorkerHandle,
        network_rx: mpsc::Receiver<NetworkCommand>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (network_tx, network_rx) = mpsc::channel(16);
        let (store, handle) = Store::spawn(
            &dir.path().join("chat.db"),
            dir.path().to_path_buf(),
            alice(),
            "alice".to_string(),
            network_tx,
        )
        .unwrap();
        Fixture {
            dir,
            store,
            handle,
            network_rx,
        }
    }

    impl Fixture {
        /// Stop the worker but hand the temp directory back so restart tests can reopen it.
        async fn stop(self) -> tempfile::TempDir {
            self.store.shutdown();
            let _ = timeout(Duration::from_secs(2), self.handle.task).await;
            self.dir
        }
    }

    async fn next_command(rx: &mut mpsc::Receiver<NetworkCommand>) -> NetworkCommand {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for network command")
            .expect("network command channel closed")
    }

    #[tokio::test]
    async fn outbox_flushes_only_after_peer_comes_online() {
        let mut fixture = fixture();
        fixture
            .store
            .save_outgoing(message("m1", "#general"), vec![bob()], Vec::new())
            .await
            .unwrap();
        assert!(timeout(Duration::from_millis(150), fixture.network_rx.recv())
            .await
            .is_err());

        fixture.store.observe_network_event(&NetworkEvent::Presence {
            peer_id: bob(),
            username: "bob".to_string(),
            online: true,
        });
        let command = next_command(&mut fixture.network_rx).await;
        let (request, recipients) = match command {
            NetworkCommand::SendDirect {
                request,
                recipients,
            } => (request, recipients),
            _ => panic!("wrong command"),
        };
        assert_eq!(recipients, vec![bob()]);
        let outbox_id = match request {
            ChatRequest::Chat(ChatMessage { outbox_id, .. }) => outbox_id.unwrap(),
            _ => panic!("wrong request"),
        };
        fixture
            .store
            .observe_network_event(&NetworkEvent::DeliveryReport {
                outbox_id,
                ok: true,
                error: None,
            });
        let event = timeout(Duration::from_secs(2), fixture.handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event,
            WorkerEvent::PendingChanged {
                message_id: "m1".to_string(),
                pending: false
            }
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn group_invite_queues_invite_and_member_notice() {
        let mut fixture = fixture();
        let carol = PeerId::random();
        fixture
            .store
            .create_group("g1".to_string(), "team".to_string())
            .await
            .unwrap();
        fixture
            .store
            .queue_group_invite(
                "g1".to_string(),
                "team".to_string(),
                bob(),
                "bob·abcd".to_string(),
                vec![carol],
            )
            .await
            .unwrap();
        fixture.store.observe_network_event(&NetworkEvent::Presence {
            peer_id: bob(),
            username: "bob".to_string(),
            online: true,
        });
        fixture.store.observe_network_event(&NetworkEvent::Presence {
            peer_id: carol,
            username: "carol".to_string(),
            online: true,
        });

        let mut kinds = Vec::new();
        for _ in 0..2 {
            match next_command(&mut fixture.network_rx).await {
                NetworkCommand::SendDirect { request, .. } => match request {
                    ChatRequest::GroupInvite { outbox_id, .. }
                    | ChatRequest::MemberAdded { outbox_id, .. } => {
                        assert!(outbox_id.is_some());
                        kinds.push(1);
                    }
                    _ => panic!("wrong request"),
                },
                _ => panic!("wrong command"),
            }
        }
        assert_eq!(kinds.len(), 2);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn history_survives_worker_restart() {
        let fixture = fixture();
        fixture
            .store
            .create_group("g1".to_string(), "team".to_string())
            .await
            .unwrap();
        fixture
            .store
            .save_outgoing(message("m1", "#general"), vec![bob()], Vec::new())
            .await
            .unwrap();

        let db_path = fixture.dir.path().join("chat.db");
        let data_dir = fixture.dir.path().to_path_buf();
        let _dir = fixture.stop().await;
        let (network_tx, _network_rx) = mpsc::channel(4);
        let (store, handle) = Store::spawn(
            &db_path,
            data_dir,
            alice(),
            "alice".to_string(),
            network_tx,
        )
        .unwrap();
        let history = store.load_history().await.unwrap();
        assert!(history.messages.iter().any(|item| item.id == "m1"));
        assert!(history.groups.iter().any(|group| group.id == "g1"));
        assert!(history.pending_message_ids.contains("m1"));
        store.shutdown();
        let _ = timeout(Duration::from_secs(2), handle.task).await;
    }

    #[tokio::test]
    async fn incoming_message_and_unread_marker_follow_store_commands() {
        let fixture = fixture();
        let dm = format!("dm:{}", bob());
        let incoming = ChatMessage {
            from_peer: bob().to_string(),
            from_username: "bob".to_string(),
            ..message("m1", "dm:me")
        };

        // `observe_network_event` must not store messages: the UI supplies the canonical key and
        // the read state, so only `record_incoming_message` writes them.
        fixture.store.observe_network_event(&NetworkEvent::MessageReceived {
            message: incoming.clone(),
        });
        let history = fixture.store.load_history().await.unwrap();
        assert!(history.messages.is_empty());
        assert!(history.unread.is_empty());

        fixture
            .store
            .record_incoming_message(&incoming, &dm, true);
        let history = fixture.store.load_history().await.unwrap();
        assert!(history.messages.iter().any(|item| item.id == "m1"));
        assert_eq!(history.unread.len(), 1);
        assert_eq!(history.unread[0].channel, dm);

        fixture.store.mark_conversation_read(&dm);
        let history = fixture.store.load_history().await.unwrap();
        assert_eq!(history.messages.len(), 1);
        assert!(history.unread.is_empty());
        fixture.stop().await;
    }

    #[tokio::test]
    async fn outgoing_attachment_uses_absolute_source_path() {
        let mut fixture = fixture();
        let source_path = fixture.dir.path().join("source.txt");
        fs::write(&source_path, b"payload").unwrap();
        let dm = format!("dm:{}", bob());

        fixture
            .store
            .save_outgoing(
                message("m1", &dm),
                vec![bob()],
                vec![source_path.clone()],
            )
            .await
            .unwrap();

        let history = fixture.store.load_history().await.unwrap();
        let stored = history.messages.iter().find(|item| item.id == "m1").unwrap();
        assert_eq!(stored.attachments.len(), 1);
        assert_eq!(stored.attachments[0].filename, "source.txt");
        assert_eq!(
            stored.attachments[0].local_path,
            source_path.canonicalize().unwrap().to_string_lossy()
        );
        assert!(stored.attachments[0].is_sender);

        // Bob must be online before the outbox row is claimed.
        fixture.store.observe_network_event(&NetworkEvent::Presence {
            peer_id: bob(),
            username: "bob".to_string(),
            online: true,
        });

        // The queued network payload should carry the bytes.
        let command = next_command(&mut fixture.network_rx).await;
        let NetworkCommand::SendDirect { request, .. } = command else {
            panic!("wrong command");
        };
        let ChatRequest::Chat(queued) = request else {
            panic!("wrong request");
        };
        assert_eq!(queued.attachments.len(), 1);
        assert_eq!(queued.attachments[0].bytes, b"payload");

        fixture.stop().await;
    }

    #[tokio::test]
    async fn incoming_attachment_is_stored_in_downloads() {
        let fixture = fixture();
        let dm = format!("dm:{}", bob());
        let mut incoming = ChatMessage {
            from_peer: bob().to_string(),
            from_username: "bob".to_string(),
            attachments: vec![Attachment {
                filename: "note.txt".to_string(),
                bytes: b"hello".to_vec(),
                mime_type: None,
            }],
            ..message("m1", "dm:me")
        };
        incoming.created_at = 1_735_689_600_000;

        fixture.store.record_incoming_message(&incoming, &dm, true);
        let history = fixture.store.load_history().await.unwrap();
        let stored = history.messages.iter().find(|item| item.id == "m1").unwrap();
        assert_eq!(stored.attachments.len(), 1);
        assert_eq!(stored.attachments[0].filename, "note.txt");
        assert!(
            stored.attachments[0]
                .local_path
                .contains("downloads/20250101_bob_note.txt"),
            "got {}",
            stored.attachments[0].local_path
        );
        assert!(!stored.attachments[0].is_sender);
        assert!(Path::new(&stored.attachments[0].local_path).exists());
        assert_eq!(
            fs::read(&stored.attachments[0].local_path).unwrap(),
            b"hello"
        );

        fixture.stop().await;
    }

    #[test]
    fn ack_response_shape_is_unchanged() {
        let response = ChatResponse::Ack {
            outbox_id: Some(7),
            ok: true,
            error: None,
        };
        let bytes = serde_json::to_vec(&response).unwrap();
        assert_eq!(serde_json::from_slice::<ChatResponse>(&bytes).unwrap(), response);
    }
}
