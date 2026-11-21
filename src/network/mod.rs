pub mod behaviour;
pub mod events;
pub mod protocol;

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    io,
};

use futures::StreamExt;
use libp2p::{
    gossipsub, identity::Keypair, mdns,
    request_response::{self, OutboundRequestId},
    swarm::{Swarm, SwarmEvent},
    Multiaddr, PeerId,
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};
use tracing::{debug, warn};

use self::{
    behaviour::{presence_topic, ChatBehaviour, ChatBehaviourEvent},
    events::{discovered_peers, short_peer_id, should_initiate_dial},
    protocol::{ChatMessage, ChatRequest, ChatResponse, Presence},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkCommand {
    /// Phase-1 CLI fallback: broadcast to every connected peer.
    SendChat { message: ChatMessage },
    /// Send one request to each explicit recipient (chat, invite, member update).
    SendDirect {
        request: ChatRequest,
        recipients: Vec<PeerId>,
    },
    Shutdown,
}

impl NetworkCommand {
    pub fn is_shutdown(&self) -> bool {
        matches!(self, NetworkCommand::Shutdown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkEvent {
    PeerDiscovered {
        peer_id: PeerId,
        addrs: Vec<Multiaddr>,
    },
    PeerExpired {
        peer_id: PeerId,
    },
    PeerConnected {
        peer_id: PeerId,
        username: String,
    },
    PeerDisconnected {
        peer_id: PeerId,
        username: Option<String>,
    },
    Presence {
        peer_id: PeerId,
        username: String,
        online: bool,
    },
    MessageReceived {
        message: ChatMessage,
    },
    /// Phase-2 ACK correlation, keyed by message id (no database yet).
    MessageAck {
        message_id: String,
        peer_id: PeerId,
        ok: bool,
        error: Option<String>,
    },
    GroupInviteReceived {
        from: PeerId,
        group_id: String,
        group_name: String,
        invited_by: String,
    },
    MemberAddedReceived {
        from: PeerId,
        group_id: String,
        peer_id: PeerId,
        username: String,
        invited_by: String,
    },
    /// Used from phase 3 to update the outbox after a delivery attempt.
    DeliveryReport {
        outbox_id: u64,
        ok: bool,
        error: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRequest {
    /// Phase-3 requests carry their outbox id in the wire payload and ACK.
    Outbox { outbox_id: u64 },
    /// Phase-1/2 fallback: correlate by message id until the peer learns phase 3.
    Legacy { message_id: String, peer: PeerId },
}

pub struct NetworkHandle {
    pub peer_id: PeerId,
    pub task: JoinHandle<()>,
}

/// Build the swarm and spawn the single task that owns and polls it.
pub fn spawn(
    keypair: Keypair,
    local_username: String,
    command_rx: Receiver<NetworkCommand>,
    event_tx: Sender<NetworkEvent>,
) -> Result<NetworkHandle, Box<dyn Error>> {
    let local_peer_id = keypair.public().to_peer_id();
    let behaviour = behaviour::new_behaviour(&keypair).map_err(io::Error::other)?;

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_dns()?
        .with_behaviour(|_| behaviour)?
        .build();

    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    let task = tokio::spawn(run_loop(
        swarm,
        local_peer_id,
        local_username,
        command_rx,
        event_tx,
    ));

    Ok(NetworkHandle {
        peer_id: local_peer_id,
        task,
    })
}

async fn run_loop(
    mut swarm: Swarm<ChatBehaviour>,
    local_peer_id: PeerId,
    local_username: String,
    mut command_rx: Receiver<NetworkCommand>,
    event_tx: Sender<NetworkEvent>,
) {
    let mut dialed = HashSet::new();
    let mut connected = HashSet::new();
    let mut announced = HashSet::new();
    let mut known_usernames: HashMap<PeerId, String> = HashMap::new();
    // Outbound request id -> correlation data for ACKs and delivery reports.
    let mut pending_requests: HashMap<OutboundRequestId, PendingRequest> = HashMap::new();

    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                if command.is_shutdown() {
                    break;
                }
                let recipients = match &command {
                    NetworkCommand::SendChat { .. } => {
                        recipients_for(&command, &connected)
                    }
                    NetworkCommand::SendDirect { recipients, .. } => recipients.clone(),
                    NetworkCommand::Shutdown => Vec::new(),
                };
                if recipients.is_empty() {
                    let _ = event_tx.send(NetworkEvent::Error {
                        message: "no connected peers to send to".to_string(),
                    }).await;
                    continue;
                }
                for peer in recipients {
                    let request = match &command {
                        NetworkCommand::SendChat { message } => ChatRequest::Chat(message.clone()),
                        NetworkCommand::SendDirect { request, .. } => request.clone(),
                        NetworkCommand::Shutdown => unreachable!(),
                    };
                    send_request(&mut swarm, &mut pending_requests, peer, request);
                }
            }
            event = swarm.next() => {
                let Some(event) = event else { break };

                match event {
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns_event)) => match mdns_event {
                        mdns::Event::Discovered(peers) => {
                            for (peer_id, address) in discovered_peers(&peers, &local_peer_id) {
                                let _ = event_tx.send(NetworkEvent::PeerDiscovered {
                                    peer_id,
                                    addrs: vec![address.clone()],
                                }).await;

                                // Avoid two peers dialing each other at the same instant.
                                if !should_initiate_dial(&local_peer_id, &peer_id)
                                    || !dialed.insert(address.clone())
                                {
                                    continue;
                                }
                                if let Err(error) = swarm.dial(address.clone()) {
                                    warn!(%error, address = %address, "mDNS dial failed");
                                    dialed.remove(&address);
                                }
                            }
                        }
                        mdns::Event::Expired(peers) => {
                            for (peer_id, address) in peers {
                                dialed.remove(&address);
                                let _ = event_tx.send(NetworkEvent::PeerExpired { peer_id }).await;
                            }
                        }
                    },
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(_)) => {}
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        message, ..
                    })) => {
                        if message.topic.as_str() != protocol::PRESENCE_TOPIC {
                            continue;
                        }
                        match serde_json::from_slice::<Presence>(&message.data) {
                            Ok(presence) => {
                                if let Ok(peer_id) = presence.peer_id.parse::<PeerId>() {
                                    // Ignore our own broadcasts echoed back by gossipsub.
                                    if peer_id == local_peer_id {
                                        continue;
                                    }
                                    if presence.online {
                                        known_usernames.insert(peer_id, presence.username.clone());
                                    }
                                    let _ = event_tx.send(NetworkEvent::Presence {
                                        peer_id,
                                        username: presence.username,
                                        online: presence.online,
                                    }).await;
                                }
                            }
                            Err(error) => debug!(%error, "ignoring malformed presence message"),
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(_)) => {}
                    SwarmEvent::Behaviour(ChatBehaviourEvent::RequestResponse(
                        request_response::Event::Message { peer, message, .. },
                    )) => match message {
                        request_response::Message::Request { request, channel, .. } => match request {
                            ChatRequest::Hello { username } => {
                                known_usernames.insert(peer, username.clone());
                                if announced.insert(peer) {
                                    let _ = event_tx.send(NetworkEvent::PeerConnected {
                                        peer_id: peer,
                                        username: username.clone(),
                                    }).await;
                                }
                                let _ = swarm.behaviour_mut().request_response.send_response(
                                    channel,
                                    ChatResponse::HelloAck {
                                        username: local_username.clone(),
                                    },
                                );
                            }
                            ChatRequest::Chat(message) => {
                                let outbox_id = message.outbox_id;
                                let message = ChatMessage { outbox_id: None, ..message };
                                let _ = event_tx.send(NetworkEvent::MessageReceived { message }).await;
                                let _ = swarm.behaviour_mut().request_response.send_response(
                                    channel,
                                    ChatResponse::Ack { outbox_id, ok: true, error: None },
                                );
                            }
                            ChatRequest::GroupInvite { group_id, group_name, invited_by, outbox_id } => {
                                let _ = event_tx.send(NetworkEvent::GroupInviteReceived {
                                    from: peer,
                                    group_id,
                                    group_name,
                                    invited_by,
                                }).await;
                                let _ = swarm.behaviour_mut().request_response.send_response(
                                    channel,
                                    ChatResponse::Ack { outbox_id, ok: true, error: None },
                                );
                            }
                            ChatRequest::MemberAdded { group_id, peer_id, username, invited_by, outbox_id } => {
                                match peer_id.parse::<PeerId>() {
                                    Ok(added) => {
                                        let _ = event_tx.send(NetworkEvent::MemberAddedReceived {
                                            from: peer,
                                            group_id,
                                            peer_id: added,
                                            username,
                                            invited_by,
                                        }).await;
                                    }
                                    Err(error) => debug!(%error, "ignoring member_added with bad peer id"),
                                }
                                let _ = swarm.behaviour_mut().request_response.send_response(
                                    channel,
                                    ChatResponse::Ack { outbox_id, ok: true, error: None },
                                );
                            }
                        },
                        request_response::Message::Response { request_id, response, .. } => match response {
                            ChatResponse::HelloAck { username } => {
                                known_usernames.insert(peer, username.clone());
                                if announced.insert(peer) {
                                    let _ = event_tx.send(NetworkEvent::PeerConnected {
                                        peer_id: peer,
                                        username,
                                    }).await;
                                }
                            }
                            ChatResponse::Ack { outbox_id: Some(outbox_id), ok, error } => {
                                pending_requests.remove(&request_id);
                                let _ = event_tx.send(NetworkEvent::DeliveryReport {
                                    outbox_id,
                                    ok,
                                    error,
                                }).await;
                            }
                            ChatResponse::Ack { outbox_id: None, ok, error } => {
                                // A phase-2 peer omits `outbox_id`. We still know which tracked
                                // send this reply belongs to, so keep the row pending/done correctly.
                                match pending_requests.remove(&request_id) {
                                    Some(PendingRequest::Outbox { outbox_id }) => {
                                        let _ = event_tx.send(NetworkEvent::DeliveryReport {
                                            outbox_id,
                                            ok,
                                            error,
                                        }).await;
                                    }
                                    Some(PendingRequest::Legacy { message_id, peer }) => {
                                        debug!(%message_id, peer = %short_peer_id(&peer), ok, "chat ack");
                                        let _ = event_tx.send(NetworkEvent::MessageAck {
                                            message_id,
                                            peer_id: peer,
                                            ok,
                                            error,
                                        }).await;
                                    }
                                    None => {}
                                }
                            }
                        },
                    },
                    SwarmEvent::Behaviour(ChatBehaviourEvent::RequestResponse(
                        request_response::Event::OutboundFailure { peer, request_id, error, .. },
                    )) => {
                        match pending_requests.remove(&request_id) {
                            Some(PendingRequest::Outbox { outbox_id, .. }) => {
                                let _ = event_tx.send(NetworkEvent::DeliveryReport {
                                    outbox_id,
                                    ok: false,
                                    error: Some(error.to_string()),
                                }).await;
                            }
                            Some(PendingRequest::Legacy { message_id, peer }) => {
                                let _ = event_tx.send(NetworkEvent::MessageAck {
                                    message_id,
                                    peer_id: peer,
                                    ok: false,
                                    error: Some(error.to_string()),
                                }).await;
                            }
                            None => {}
                        }
                        let _ = event_tx.send(NetworkEvent::Error {
                            message: format!("request to {} failed: {error}", short_peer_id(&peer)),
                        }).await;
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::RequestResponse(
                        request_response::Event::InboundFailure { peer, error, .. },
                    )) => {
                        let _ = event_tx.send(NetworkEvent::Error {
                            message: format!("request from {} failed: {error:?}", short_peer_id(&peer)),
                        }).await;
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::RequestResponse(
                        request_response::Event::ResponseSent { .. },
                    )) => {}
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        connected.insert(peer_id);
                        swarm.behaviour_mut().request_response.send_request(
                            &peer_id,
                            ChatRequest::Hello {
                                username: local_username.clone(),
                            },
                        );
                        publish_presence(&mut swarm, local_peer_id, &local_username, true);
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        ..
                    } => {
                        if num_established == 0 {
                            connected.remove(&peer_id);
                            announced.remove(&peer_id);
                            publish_presence(&mut swarm, local_peer_id, &local_username, false);
                            let _ = event_tx.send(NetworkEvent::PeerDisconnected {
                                peer_id,
                                username: known_usernames.remove(&peer_id),
                            }).await;
                        }
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        // Allow a retry when mDNS reports the address again.
                        dialed.clear();
                        let who = peer_id
                            .map(|peer| short_peer_id(&peer))
                            .unwrap_or_else(|| "unknown".to_string());
                        let _ = event_tx.send(NetworkEvent::Error {
                            message: format!("could not connect to {who}: {error:?}"),
                        }).await;
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        debug!(%address, "listening for libp2p connections");
                    }
                    _ => {}
                }
            }
        }
    }

    debug!("network task stopped");
}

