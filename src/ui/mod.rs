#[cfg(test)]
mod autocomplete_tests;
#[cfg(test)]
mod unread_tests;

pub mod app;
pub mod cli;
pub mod complete;
pub mod input;
pub mod render;

use std::{error::Error, time::Duration};

use crossterm::event::{poll, read, Event};
use libp2p::PeerId;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::{
    network::{NetworkCommand, NetworkEvent},
    worker::{Store, WorkerEvent},
};

use app::{make_message, App, GENERAL};
use input::{Action, InputResult};

fn handle_network_event(app: &mut App, event: NetworkEvent, store: Option<&Store>) {
    if let Some(store) = store {
        store.observe_network_event(&event);
    }
    match event {
        NetworkEvent::PeerConnected { peer_id, username }
        | NetworkEvent::Presence {
            peer_id,
            username,
            online: true,
        } => app.add_or_update_peer(peer_id, &username, true),
        NetworkEvent::PeerDisconnected { peer_id, username } => {
            if let Some(username) = username {
                app.add_or_update_peer(peer_id, &username, false);
            } else {
                app.remove_peer(&peer_id);
            }
        }
        NetworkEvent::Presence {
            peer_id,
            username,
            online: false,
        } => app.add_or_update_peer(peer_id, &username, false),
        NetworkEvent::MessageReceived { message } => {
            let key = app.record_incoming(message.clone());
            if let (Some(store), Some(key)) = (store, key) {
                store.record_incoming_message(&message, &key, key != app.active_conversation);
            }
        }
        NetworkEvent::GroupInviteReceived {
            from,
            group_id,
            group_name,
            ..
        } => app.handle_group_invite(from, &group_id, &group_name),
        NetworkEvent::MemberAddedReceived {
            from,
            group_id,
            peer_id,
            username,
            ..
        } => {
            app.handle_member_added(from, &group_id, peer_id, &username);
        }
        NetworkEvent::MessageAck {
            message_id,
            peer_id,
            ok: true,
            ..
        } => app.ack(&message_id, peer_id),
        NetworkEvent::MessageAck {
            message_id,
            ok: false,
            error,
            ..
        } => {
            let reason = error.unwrap_or_else(|| "delivery failed".to_string());
            app.system(format!("! message {message_id}: {reason} (still pending)"));
        }
        NetworkEvent::Error { message } => app.system(format!("! {message}")),
        NetworkEvent::PeerExpired { peer_id } => app.set_peer_online(peer_id, false),
        NetworkEvent::PeerDiscovered { .. } | NetworkEvent::DeliveryReport { .. } => {}
    }
}

/// Who receives a message in the active conversation.
pub fn recipients_for(app: &App, channel: &str) -> Vec<PeerId> {
    let mut recipients: Vec<PeerId> = if channel == GENERAL {
        // Keep offline peers too: phase 3 stores their rows in the outbox and flushes on reconnect.
        app.peers.keys().copied().collect()
    } else if let Some(value) = channel.strip_prefix("dm:") {
        match value.parse::<PeerId>() {
            Ok(peer_id) => vec![peer_id],
            Err(_) => Vec::new(),
        }
    } else {
        app.group(channel)
            .map(|group| {
                group
                    .members
                    .iter()
                    .copied()
                    .filter(|peer| *peer != app.me)
                    .collect()
            })
            .unwrap_or_default()
    };
    recipients.sort_by_key(|peer| peer.to_string());
    recipients
}

async fn send_message(
    app: &mut App<'_>,
    me: PeerId,
    username: &str,
    channel: String,
    body: String,
    recipients: Vec<PeerId>,
    store: &Store,
) {
    if recipients.is_empty() {
        app.system("No one to send to yet — wait for a peer to come online.");
        return;
    }
    let message = make_message(&me, username, channel, body);
    let message_id = message.id.clone();
    app.record_outgoing(message.clone(), recipients.clone());
    if let Err(error) = store.save_outgoing(message, recipients).await {
        app.set_message_pending(&message_id, false);
        app.system(format!("! message was not saved: {error}"));
    }
}

