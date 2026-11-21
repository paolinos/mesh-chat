use std::{
    collections::{HashMap, HashSet},
    error::Error,
    io::{self, BufRead, ErrorKind, Write},
    time::{SystemTime, UNIX_EPOCH},
};

use libp2p::PeerId;
use tokio::sync::mpsc::{self, Receiver, Sender};
use uuid::Uuid;

use crate::network::{
    events::{display_name, short_peer_id},
    protocol::ChatMessage,
    NetworkCommand, NetworkEvent,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Ignore,
    Quit,
    Users,
    Chat(String),
}

/// Usernames are display labels; keep them short and single-line.
pub fn validate_username(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("username cannot be empty");
    }
    if name.len() > 32 {
        return Err("username must be at most 32 characters");
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("username cannot contain control characters");
    }
    Ok(name.to_string())
}

pub fn parse_input(line: &str) -> InputAction {
    let trimmed = line.trim();
    match trimmed {
        "" => InputAction::Ignore,
        "/quit" | "/exit" => InputAction::Quit,
        "/users" => InputAction::Users,
        text if text.starts_with('/') => InputAction::Ignore,
        text => InputAction::Chat(text.to_string()),
    }
}

pub fn prompt_username() -> io::Result<String> {
    let stdin = io::stdin();
    loop {
        print!("Username: ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "no username provided",
            ));
        }
        match validate_username(&line) {
            Ok(name) => return Ok(name),
            Err(message) => println!("Invalid username: {message}"),
        }
    }
}