/// Decide how an outbound request's ACK will be correlated.
fn pending_for_request(request: &ChatRequest, peer: PeerId) -> Option<PendingRequest> {
    match request {
        ChatRequest::Chat(message) => Some(match message.outbox_id {
            Some(outbox_id) => PendingRequest::Outbox { outbox_id },
            None => PendingRequest::Legacy {
                message_id: message.id.clone(),
                peer,
            },
        }),
        ChatRequest::GroupInvite { outbox_id: Some(outbox_id), .. }
        | ChatRequest::MemberAdded { outbox_id: Some(outbox_id), .. } => {
            Some(PendingRequest::Outbox {
                outbox_id: *outbox_id,
            })
        }
        _ => None,
    }
}

/// Send one request and remember how to correlate its ACK.
fn send_request(
    swarm: &mut Swarm<ChatBehaviour>,
    pending_requests: &mut HashMap<OutboundRequestId, PendingRequest>,
    peer: PeerId,
    request: ChatRequest,
) {
    let pending = pending_for_request(&request, peer);
    let request_id = swarm
        .behaviour_mut()
        .request_response
        .send_request(&peer, request);
    if let Some(pending) = pending {
        pending_requests.insert(request_id, pending);
    }
}

fn publish_presence(
    swarm: &mut Swarm<ChatBehaviour>,
    local_peer_id: PeerId,
    username: &str,
    online: bool,
) {
    let payload = match serde_json::to_vec(&Presence {
        peer_id: local_peer_id.to_string(),
        username: username.to_string(),
        online,
    }) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(%error, "could not serialize presence");
            return;
        }
    };

    if let Err(error) = swarm
        .behaviour_mut()
        .gossipsub
        .publish(presence_topic(), payload)
    {
        // No peers to publish to yet is normal on startup.
        debug!(%error, "presence announcement skipped");
    }
}