pub async fn handle_action(
    app: &mut App<'_>,
    me: PeerId,
    username: &str,
    action: Action,
    store: &Store,
) {
    match action {
        Action::SendChat(body) => {
            let channel = app.active_conversation.clone();
            let recipients = recipients_for(app, &channel);
            send_message(app, me, username, channel, body, recipients, store).await;
        }
        Action::SendDm { user, body } => match app.resolve_user(&user) {
            Ok(peer_id) => {
                let channel = format!("dm:{peer_id}");
                app.activate_conversation(&channel);
                send_message(app, me, username, channel, body, vec![peer_id], store).await;
            }
            Err(error) => app.system(error.to_string()),
        },
        Action::Users => {
            let summary = app.users_summary();
            app.system(summary);
        }
        Action::GroupCreate(name) => {
            let id = app.create_group(&name);
            if let Err(error) = store.create_group(id.clone(), name.clone()).await {
                app.system(format!("! group was not saved: {error}"));
            }
            app.activate_conversation(&id);
            app.system(format!(
                "Created private group '{name}'. Invite with /group add <group> <user>."
            ));
        }
        Action::GroupAdd { group, user } => {
            let index = match app.resolve_group(&group) {
                Ok(index) => index,
                Err(error) => {
                    app.system(error.to_string());
                    return;
                }
            };
            let group_id = app.groups[index].id.clone();
            let group_name = app.groups[index].name.clone();
            if !app.is_group_member(&group_id) {
                app.system("You are not a member of that group.");
                return;
            }
            let invitee = match app.resolve_user(&user) {
                Ok(peer_id) => peer_id,
                Err(error) => {
                    app.system(error.to_string());
                    return;
                }
            };
            if invitee == me {
                app.system("You are already in the group.");
                return;
            }
            if app.groups[index].members.contains(&invitee) {
                app.system(format!("{} is already in '{group_name}'.", app.peer_label(&invitee)));
                return;
            }
            let existing: Vec<PeerId> = app.groups[index]
                .members
                .iter()
                .copied()
                .filter(|peer| *peer != me)
                .collect();
            let label = app.peer_label(&invitee);
            if let Err(error) = store
                .queue_group_invite(
                    group_id.clone(),
                    group_name.clone(),
                    invitee,
                    label.clone(),
                    existing,
                )
                .await
            {
                app.system(format!("! invitation was not saved: {error}"));
                return;
            }
            app.add_group_member(&group_id, invitee);
            app.system(format!("➜ invited {label} to '{group_name}'"));
        }
        Action::GroupMembers(group) => match app.resolve_group(&group) {
            Ok(index) => {
                let id = app.groups[index].id.clone();
                let name = app.groups[index].name.clone();
                let members = app.groups[index]
                    .members
                    .iter()
                    .map(|peer| app.peer_label(peer))
                    .collect::<Vec<_>>();
                app.system(format!("Members of '{name}' ({}): {}", members.len(), members.join(", ")));
                let _ = id;
            }
            Err(error) => app.system(error.to_string()),
        },
        Action::Help(text) => app.system(text),
    }
}

fn apply_worker_event(app: &mut App<'_>, event: WorkerEvent) {
    match event {
        WorkerEvent::PendingChanged {
            message_id,
            pending,
        } => app.set_message_pending(&message_id, pending),
        WorkerEvent::DeliveryFailed { error, .. } => {
            app.system(format!("! delivery failed: {error} (still pending)"));
        }
    }
}

async fn tui_loop(
    terminal: &mut app::TerminalHandle,
    app: &mut App<'_>,
    me: PeerId,
    username: &str,
    store: &Store,
    mut event_rx: Receiver<NetworkEvent>,
    mut worker_events: tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>,
) -> Result<(), Box<dyn Error>> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut last_active = app.active_conversation.clone();
    loop {
        terminal.draw(|frame| render::render(frame, app))?;

        tokio::select! {
            event = event_rx.recv() => match event {
                Some(event) => handle_network_event(app, event, Some(store)),
                None => return Ok(()),
            },
            _ = tick.tick() => {
                while let Ok(event) = worker_events.try_recv() {
                    apply_worker_event(app, event);
                }
                while poll(Duration::ZERO)? {
                    if let Event::Key(key) = read()? {
                        match input::handle_key(app, key) {
                            InputResult::Quit => return Ok(()),
                            InputResult::Redraw => {}
                            InputResult::Action(action) => {
                                handle_action(app, me, username, action, store).await;
                            }
                        }
                    }
                }
            }
        }
        // Opening a conversation (by key, command, or invite) clears its persisted marker.
        if app.active_conversation != last_active {
            store.mark_conversation_read(&app.active_conversation);
            last_active = app.active_conversation.clone();
        }
    }
}

