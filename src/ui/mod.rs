pub mod app;
pub mod cli;
pub mod input;
pub mod render;

use std::{error::Error, time::Duration};

use crossterm::event::{poll, read, Event};
use libp2p::PeerId;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::network::{
    protocol::ChatRequest,
    NetworkCommand, NetworkEvent,
};

use app::{make_message, App, GENERAL};
use input::{Action, InputResult};

fn handle_network_event(app: &mut App, event: NetworkEvent) {
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
            app.record_incoming(message);
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
        app.peers
            .iter()
            .filter(|(_, peer)| peer.online)
            .map(|(peer_id, _)| *peer_id)
            .collect()
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
    cmd_tx: &Sender<NetworkCommand>,
) {
    if recipients.is_empty() {
        app.system("No one to send to yet — wait for a peer to come online.");
        return;
    }
    let message = make_message(&me, username, channel, body);
    app.record_outgoing(message.clone(), recipients.clone());
    let _ = cmd_tx
        .send(NetworkCommand::SendDirect {
            request: ChatRequest::Chat(message),
            recipients,
        })
        .await;
}

pub async fn handle_action(
    app: &mut App<'_>,
    me: PeerId,
    username: &str,
    action: Action,
    cmd_tx: &Sender<NetworkCommand>,
) {
    match action {
        Action::SendChat(body) => {
            let channel = app.active_conversation.clone();
            let recipients = recipients_for(app, &channel);
            send_message(app, me, username, channel, body, recipients, cmd_tx).await;
        }
        Action::SendDm { user, body } => match app.resolve_user(&user) {
            Ok(peer_id) => {
                let channel = format!("dm:{peer_id}");
                app.activate_conversation(&channel);
                send_message(app, me, username, channel, body, vec![peer_id], cmd_tx).await;
            }
            Err(error) => app.system(error.to_string()),
        },
        Action::Users => {
            let summary = app.users_summary();
            app.system(summary);
        }
        Action::GroupCreate(name) => {
            let id = app.create_group(&name);
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
            if !app.is_group_member(&app.groups[index].id) {
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
            let group_id = app.groups[index].id.clone();
            let group_name = app.groups[index].name.clone();
            let existing: Vec<PeerId> = app.groups[index]
                .members
                .iter()
                .copied()
                .filter(|peer| *peer != me)
                .collect();
            app.add_group_member(&group_id, invitee);
            app.system(format!(
                "➜ invited {} to '{group_name}'",
                app.peer_label(&invitee)
            ));

            let _ = cmd_tx
                .send(NetworkCommand::SendDirect {
                    request: ChatRequest::GroupInvite {
                        group_id: group_id.clone(),
                        group_name: group_name.clone(),
                        invited_by: me.to_string(),
                    },
                    recipients: vec![invitee],
                })
                .await;
            if !existing.is_empty() {
                let _ = cmd_tx
                    .send(NetworkCommand::SendDirect {
                        request: ChatRequest::MemberAdded {
                            group_id,
                            peer_id: invitee.to_string(),
                            username: app.peer_label(&invitee),
                            invited_by: me.to_string(),
                        },
                        recipients: existing,
                    })
                    .await;
            }
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

async fn tui_loop(
    terminal: &mut app::TerminalHandle,
    app: &mut App<'_>,
    me: PeerId,
    username: &str,
    cmd_tx: &Sender<NetworkCommand>,
    mut event_rx: Receiver<NetworkEvent>,
) -> Result<(), Box<dyn Error>> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    loop {
        terminal.draw(|frame| render::render(frame, app))?;

        tokio::select! {
            event = event_rx.recv() => match event {
                Some(event) => handle_network_event(app, event),
                None => return Ok(()),
            },
            _ = tick.tick() => {
                while poll(Duration::ZERO)? {
                    if let Event::Key(key) = read()? {
                        match input::handle_key(app, key) {
                            InputResult::Quit => return Ok(()),
                            InputResult::Redraw => {}
                            InputResult::Action(action) => {
                                handle_action(app, me, username, action, cmd_tx).await;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Run the full-screen UI until `/quit`, Ctrl+C, or the network task stops.
pub async fn run(
    me: PeerId,
    username: String,
    cmd_tx: Sender<NetworkCommand>,
    event_rx: Receiver<NetworkEvent>,
) -> Result<(), Box<dyn Error>> {
    // Created first so every later error path still restores the terminal.
    let _guard = app::TerminalGuard::new()?;
    let mut terminal = app::terminal()?;
    let mut app = App::new(me, username.clone());
    app.system("Welcome! /help lists commands. mDNS discovery may take a few seconds.");

    let result = tui_loop(
        &mut terminal,
        &mut app,
        me,
        &username,
        &cmd_tx,
        event_rx,
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
        );
        assert!(app.peers[&peer].online);
        handle_network_event(
            &mut app,
            NetworkEvent::MessageReceived {
                message: message(GENERAL),
            },
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
        );
        assert!(app.is_pending("m1"));
    }

    #[test]
    fn recipients_follow_conversation_type() {
        let mut app = fixture();
        let bob = *app.peers.keys().next().unwrap();
        app.add_or_update_peer(bob, "bob", false);
        assert!(recipients_for(&app, GENERAL).is_empty());

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