/// Deterministic recipient list: phase 1 broadcasts, phase 2 uses explicit recipients.
fn recipients_for(command: &NetworkCommand, connected: &HashSet<PeerId>) -> Vec<PeerId> {
    match command {
        NetworkCommand::SendChat { .. } => {
            let mut peers: Vec<PeerId> = connected.iter().copied().collect();
            peers.sort_by_key(|peer| peer.to_string());
            peers
        }
        NetworkCommand::SendDirect { recipients, .. } => {
            // Keep offline recipients too: the resulting failure becomes a MessageAck so the
            // UI can keep the line marked `(Pending)` until phase 3 retries it.
            let mut peers = recipients.clone();
            peers.sort_by_key(|peer| peer.to_string());
            peers
        }
        NetworkCommand::Shutdown => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> ChatMessage {
        ChatMessage {
            id: "m-1".to_string(),
            from_peer: "self".to_string(),
            from_username: "me".to_string(),
            channel: "#general".to_string(),
            body: "hi".to_string(),
            created_at: 1,
            outbox_id: None,
        }
    }

    #[test]
    fn broadcast_targets_all_connected_peers() {
        let a = PeerId::random();
        let b = PeerId::random();
        let connected = HashSet::from([a, b]);
        let recipients = recipients_for(&NetworkCommand::SendChat { message: message() }, &connected);
        assert_eq!(recipients.len(), 2);
        assert!(recipients.contains(&a));
        assert!(recipients.contains(&b));
    }

    #[test]
    fn send_direct_uses_explicit_recipients_even_when_offline() {
        let a = PeerId::random();
        let offline = PeerId::random();
        let connected = HashSet::from([a]);
        let command = NetworkCommand::SendDirect {
            request: ChatRequest::Chat(message()),
            recipients: vec![a, offline],
        };
        let mut expected = vec![a, offline];
        expected.sort_by_key(|peer| peer.to_string());
        assert_eq!(recipients_for(&command, &connected), expected);
    }

    #[test]
    fn group_events_carry_the_sender_and_payload() {
        let from = PeerId::random();
        let added = PeerId::random();
        let invite = NetworkEvent::GroupInviteReceived {
            from,
            group_id: "g1".to_string(),
            group_name: "team".to_string(),
            invited_by: from.to_string(),
        };
        match invite {
            NetworkEvent::GroupInviteReceived { group_name, .. } => {
                assert_eq!(group_name, "team")
            }
            _ => panic!("wrong event"),
        }
        let member = NetworkEvent::MemberAddedReceived {
            from,
            group_id: "g1".to_string(),
            peer_id: added,
            username: "bob".to_string(),
            invited_by: from.to_string(),
        };
        match member {
            NetworkEvent::MemberAddedReceived { peer_id, username, .. } => {
                assert_eq!(peer_id, added);
                assert_eq!(username, "bob");
            }
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn outbox_requests_correlate_by_outbox_id() {
        let peer = PeerId::random();
        let tracked = ChatRequest::Chat(ChatMessage {
            outbox_id: Some(42),
            ..message()
        });
        assert_eq!(
            pending_for_request(&tracked, peer),
            Some(PendingRequest::Outbox { outbox_id: 42 })
        );

        let invite = ChatRequest::GroupInvite {
            group_id: "g1".to_string(),
            group_name: "team".to_string(),
            invited_by: "me".to_string(),
            outbox_id: Some(7),
        };
        assert_eq!(
            pending_for_request(&invite, peer),
            Some(PendingRequest::Outbox { outbox_id: 7 })
        );
    }

    #[test]
    fn legacy_chat_requests_correlate_by_message_id() {
        let peer = PeerId::random();
        assert_eq!(
            pending_for_request(&ChatRequest::Chat(message()), peer),
            Some(PendingRequest::Legacy {
                message_id: "m-1".to_string(),
                peer,
            })
        );
    }

    #[test]
    fn shutdown_has_no_recipients() {
        let connected = HashSet::from([PeerId::random()]);
        assert!(recipients_for(&NetworkCommand::Shutdown, &connected).is_empty());
        assert!(NetworkCommand::Shutdown.is_shutdown());
        assert!(!NetworkCommand::SendChat { message: message() }.is_shutdown());
    }
}