/// Run the full-screen UI until `/quit`, Ctrl+C, or the network task stops.
pub async fn run(
    me: PeerId,
    username: String,
    cmd_tx: Sender<NetworkCommand>,
    event_rx: Receiver<NetworkEvent>,
    store: Store,
    worker_events: tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>,
) -> Result<(), Box<dyn Error>> {
    // Created first so every later error path still restores the terminal.
    let _guard = app::TerminalGuard::new()?;
    let mut terminal = app::terminal()?;
    let mut app = App::new(me, username.clone());
    match store.load_history().await {
        Ok(history) => app.apply_history(history),
        Err(error) => app.system(format!("! could not load saved history: {error}")),
    }
    // The startup conversation is open, so drop any marker left for it by a previous run.
    store.mark_conversation_read(&app.active_conversation);
    app.system("Welcome! /help lists commands. mDNS discovery may take a few seconds.");

    let result = tui_loop(
        &mut terminal,
        &mut app,
        me,
        &username,
        &store,
        event_rx,
        worker_events,
    )
    .await;
    let _ = cmd_tx.send(NetworkCommand::Shutdown).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::protocol::ChatMessage;

    fn fixture() -> App<'static> {
        let me: PeerId = "12D3KooWJ2q5Aj9fMZ43AFFz152XxBRQEDBsUo7hHKi4YpYJ4yyB"
            .parse()
            .unwrap();
        let mut app = App::new(me, "alice".to_string());
        app.add_or_update_peer(PeerId::random(), "bob", true);
        app
    }

    fn message(channel: &str) -> ChatMessage {
        ChatMessage {
            id: "m1".to_string(),
            from_peer: PeerId::random().to_string(),
            from_username: "bob".to_string(),
            channel: channel.to_string(),
            body: "hello".to_string(),
            created_at: 1,
            outbox_id: None,
        }
    }

    #[test]
    fn routes_events_to_app_state() {
        let mut app = fixture();
        let peer = PeerId::random();
        handle_network_event(
            &mut app,
            NetworkEvent::Presence {
                peer_id: peer,
                username: "carol".to_string(),
                online: true,
            },
            None,
        );
        assert!(app.peers[&peer].online);
        handle_network_event(
            &mut app,
            NetworkEvent::MessageReceived {
                message: message(GENERAL),
            },
            None,
        );
        assert_eq!(app.conversations[GENERAL].len(), 1);
        handle_network_event(
            &mut app,
            NetworkEvent::MessageAck {
                message_id: "m1".to_string(),
                peer_id: peer,
                ok: true,
                error: None,
            },
            None,
        );
        assert!(!app.is_pending("m1"));
    }

    #[test]
    fn failed_ack_keeps_message_pending() {
        let mut app = fixture();
        let peer = *app.peers.keys().next().unwrap();
        app.record_outgoing(message(GENERAL), vec![peer]);
        assert!(app.is_pending("m1"));
        handle_network_event(
            &mut app,
            NetworkEvent::MessageAck {
                message_id: "m1".to_string(),
                peer_id: peer,
                ok: false,
                error: Some("offline".to_string()),
            },
            None,
        );
        assert!(app.is_pending("m1"));
    }

    #[test]
    fn recipients_follow_conversation_type() {
        let mut app = fixture();
        let bob = *app.peers.keys().next().unwrap();
        app.add_or_update_peer(bob, "bob", false);
        // Offline peers stay in the outbox instead of being dropped.
        assert_eq!(recipients_for(&app, GENERAL), vec![bob]);

        app.set_peer_online(bob, true);
        assert_eq!(recipients_for(&app, GENERAL), vec![bob]);
        assert_eq!(
            recipients_for(&app, &format!("dm:{bob}")),
            vec![bob]
        );

        let id = app.create_group("team");
        assert_eq!(recipients_for(&app, &id), Vec::new()); // only me so far
        app.add_group_member(&id, bob);
        assert_eq!(recipients_for(&app, &id), vec![bob]);
    }

    #[test]
    fn cli_module_is_wired_in() {
        assert_eq!(super::cli::parse_input("/quit"), super::cli::InputAction::Quit);
    }
}