pub fn make_chat_message(local_peer_id: &PeerId, username: &str, body: String) -> ChatMessage {
    ChatMessage {
        id: Uuid::new_v4().to_string(),
        from_peer: local_peer_id.to_string(),
        from_username: username.to_string(),
        channel: "#general".to_string(),
        body,
        created_at: now_millis(),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Turn a network event into the line printed by the phase-1 CLI.
pub fn format_event(event: &NetworkEvent) -> Option<String> {
    match event {
        NetworkEvent::PeerConnected { peer_id, username } => Some(format!(
            "➜ {} joined the network",
            display_name(username, peer_id)
        )),
        NetworkEvent::PeerDisconnected { peer_id, username } => match username {
            Some(username) => Some(format!(
                "➜ {} left the network",
                display_name(username, peer_id)
            )),
            None => Some(format!(
                "➜ {} left the network",
                short_peer_id(peer_id)
            )),
        },
        NetworkEvent::Presence {
            peer_id,
            username,
            online,
        } => Some(format!(
            "➜ {} is {}",
            display_name(username, peer_id),
            if *online { "online" } else { "offline" }
        )),
        NetworkEvent::MessageReceived { message } => {
            let label = match message.from_peer.parse::<PeerId>() {
                Ok(peer_id) => display_name(&message.from_username, &peer_id),
                Err(_) => message.from_username.clone(),
            };
            Some(format!("<{label}> {}", message.body))
        }
        NetworkEvent::Error { message } => Some(format!("! {message}")),
        NetworkEvent::PeerDiscovered { .. }
        | NetworkEvent::PeerExpired { .. }
        | NetworkEvent::DeliveryReport { .. } => None,
    }
}

pub fn update_peer_list(peers: &mut HashMap<PeerId, String>, event: &NetworkEvent) {
    match event {
        NetworkEvent::PeerConnected { peer_id, username }
        | NetworkEvent::Presence {
            peer_id,
            username,
            online: true,
        } => {
            peers.insert(*peer_id, username.clone());
        }
        NetworkEvent::PeerDisconnected { peer_id, .. }
        | NetworkEvent::Presence {
            peer_id,
            online: false,
            ..
        }
        | NetworkEvent::PeerExpired { peer_id } => {
            peers.remove(peer_id);
        }
        _ => {}
    }
}

fn print_users(peers: &HashMap<PeerId, String>) {
    if peers.is_empty() {
        println!("No peers discovered yet. On one laptop, make sure each terminal uses its own --data-dir (e.g. data/alice and data/bob); on different machines, check that mDNS/UDP 5353 is allowed on the LAN.");
        return;
    }
    println!("Users on this LAN:");
    let mut entries: Vec<_> = peers.iter().collect();
    entries.sort_by_key(|(peer_id, username)| (username.to_string(), peer_id.to_string()));
    for (peer_id, username) in entries {
        println!("  {}", display_name(username, peer_id));
    }
}

/// Plain-terminal UI: stdin is read on its own thread while network events are routed here.
pub async fn run(
    local_peer_id: PeerId,
    local_username: String,
    cmd_tx: Sender<NetworkCommand>,
    mut event_rx: Receiver<NetworkEvent>,
) -> Result<(), Box<dyn Error>> {
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if input_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut peers = HashMap::new();
    let mut reported_online = HashSet::new();

    println!("Connected as {} ({})", local_username, short_peer_id(&local_peer_id));
    println!("Type a message and press Enter. /users lists peers, /quit exits.");
    println!("Looking for peers on the local network with mDNS...\n");

    loop {
        tokio::select! {
            line = input_rx.recv() => {
                let Some(line) = line else { break };
                match parse_input(&line) {
                    InputAction::Ignore => {}
                    InputAction::Quit => break,
                    InputAction::Users => print_users(&peers),
                    InputAction::Chat(body) => {
                        let message = make_chat_message(&local_peer_id, &local_username, body);
                        if cmd_tx.send(NetworkCommand::SendChat { message }).await.is_err() {
                            break;
                        }
                    }
                }
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                update_peer_list(&mut peers, &event);

                let mut skip_print = false;
                if let NetworkEvent::Presence { peer_id, online: true, .. } = &event {
                    if !reported_online.insert(*peer_id) {
                        skip_print = true;
                    }
                }
                if let NetworkEvent::PeerConnected { peer_id, .. } = &event {
                    reported_online.insert(*peer_id);
                }
                if let NetworkEvent::PeerDisconnected { peer_id, .. }
                | NetworkEvent::PeerExpired { peer_id } = &event {
                    reported_online.remove(peer_id);
                }

                if !skip_print {
                    if let Some(text) = format_event(&event) {
                        println!("{text}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nBye!");
                break;
            }
        }
    }

    let _ = cmd_tx.send(NetworkCommand::Shutdown).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_usernames() {
        assert_eq!(validate_username("  alice \n").unwrap(), "alice");
        assert!(validate_username("   ").is_err());
        assert!(validate_username(&"a".repeat(33)).is_err());
        assert!(validate_username("bad\nname").is_err());
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_input("   "), InputAction::Ignore);
        assert_eq!(parse_input("/quit"), InputAction::Quit);
        assert_eq!(parse_input("/users"), InputAction::Users);
        assert_eq!(parse_input("/unknown"), InputAction::Ignore);
        assert_eq!(parse_input(" hello world "), InputAction::Chat("hello world".to_string()));
    }

    #[test]
    fn formats_events() {
        let peer = PeerId::random();
        let connected = NetworkEvent::PeerConnected {
            peer_id: peer,
            username: "alice".to_string(),
        };
        let text = format_event(&connected).unwrap();
        assert!(text.contains("alice·"));
        assert!(text.contains("joined"));

        let message = ChatMessage {
            id: "m-1".to_string(),
            from_peer: peer.to_string(),
            from_username: "alice".to_string(),
            channel: "#general".to_string(),
            body: "hi there".to_string(),
            created_at: 1,
        };
        let text = format_event(&NetworkEvent::MessageReceived { message }).unwrap();
        assert!(text.contains("hi there"));
        assert!(text.starts_with("<alice·"));
    }

    #[test]
    fn tracks_peer_list() {
        let mut peers = HashMap::new();
        let peer = PeerId::random();
        update_peer_list(
            &mut peers,
            &NetworkEvent::PeerConnected {
                peer_id: peer,
                username: "bob".to_string(),
            },
        );
        assert_eq!(peers.get(&peer).unwrap(), "bob");
        update_peer_list(
            &mut peers,
            &NetworkEvent::PeerDisconnected {
                peer_id: peer,
                username: None,
            },
        );
        assert!(peers.is_empty());
    }

    #[test]
    fn outgoing_message_has_identity_and_timestamp() {
        let peer = PeerId::random();
        let message = make_chat_message(&peer, "me", "hello".to_string());
        assert!(!message.id.is_empty());
        assert_eq!(message.from_peer, peer.to_string());
        assert_eq!(message.from_username, "me");
        assert_eq!(message.channel, "#general");
        assert!(message.created_at > 0);
    }
}
